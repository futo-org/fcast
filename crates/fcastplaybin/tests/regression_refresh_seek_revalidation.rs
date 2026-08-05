//! `Job::RefreshSeek` re-checks its preconditions AT EXECUTION TIME.
//!
//! The refresh is the flushing seek a track switch schedules so a freshly
//! selected sparse subtitle re-emits its current cue. `SelectionEngine::pump`
//! samples the gates when it schedules one (the caller's quiet, seekable, and
//! NO external subtitle input attached, `selection.rs`), and the job then sits
//! in the worker queue behind SetState/Load/DrainTextWork and the text branch
//! disposals. Everything it was gated on can change in that interval, and the
//! engine's own suppression cannot recall a job it already queued.
//!
//! Why that matters: a flushing seek that lands at the wrong moment is the
//! FLUSHING-return hazard. On an adaptive main input one non-OK push pauses
//! adaptivedemux2's single output task for good, with two DEBUG lines and no
//! error posted anywhere (FREEZE-DIAGN.md sections 1 and 8.2 #2, and the
//! harness's own measured DASH freeze from this seek fired off-moment,
//! `dash_testbed.rs`). `Job::Seek` has always had such a guard; this one had
//! none.
//!
//! The deterministic invalidation used here is an external subtitle input
//! attached after the refresh was queued: that is exactly the condition the
//! engine refuses to schedule against ("any flush races the external inputs'
//! reconfiguration and can freeze the play item"), and the one an already
//! queued job cannot be recalled by.
//!
//! # Verification
//!
//! * Green: no env vars. The stale refresh is dropped and reported.
//! * RED with `FCAST_NO_REFRESH_SEEK_REVALIDATION=1`: the job performs the seek
//!   regardless, so `RefreshSeekFailed` never arrives (the wait times out) and
//!   the seek shows up in the sink's log.

use std::{
    io::Write,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fcastplaybin::{AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, Sinks, StartPoint};
use gst::prelude::*;

const BOUND: Duration = Duration::from_secs(20);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        gst::init().unwrap();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// A real MP3 so the item runs through the real
/// urisourcebin/parsebin/decodebin3 topology AND answers the seeking query,
/// which the gate under test consults (`regression_lib.rs`'s recipe).
fn make_mp3_file(path: &std::path::Path, seconds: f64) {
    let num_buffers = (seconds * 44100.0 / 1024.0).round() as i32;
    let src = gst::ElementFactory::make("audiotestsrc")
        .property("num-buffers", num_buffers)
        .property("is-live", false)
        .property_from_str("wave", "silence")
        .build()
        .unwrap();
    let conv = gst::ElementFactory::make("audioconvert").build().unwrap();
    let enc = gst::ElementFactory::make("lamemp3enc").build().unwrap();
    let sink = gst::ElementFactory::make("filesink")
        .property("location", path.to_str().unwrap())
        .build()
        .unwrap();
    let pipeline = gst::Pipeline::new();
    pipeline.add_many([&src, &conv, &enc, &sink]).unwrap();
    gst::Element::link_many([&src, &conv, &enc, &sink]).unwrap();
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    while let Some(msg) = bus.timed_pop(gst::ClockTime::from_seconds(10)) {
        match msg.view() {
            gst::MessageView::Eos(_) => break,
            gst::MessageView::Error(err) => panic!("mp3 encode failed: {err:?}"),
            _ => {}
        }
    }
    pipeline.set_state(gst::State::Null).unwrap();
}

fn temp_file(tag: &str, ext: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "fcastplaybin-refresh-{}-{tag}.{ext}",
        std::process::id()
    ))
}

type SeekLog = Arc<Mutex<Vec<gst::Seqnum>>>;

/// The per-load audio sink records every SEEK event pushed upstream through it.
/// A pipeline seek reaches its sinks first, so this sees the crate's own seek
/// exactly as sent (`regression_lib.rs`).
fn recording_sinks(log: &SeekLog) -> Sinks {
    let log = log.clone();
    Sinks {
        video: None,
        audio: AudioSink::Factory(Box::new(move || {
            let sink = gst::ElementFactory::make("fakesink")
                .property("sync", true)
                .build()?;
            let pad = sink.static_pad("sink").expect("fakesink has a sink pad");
            let log = log.clone();
            pad.add_probe(gst::PadProbeType::EVENT_UPSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && matches!(event.view(), gst::EventView::Seek(_))
                {
                    log.lock().unwrap().push(event.seqnum());
                }
                gst::PadProbeReturn::Ok
            });
            Ok(sink)
        })),
    }
}

#[test]
fn a_refresh_seek_whose_preconditions_lapsed_is_dropped_not_performed() {
    init();

    let media = temp_file("item", "mp3");
    make_mp3_file(&media, 5.0);
    let subs = temp_file("subs", "srt");
    {
        let mut file = std::fs::File::create(&subs).expect("writing the subtitle fixture");
        file.write_all(b"1\n00:00:00,500 --> 00:00:02,000\nCUE\n\n")
            .expect("writing the subtitle fixture");
    }

    let log: SeekLog = Arc::new(Mutex::new(Vec::new()));
    let playbin = FcastPlaybin::new(recording_sinks(&log)).expect("building fcastplaybin");
    let (tx, events) = std::sync::mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
        let _ = tx.send(event);
    });

    playbin.load_async(
        MediaInput::Uri(format!("file://{}", media.display())),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    let deadline = Instant::now() + BOUND;
    let mut loaded = false;
    let mut failed: Option<gst::Seqnum> = None;
    while !loaded {
        assert!(Instant::now() < deadline, "the load never reported Loaded");
        while let Ok(event) = events.try_recv() {
            match event {
                PlaybinEvent::Loaded { .. } => loaded = true,
                PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    playbin.play().expect("play");
    let deadline = Instant::now() + BOUND;
    while playbin.state_summary() != (gst::State::Playing, gst::State::VoidPending) {
        assert!(
            Instant::now() < deadline,
            "the pipeline never settled in PLAYING: {:?}",
            playbin.state_summary()
        );
        std::thread::sleep(Duration::from_millis(10));
    }
    // The premise: everything EXCEPT the external is in order, so the drop
    // below can only be the external's doing.
    let seekable = {
        let mut query = gst::query::Seeking::new(gst::Format::Time);
        playbin.pipeline().query(&mut query) && query.result().0
    };
    assert!(seekable, "the item must be seekable for this test to mean anything");

    // The invalidation, in the order the field produces it: the engine gates a
    // refresh on there being no external subtitle input, and an
    // AddSubtitleSource can land between the scheduling and the job.
    playbin
        .attach_subtitle(&format!("file://{}", subs.display()))
        .expect("attaching the external subtitle input");
    let seqnum = gst::Seqnum::next();
    playbin.refresh_seek_async(seqnum);

    let deadline = Instant::now() + BOUND;
    while failed.is_none() {
        assert!(
            Instant::now() < deadline,
            "the stale refresh seek was neither dropped nor reported; seeks seen: {:?}",
            log.lock().unwrap()
        );
        while let Ok(event) = events.try_recv() {
            match event {
                PlaybinEvent::RefreshSeekFailed { seqnum } => failed = Some(seqnum),
                PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
                _ => {}
            }
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    assert_eq!(failed, Some(seqnum), "a different refresh reported failure");
    assert!(
        !log.lock().unwrap().contains(&seqnum),
        "the dropped refresh seek was performed anyway; seeks seen: {:?}",
        log.lock().unwrap()
    );

    let _ = playbin.stop();
    let _ = std::fs::remove_file(&media);
    let _ = std::fs::remove_file(&subs);
}
