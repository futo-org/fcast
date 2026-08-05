//! ftestsink against real pipelines, plus the synthetic-log path the fuzz mode uses.

use std::time::Duration;

use fcasttest::{
    registry,
    sink::{self, FTestSink, RecordEntry, Recording, asserts, event_name},
    spec::MediaSpec,
};
use gst::prelude::*;

const TIMEOUT: Duration = Duration::from_secs(10);

fn make_sink(recording_key: Option<&str>) -> (gst::Element, Recording) {
    fcasttest::register_for_tests();
    // sync=false keeps the smoke tests off the clock, the property itself stays the
    // BaseSink one.
    let mut builder = gst::ElementFactory::make(sink::FACTORY_NAME).property("sync", false);
    if let Some(key) = recording_key {
        builder = builder.property("recording-key", key);
    }
    let element = builder.build().expect("ftestsink is registered");
    let recording = element
        .clone()
        .downcast::<FTestSink>()
        .expect("ftestsink downcasts to its own type")
        .recording();
    (element, recording)
}

fn videotestsrc_pipeline(sink: &gst::Element, num_buffers: i32) -> gst::Pipeline {
    let src = gst::ElementFactory::make("videotestsrc")
        .property("num-buffers", num_buffers)
        .build()
        .expect("videotestsrc");
    let pipeline = gst::Pipeline::new();
    pipeline.add_many([&src, sink]).unwrap();
    src.link(sink).unwrap();
    pipeline
}

fn wait_for_eos(pipeline: &gst::Pipeline) {
    let bus = pipeline.bus().unwrap();
    let message = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(TIMEOUT.as_secs()),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("eos or error before the timeout");
    if let gst::MessageView::Error(err) = message.view() {
        panic!("pipeline error: {err:?}");
    }
}

#[test]
fn records_ten_buffers_in_a_legal_sequence() {
    let (sink, recording) = make_sink(None);
    let pipeline = videotestsrc_pipeline(&sink, 10);

    pipeline.set_state(gst::State::Playing).unwrap();
    wait_for_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();

    let log = recording.snapshot();
    asserts::all(&log).expect("legal sink sequence");

    assert_eq!(recording.buffer_count(), 10, "log: {log:#?}");
    assert_eq!(recording.event_count(event_name::EOS), 1, "log: {log:#?}");
    assert_eq!(recording.event_count(event_name::STREAM_START), 1);
    assert_eq!(recording.event_count(event_name::CAPS), 1);
    assert_eq!(recording.event_count(event_name::SEGMENT), 1);

    // The first buffer prerolls, and the same buffer reaches render afterwards.
    let prerolls = log
        .iter()
        .filter(|entry| matches!(entry, RecordEntry::Preroll { .. }))
        .count();
    assert_eq!(prerolls, 1, "log: {log:#?}");
    let first_data = log.iter().position(|entry| entry.is_data()).unwrap();
    assert!(
        matches!(log[first_data], RecordEntry::Preroll { .. }),
        "log: {log:#?}"
    );

    // Every state transition of a NULL -> PLAYING -> NULL run, in order.
    let transitions: Vec<gst::StateChange> = log
        .iter()
        .filter_map(|entry| match entry {
            RecordEntry::StateChange { transition, .. } => Some(*transition),
            _ => None,
        })
        .collect();
    assert_eq!(
        transitions,
        vec![
            gst::StateChange::NullToReady,
            gst::StateChange::ReadyToPaused,
            gst::StateChange::PausedToPlaying,
            gst::StateChange::PlayingToPaused,
            gst::StateChange::PausedToReady,
            gst::StateChange::ReadyToNull,
        ],
        "log: {log:#?}"
    );

    // Sticky flags and detail strings.
    let caps_detail = log
        .iter()
        .find_map(|entry| match entry {
            RecordEntry::Event {
                type_name,
                sticky,
                details,
                ..
            } if *type_name == event_name::CAPS => {
                assert!(*sticky);
                details.clone()
            }
            _ => None,
        })
        .expect("caps details");
    assert!(caps_detail.starts_with("video/x-raw"), "{caps_detail}");
    assert!(recording.check_invariants().is_ok());
}

