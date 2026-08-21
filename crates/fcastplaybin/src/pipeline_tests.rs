/// gst::init plus the elements the APPLICATION registers in production:
/// the constructor builds fcastaudiostretch unconditionally, and these
/// tests are their own application.
fn test_init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

use super::*;
use std::time::Instant;

use gst::prelude::*;

use crate::gapless::SwapState;

/// Encode `seconds` of silence to an MP3 file (audio/mpeg, the real fcomp
/// container). Done once per source so playback can go through the real
/// `urisourcebin` topology below.
fn make_mp3_file(path: &std::path::Path, seconds: f64) {
    // audiotestsrc defaults: 44100 Hz, 1024 samples/buffer.
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

/// A unique temp path under the test dir (no wall clock needed).
fn temp_mp3(tag: &str) -> std::path::PathBuf {
    use std::sync::atomic::{AtomicU64, Ordering};
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let n = SEQ.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "fcastplaybin-gapless-{}-{tag}-{n}.mp3",
        std::process::id()
    ))
}

/// The real gapless source: `urisourcebin` over a file URI with
/// `parse-streams`, exactly what `media_source::build_uri_source_with_head`
/// builds (so decodebin3 gets the stream collection urisourcebin forwards).
fn uri_source(path: &std::path::Path) -> gst::Element {
    gst::ElementFactory::make("urisourcebin")
        .property("uri", format!("file://{}", path.display()))
        .property("parse-streams", true)
        .property("use-buffering", true)
        .build()
        .unwrap()
}

fn fake_audio_sinks() -> Sinks {
    Sinks {
        video: None,
        audio: AudioSink::Factory(Box::new(|| {
            Ok(gst::ElementFactory::make("fakesink")
                .property("sync", true)
                .build()?)
        })),
    }
}

/// Gapless handoff smoke test, end to end on a real pipeline: play item A
/// through `urisourcebin` (the field's gapless source topology), pre-arm
/// item B, and assert B plays to ITS end rather than being cut off at A's.
/// Guards the generic swap path. NOTE: this passes for `file://`/`filesrc`
/// sources. The FIELD bug (an fcomp item cut at the previous item's
/// declared duration) does NOT reproduce here, which localizes it to
/// `fcompsrc`'s size/segment/EOS behavior, not the swap itself (a
/// fcompsrc + fake-companion repro belongs in receiver-core).
#[test]
fn gapless_swap_plays_the_next_item_to_its_end() {
    test_init();
    let playbin = FcastPlaybin::new(fake_audio_sinks()).unwrap();

    let (tx, rx) = mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| match event {
        PlaybinEvent::PreparedActivated => {
            let _ = tx.send(Ev::Activated);
        }
        PlaybinEvent::EndOfStream => {
            let _ = tx.send(Ev::Eos);
        }
        _ => {}
    });

    // A is long enough that decodebin3's multiqueue and the decoupling
    // audio queue cannot swallow it whole, so its EOS is PACED to near its
    // end (mirroring a real track). Pre-arming early then lands before
    // A's EOS reaches the output-side hold, the order the field hits.
    let a_secs = 5.0;
    let b_secs = 2.0;
    let a_path = temp_mp3("a");
    let b_path = temp_mp3("b");
    make_mp3_file(&a_path, a_secs);
    make_mp3_file(&b_path, b_secs);

    playbin
        .load(MediaInput::Element(uri_source(&a_path)), StartPoint::Live)
        .unwrap();
    // Pre-arm B BEFORE A's end-of-stream can reach the output hold. The
    // field pre-arms tens of seconds early; a short test source's input
    // drains at load (its parsed data fits decodebin3's multiqueue whole),
    // so the pre-arm has to be up front to win that race. `pending` is
    // then set when A's EOS drains out, which is what must hold it back.
    playbin.prepare_next_async(MediaInput::Element(uri_source(&b_path)));
    let t0 = Instant::now();
    playbin.play().unwrap();

    let mut activated = false;
    let eos_elapsed = loop {
        match rx.recv_timeout(Duration::from_secs(15)) {
            Ok(Ev::Activated) => activated = true,
            Ok(Ev::Eos) => break Some(t0.elapsed()),
            Err(_) => break None,
        }
    };
    let _ = playbin.stop();
    let _ = std::fs::remove_file(&a_path);
    let _ = std::fs::remove_file(&b_path);

    let eos_elapsed = eos_elapsed.expect("pipeline never reached EOS (wedged)");
    assert!(
        activated,
        "the prepared item never activated (handoff missed)"
    );
    // Gapless success plays A then B back to back (~7s). The bug cuts B off
    // at A's end, so EOS lands near A's length (~5s) instead. The 6s
    // threshold sits between the two with margin for buffering slack.
    assert!(
        eos_elapsed >= Duration::from_millis(6000),
        "playback ended after {eos_elapsed:?}, expected ~{}s (A+B): the \
         next item was cut off at the previous item's segment end",
        a_secs + b_secs,
    );
}

#[derive(PartialEq)]
enum Ev {
    Activated,
    Eos,
}

/// The duration-refresh edge, end to end through the real bus
/// translation: a `DURATION_CHANGED` must reach the caller as
/// [`PlaybinEvent::DurationChanged`] (its cue to re-query), and must be
/// dropped while a performed swap waits to activate, where the query would
/// be answered by the successor item.
///
/// Posting the message is the deterministic trigger: translation is a bus
/// SYNC handler, so `post` runs it inline on this thread and the channel is
/// already settled when it returns. The message carries no payload, so a
/// synthesized one is indistinguishable from a demuxer's (which is the
/// whole point of the no-payload contract).
#[test]
fn duration_changed_reaches_the_caller_except_mid_activation() {
    test_init();
    let playbin = FcastPlaybin::new(fake_audio_sinks()).unwrap();

    let (tx, rx) = mpsc::channel();
    playbin.set_event_handler(None, move |event, generation| {
        if matches!(event, PlaybinEvent::DurationChanged) {
            let _ = tx.send(generation);
        }
    });

    let bus = playbin.bus();
    bus.post(gst::message::DurationChanged::new()).unwrap();
    rx.try_recv()
        .expect("a duration-changed on the bus must reach the caller");

    // The swapped-with-pending-activation window (the same predicate the
    // cancel refusal uses).
    *playbin.inner.swap_gate.state.lock() = SwapState {
        pending: Some(42),
        swapped: true,
        ..Default::default()
    };
    bus.post(gst::message::DurationChanged::new()).unwrap();
    assert!(
        rx.try_recv().is_err(),
        "duration-changed must be dropped while a performed swap waits to activate: \
         upstream answers for the successor item there"
    );

    // Leave the gate as found: teardown reads it.
    *playbin.inner.swap_gate.state.lock() = SwapState::default();
    let _ = playbin.stop();
}