#[test]
fn records_a_real_flush_pair_around_a_seek() {
    let (sink, recording) = make_sink(None);
    // Unbounded source: the run ends with the seek, not with EOS.
    let pipeline = videotestsrc_pipeline(&sink, -1);

    pipeline.set_state(gst::State::Playing).unwrap();
    assert!(recording.wait_for_buffers(5, TIMEOUT), "no buffers arrived");
    pipeline
        .seek_simple(
            gst::SeekFlags::FLUSH | gst::SeekFlags::KEY_UNIT,
            gst::ClockTime::ZERO,
        )
        .expect("flushing seek");
    assert!(
        recording.wait_for_event(event_name::FLUSH_STOP, TIMEOUT),
        "no flush-stop recorded: {:#?}",
        recording.snapshot()
    );
    pipeline.set_state(gst::State::Null).unwrap();

    let log = recording.snapshot();
    assert!(
        log.iter()
            .any(|entry| entry.is_event(event_name::FLUSH_START)),
        "log: {log:#?}"
    );
    asserts::all(&log).expect("legal sink sequence across a flush");
}

/// The BaseSink sync property is untouched, so a clocked run still records
/// everything, just paced by the pipeline clock.
#[test]
fn clocked_run_records_every_buffer() {
    fcasttest::register_for_tests();
    let sink = gst::ElementFactory::make(sink::FACTORY_NAME)
        .build()
        .expect("ftestsink is registered");
    assert!(
        sink.property::<bool>("sync"),
        "sync must keep the BaseSink default"
    );
    let recording = sink
        .clone()
        .downcast::<FTestSink>()
        .expect("ftestsink downcasts to its own type")
        .recording();

    let pipeline = videotestsrc_pipeline(&sink, 3);
    pipeline.set_state(gst::State::Playing).unwrap();
    wait_for_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();

    assert_eq!(recording.buffer_count(), 3);
    asserts::all(&recording.snapshot()).expect("legal sink sequence");
}

#[test]
fn recording_key_stashes_the_handle_in_the_scenario() {
    fcasttest::register_for_tests();
    let scenario = registry::register_scenario("sinkkey", MediaSpec::new(1));

    let (sink, direct) = make_sink(Some("sinkkey"));
    let stashed = scenario
        .handle::<Recording>(sink::DEFAULT_HANDLE_NAME)
        .expect("handle stashed on property set");
    assert!(
        sink::stashed_recording("sinkkey").is_some(),
        "registry lookup helper"
    );

    let pipeline = videotestsrc_pipeline(&sink, 4);
    pipeline.set_state(gst::State::Playing).unwrap();
    wait_for_eos(&pipeline);
    pipeline.set_state(gst::State::Null).unwrap();

    // Same log through both routes.
    assert!(stashed.wait_for_event(event_name::EOS, TIMEOUT));
    assert_eq!(stashed.buffer_count(), 4);
    assert_eq!(direct.len(), stashed.len());
    asserts::all(&stashed.snapshot()).expect("legal sink sequence");

    registry::unregister("sinkkey");
}

#[test]
fn recording_key_slot_names_the_handle() {
    fcasttest::register_for_tests();
    let scenario = registry::register_scenario("sinkslot", MediaSpec::new(2));

    let (_video, _) = make_sink(Some("sinkslot/video"));
    let (_audio, _) = make_sink(Some("sinkslot/audio"));

    assert!(scenario.handle::<Recording>("video").is_some());
    assert!(scenario.handle::<Recording>("audio").is_some());
    assert!(
        scenario
            .handle::<Recording>(sink::DEFAULT_HANDLE_NAME)
            .is_none(),
        "an explicit slot must not claim the default name"
    );

    registry::unregister("sinkslot");
}

/// The invariant checkers run on synthetic logs too, which is how the fuzz mode
/// shrinks a failure without replaying a pipeline.
#[test]
fn flush_injection_on_a_synthetic_log_is_caught() {
    gst::init().unwrap();
    let mut log = vec![
        RecordEntry::synthetic_event(gst::EventType::StreamStart),
        RecordEntry::synthetic_event(gst::EventType::Caps),
        RecordEntry::synthetic_event(gst::EventType::Segment),
        RecordEntry::synthetic_buffer(gst::ClockTime::ZERO),
        RecordEntry::synthetic_event(gst::EventType::FlushStart),
        RecordEntry::synthetic_event(gst::EventType::FlushStop),
        RecordEntry::synthetic_event(gst::EventType::Segment),
        RecordEntry::synthetic_buffer(gst::ClockTime::ZERO),
    ];
    asserts::all(&log).expect("matched flush pair");

    // Inject a second flush-start whose flush-stop never arrives.
    log.insert(6, RecordEntry::synthetic_event(gst::EventType::FlushStart));
    let err = asserts::flush_pairs_matched(&log).expect_err("unmatched flush-start");
    assert!(err.contains("entry 6"), "{err}");
    assert!(err.contains("never matched"), "{err}");
    assert_eq!(asserts::all(&log).err(), Some(err));
}