/// The routed-pad EOS gate must never split a group. Two sibling A/V EOS
/// racing a pre-arm may both pass or both drop, never one of each: the
/// passed one parks its pushing thread (a multiqueue slot task) inside
/// streamsynchronizer's group wait, and the dropped sibling never arrives to
/// complete the group (CLEANUP invariant 12, the boundary freeze).
///
/// Driven straight against `Inner`, because in a real pipeline the window
/// between one sibling's verdict and its mirror commit is sub-microsecond.
/// The three threads are the field's: two decodebin3 streaming threads at the
/// gate while the worker runs `Job::PrepareNext`.
///
/// Measured teeth: against a gate that decides and commits under separate
/// locks this fails on round 0 as soon as anything delays the commit (a
/// `yield_now` between the two stands in for the scheduler), while the
/// natural window is too narrow to lose 1000 rounds to. What it pins is
/// therefore the structure, not the odds.
#[test]
fn the_eos_gate_never_splits_a_group_against_a_racing_pre_arm() {
    use crate::decisions::EosGate;

    test_init();
    let playbin = FcastPlaybin::new(fake_audio_sinks()).unwrap();
    let inner = &playbin.inner;

    for round in 0..1000u64 {
        let group = gst::GroupId::next();
        *inner.active_group.lock() = Some(group);
        *inner.retired_group.lock() = None;
        *inner.passing_eos_group.lock() = None;
        *inner.swap_gate.state.lock() = SwapState::default();

        let start = std::sync::Barrier::new(3);
        let (audio, video) = std::thread::scope(|scope| {
            // The arm, exactly as `Job::PrepareNext` writes it.
            scope.spawn(|| {
                start.wait();
                *inner.swap_gate.state.lock() = SwapState {
                    pending: Some(round + 1),
                    drained: false,
                    swapped: false,
                    dropped_eos: false,
                };
            });
            let audio = scope.spawn(|| {
                start.wait();
                inner.gapless_eos_gate(Some(group), true)
            });
            let video = scope.spawn(|| {
                start.wait();
                inner.gapless_eos_gate(Some(group), true)
            });
            (audio.join().unwrap(), video.join().unwrap())
        });

        let passed = |gate: EosGate| matches!(gate, EosGate::Pass { .. } | EosGate::SiblingPass);
        assert_eq!(
            passed(audio),
            passed(video),
            "round {round}: the group was split, {audio:?} vs {video:?}"
        );
        // A passed group is committed, so any straggler sibling passes too.
        if passed(audio) {
            assert_eq!(*inner.passing_eos_group.lock(), Some(group));
            assert_eq!(
                inner.gapless_eos_gate(Some(group), true),
                EosGate::SiblingPass
            );
        }
    }

    // Leave the gate as found: teardown reads it.
    *inner.swap_gate.state.lock() = SwapState::default();
    *inner.active_group.lock() = None;
    *inner.passing_eos_group.lock() = None;
    let _ = playbin.stop();
}

/// One kept cue, shaped like what `Inner::park_text_stream`'s appsink hands to
/// the ring, i.e. a buffer with a pts, text caps and a time segment.
fn parked_sample(pts: gst::ClockTime) -> gst::Sample {
    let mut buffer = gst::Buffer::from_slice(b"a cue from the PREVIOUS item".as_slice());
    {
        let buffer = buffer.get_mut().expect("a fresh buffer is writable");
        buffer.set_pts(pts);
        buffer.set_duration(gst::ClockTime::from_seconds(2));
    }
    gst::Sample::builder()
        .buffer(&buffer)
        .caps(
            &gst::Caps::builder("text/x-raw")
                .field("format", "utf8")
                .build(),
        )
        .segment(&gst::FormattedSegment::<gst::ClockTime>::new())
        .build()
}

/// A LOAD must not leave the previous item's text-park memos behind.
///
/// Both maps key on the decodebin3 output pad NAME (`text_0`, ...), which is
/// per-ELEMENT, so every load's fresh core hands out the same names. A leftover
/// `parked_text_cues` ring is replayed into the NEW item at its first join, and
/// a leftover `suppress_text_clear` makes the new branch skip its own opening
/// `Clear`, the one signal that says the previous item's cues are stale.
///
/// `unroute_db3_pad` clears them on the normal path, so the case under test is
/// `Inner::teardown_core`'s straggler entry, where pad-removed never came. The
/// entries are staged directly rather than parked for real (the trick
/// `Inner::stage_text_caps_loss` uses), since what is under test is the RESET.
#[test]
fn a_load_forgets_the_previous_items_text_park() {
    test_init();
    let playbin = FcastPlaybin::new(fake_audio_sinks()).unwrap();
    let path = temp_mp3("park-reset");
    make_mp3_file(&path, 1.0);

    // The previous item's leftovers, under the name the NEXT core reuses.
    playbin
        .inner
        .parked_text_cues
        .lock()
        .entry("text_0".to_string())
        .or_default()
        .push_back((parked_sample(gst::ClockTime::ZERO), Instant::now()));
    playbin
        .inner
        .suppress_text_clear
        .lock()
        .insert("text_0".to_string());

    playbin
        .load(MediaInput::Element(uri_source(&path)), StartPoint::Live)
        .unwrap();

    let parked = playbin.inner.parked_text_cues.lock().len();
    let suppressed = playbin.inner.suppress_text_clear.lock().len();
    let _ = playbin.stop();
    let _ = std::fs::remove_file(&path);

    assert_eq!(
        parked, 0,
        "the load left a text park keyed by a pad name the fresh core reuses; \
         its cues replay into the new item"
    );
    assert_eq!(
        suppressed, 0,
        "the load left a clear-suppression keyed by a pad name the fresh core \
         reuses; the new item's branch swallows its own opening Clear"
    );
}

/// A STOP must leave no per-item state behind, even when no load ever follows.
///
/// The load reset and the stop reset were two hand-maintained lists, and they
/// diverged: the stop's copy had drifted to 9 of the load's 17 clears, so a
/// receiver that stops and idles kept the ended item's degradation memos and
/// drained-stream ids for as long as it lived. Growth-shaped state emptied only
/// by the NEXT load is a leak in a receiver that plays for days, which is the
/// very thing the load's copy documents itself as preventing.
///
/// Both boundaries call [`Inner::reset_item_state`] now, and this pins the
/// whole list at the boundary that used to be the short one: it fails on the
/// first entry that gets cleared at a load and forgotten at a stop.
///
/// The residue is staged directly (the trick the load-side twin above uses),
/// because what is under test is the RESET, not the paths that fill these in.
/// `held_activation` is the one entry not staged here: it is constructed only
/// inside `gapless.rs`, and its None is asserted as it stands.
#[test]
fn a_stop_leaves_no_per_item_state_behind() {
    use crate::text_policy::{DegradationMemo, TextDegradation};

    test_init();
    let playbin = FcastPlaybin::new(fake_audio_sinks()).unwrap();
    let path = temp_mp3("stop-reset");
    make_mp3_file(&path, 1.0);
    playbin
        .load(MediaInput::Element(uri_source(&path)), StartPoint::Live)
        .unwrap();

    // What an item has accumulated by the time it ends.
    let inner = &playbin.inner;
    let group = gst::GroupId::next();
    *inner.active_group.lock() = Some(group);
    *inner.retired_group.lock() = Some(group);
    *inner.passing_eos_group.lock() = Some(group);
    inner.input_eos_sids.lock().insert("audio-0".to_string());
    inner.last_upstream_ids.lock().push("audio-0".to_string());
    inner.text_degradations.lock().insert(
        (TextDegradation::Unsupported, "text-0".to_string(), 0),
        DegradationMemo::Spoken,
    );
    inner
        .parked_text_cues
        .lock()
        .entry("text_0".to_string())
        .or_default()
        .push_back((parked_sample(gst::ClockTime::ZERO), Instant::now()));
    inner
        .suppress_text_clear
        .lock()
        .insert("text_0".to_string());
    *inner.intended_timeline.lock() = (2.0, gst::ClockTime::from_seconds(30));
    inner.video_deselected.store(true, Ordering::SeqCst);
    inner.video_unrouted_once.store(true, Ordering::SeqCst);

    playbin.stop().expect("the stop itself must succeed");
    let _ = std::fs::remove_file(&path);

    // The unbounded ones first: they are the leak, and only a load ever
    // emptied them.
    assert!(
        inner.text_degradations.lock().is_empty(),
        "the stop kept the ended item's text degradation memos; they only ever grow"
    );
    assert!(
        inner.input_eos_sids.lock().is_empty(),
        "the stop kept the ended item's drained input stream ids"
    );
    assert!(
        inner.parked_text_cues.lock().is_empty(),
        "the stop kept a text park keyed by a pad name the next core reuses"
    );
    assert!(
        inner.suppress_text_clear.lock().is_empty(),
        "the stop kept a clear-suppression keyed by a pad name the next core reuses"
    );
    assert!(
        inner.last_upstream_ids.lock().is_empty(),
        "the stop kept the ended item's upstream-selection id mirror"
    );
    // And the per-item mirrors.
    assert_eq!(
        *inner.active_group.lock(),
        None,
        "active group survived a stop"
    );
    assert_eq!(
        *inner.retired_group.lock(),
        None,
        "retired group survived a stop"
    );
    assert_eq!(
        *inner.passing_eos_group.lock(),
        None,
        "passing-EOS group survived a stop"
    );
    assert!(
        inner.held_activation.lock().is_none(),
        "a held gapless activation survived a stop; its events name an ended item"
    );
    assert_eq!(
        *inner.intended_timeline.lock(),
        (1.0, gst::ClockTime::ZERO),
        "the ended item's intended timeline survived a stop"
    );
    assert!(
        !inner.video_deselected.load(Ordering::SeqCst),
        "the ended item's video-deselect mirror survived a stop"
    );
    assert!(
        !inner.video_unrouted_once.load(Ordering::SeqCst),
        "the ended item's video-unroute mirror survived a stop"
    );
}

/// The video chain must never cap the pipeline's max latency below a live
/// audio sink's min. Field: live SABR, the pwaudiosink declares min 235ms
/// while the queue-less video branch could absorb 33ms (one decoded frame),
/// so latency configuration failed ("Impossible to configure latency",
/// "clock problem"), the video sink fell back to zero processing latency and
/// QoS-dropped most frames. The chain's head queue (`fpb-vqueue`, non-leaky,
/// no time cap) answers the latency query with max=unlimited, playsink's own
/// video-chain shape. Guards the queue's config: a time cap or a leaky mode
/// added later turns max finite again and re-breaks live A/V.
#[test]
fn video_chain_reports_unbounded_max_latency() {
    test_init();
    let playbin = FcastPlaybin::new(fake_audio_sinks()).unwrap();
    let inner = &playbin.inner;
    inner.attach_video_chain().unwrap();

    // The internal edge is up: queue into sink.
    let entry_src = inner.video_entry.static_pad("src").unwrap();
    assert_eq!(
        entry_src.peer(),
        inner.video_sink.static_pad("sink"),
        "the chain must be vqueue ! sink"
    );

    // A live upstream with a BOUNDED max, the shape a live source hands the
    // chain. Bounded matters: an unlimited upstream would mask a
    // wrongly-capped queue.
    let upstream_max = gst::ClockTime::from_mseconds(33);
    let templ = gst::PadTemplate::new(
        "src",
        gst::PadDirection::Src,
        gst::PadPresence::Always,
        &gst::Caps::new_any(),
    )
    .unwrap();
    let feeding = gst::Pad::builder_from_template(&templ)
        .query_function(move |_pad, _parent, query| {
            if let gst::QueryViewMut::Latency(q) = query.view_mut() {
                q.set(true, gst::ClockTime::ZERO, upstream_max);
                return true;
            }
            false
        })
        .build();
    feeding.set_active(true).unwrap();
    let chain_entry = inner.video_entry.static_pad("sink").unwrap();
    feeding.link(&chain_entry).unwrap();

    let mut query = gst::query::Latency::new();
    assert!(
        inner
            .video_entry
            .static_pad("src")
            .unwrap()
            .query(&mut query),
        "the chain's queue must answer the latency query"
    );
    let (live, min, max) = query.result();
    assert!(live, "liveness must pass through the chain");
    assert_eq!(min, gst::ClockTime::ZERO, "the queue must not raise min");
    assert_eq!(
        max, None,
        "the video chain must report an unlimited max latency; a finite max \
         caps the pipeline below a live audio sink's min and playback opens \
         with a QoS drop storm"
    );

    let _ = feeding.unlink(&chain_entry);
    let _ = playbin.stop();
}

/// `buffered_ahead` must count appsrc queue levels: the SABR source buffers
/// its media in per-track appsrcs, and the receiver gates its "server busy"
/// countdown on this runway measurement, so dropping the appsrc arm would
/// silently under-report the buffer and re-show the pill during healthy live
/// playback. appsrc accepts pushes before it starts (its queue is independent
/// of the streaming task), so the level here is deterministic.
#[test]
fn buffered_ahead_reads_appsrc_levels() {
    test_init();
    let playbin = FcastPlaybin::new(fake_audio_sinks()).unwrap();

    // Polled BEFORE the appsrc joins, which is what makes the rest of this a
    // test of the probe cache too: this call walks the graph as it is now and
    // caches it, so the level below can only be found if adding an element
    // re-dirtied that list (see `LevelProbes`). The construction graph has
    // queues and the token appsrc in it, all at zero, hence None.
    assert_eq!(
        playbin.buffered_ahead(),
        None,
        "an idle graph buffers nothing"
    );

    let src = gst::ElementFactory::make("appsrc")
        .property_from_str("format", "time")
        .build()
        .unwrap();
    playbin.inner.pipeline.add(&src).unwrap();

    // Block the (unlinked) src pad so the source task parks on its first
    // push instead of erroring with NOT_LINKED and draining the queue.
    let src_pad = src.static_pad("src").unwrap();
    src_pad
        .add_probe(
            gst::PadProbeType::BLOCK | gst::PadProbeType::BUFFER,
            |_pad, _info| gst::PadProbeReturn::Ok,
        )
        .unwrap();
    // Started (READY->PAUSED) because the time-level accounting only runs
    // once the segment is initialized; pushes before start count bytes only.
    src.set_state(gst::State::Paused).unwrap();

    // 4 x 500ms of timestamped media. The task pops at most the first buffer
    // into the blocked pad, so at least 1.5s stays queued.
    for i in 0..4u64 {
        let mut buffer = gst::Buffer::with_size(256).unwrap();
        {
            let buffer = buffer.get_mut().unwrap();
            buffer.set_pts(gst::ClockTime::from_mseconds(i * 500));
            buffer.set_duration(gst::ClockTime::from_mseconds(500));
        }
        let ret = src.emit_by_name::<gst::FlowReturn>("push-buffer", &[&buffer]);
        assert_eq!(ret, gst::FlowReturn::Ok);
    }

    let deadline = Instant::now() + Duration::from_secs(5);
    let level = loop {
        match playbin.buffered_ahead() {
            Some(level) => break level,
            None if Instant::now() >= deadline => {
                panic!("appsrc queue level never counted toward the buffered runway")
            }
            None => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    assert!(
        level >= gst::ClockTime::from_mseconds(1_500),
        "queued appsrc media under-reported: {level}"
    );

    // Unwinds the task parked in the blocked probe (pad deactivation flushes it).
    let _ = src.set_state(gst::State::Null);
    let _ = playbin.stop();
}

/// The watchdog end to end, without FAST or media: attach a URI that
/// never produces a stream (the pipeline sits in NULL, so urisourcebin
/// never starts) and expect the crate to detach it and report
/// `ExternalSubtitleFailed` on its own.
#[test]
fn watchdog_fails_a_subtitle_that_never_materializes() {
    test_init();
    let playbin = FcastPlaybin::new(Sinks {
        video: None,
        audio: AudioSink::Auto,
    })
    .unwrap();
    playbin.set_external_sub_timeout(Duration::from_millis(200));

    let (tx, rx) = mpsc::channel();
    playbin.set_event_handler(None, move |event, _generation| {
        if let PlaybinEvent::ExternalSubtitleFailed { id } = event {
            let _ = tx.send(id);
        }
    });

    let id = playbin
        .attach_subtitle("file:///nonexistent/fcastplaybin-watchdog-test.srt")
        .unwrap();
    assert!(playbin.has_external_subtitles());

    let failed = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("watchdog should fail the stream-less input");
    assert_eq!(failed, id);

    // The crate detached the input itself: nothing external remains and
    // a caller-side detach of the reported id is a (harmless) error.
    assert!(!playbin.has_external_subtitles());
    assert!(playbin.subtitle_stream_ids(id).is_empty());
    assert!(playbin.detach_subtitle(id).is_err());
}
