//! Property 1, site 3: `route_db3_pad` must not make an unbounded blocking
//! GStreamer call, because it runs inside decodebin3's `pad-added` emission.
//!
//! # Why a blocking call there is not just slow
//!
//! decodebin3 emits `pad-added` from `mq_slot_check_reconfiguration`, and that
//! function takes SELECTION_LOCK before it ever reaches the pad
//! (`gstdecodebin3.c:3413` -> `db_output_stream_reconfigure` ->
//! `db_output_stream_setup_decoder` -> `db_output_stream_expose_src_pad` ->
//! `gst_element_add_pad`, which is the emission). `gst_element_send_event` then
//! takes the element's STATE_LOCK on top of that
//! (`gstelement.c`, `gst_element_send_event`). So for as long as the crate's
//! handler does not return, decodebin3 holds BOTH locks and:
//!
//! * every `SELECT_STREAMS` the crate dispatches blocks
//!   (`handle_select_streams` opens with SELECTION_LOCK,
//!   `gstdecodebin3.c:4618`), i.e. the whole selection engine is dead,
//! * so does any other slot's reconfiguration, i.e. the rest of decodebin3,
//! * and so does any state change of decodebin3, i.e. the teardown.
//!
//! The blocking call is `Inner::ensure_video_chain`'s `set_state` on the video
//! chain (`lib.rs`, `route_db3_pad`'s `StreamKind::Video` arm), and
//! `ensure_audio_sink`'s on the audio one, which is the same shape one arm
//! above. `set_state` runs the element's whole transition on the calling
//! thread, so whatever the sink does in its transition, this streaming thread
//! waits for.
//!
//! # What the crate does instead
//!
//! `route_db3_pad` keeps every non-blocking part (the streamsynchronizer
//! attach, the chain's pipeline MEMBERSHIP, the link into the chain entry and
//! the routing entry) inline and synchronous, because the selection dance
//! depends on all of it being visible the moment `send_event(SELECT_STREAMS)`
//! returns. Only the ACTIVATION goes to `fpb-join`, and the stream is held at
//! the streamsynchronizer src pad by a blocking probe until it is done, so
//! nothing is pushed into a chain that is not up yet.
//!
//! # The manufactured obstacle
//!
//! There is no reproducer for this in the wild, exactly as `NEXT-FIXES-PLAN.md`
//! section 5.2 says, so the obstacle is MANUFACTURED the way
//! `tests/teardown_races.rs` manufactures its held stream lock: `ftestsink`
//! grew a `stall-transition`/`stall-ms` pair that sleeps ONCE inside a chosen
//! transition. That is not a contrivance peculiar to tests. The receiver's
//! video sink is window- and GPU-bound: its `Ready->Paused` creates a surface
//! and round-trips to the display server, and the crate's own docs already
//! record a four-way wedge in which the mirror-image call
//! (`remove_video_chain`'s `set_state`, on `pad-removed`) sat waiting for a
//! state lock while `play`'s `set_state` and the select sender waited behind it
//! (see `Job::VideoChainGone`).
//!
//! `stalled-thread` publishes the name of the thread that entered the stall,
//! which is the direct evidence for the site: it is the thread that called
//! `set_state`, i.e. the thread running `route_db3_pad`.
//!
//! Two traps `teardown_races.rs` records and this file obeys:
//!
//! * the assertion bound sits WELL BELOW the manufactured hold, so waiting the
//!   hold out cannot pass,
//! * nothing may hang the harness, so the probe runs on its own thread and the
//!   playbin is leaked rather than torn down (a teardown would walk into the
//!   stalled sink).
//!
//! And the `teardown_full_multiqueue.rs` rule: if the window is never reached,
//! print NO VERDICT and return green rather than pass for the wrong reason.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
};
use fcasttest::{
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::FTestSink,
    spec::{Pacing, StreamKind, StreamSpec},
};
use gst::prelude::*;

/// How long the sink's transition is held. Comfortably longer than any healthy
/// route and comfortably longer than [`PROBE_BOUND`], so neither side of the
/// assertion is marginal.
const HELD: Duration = Duration::from_secs(12);

/// What decodebin3 is allowed to take to answer a stream selection while the
/// sink's transition is held. WELL BELOW [`HELD`]: a decodebin3 whose locks are
/// held by the route waits the whole hold out, a decodebin3 whose route
/// returned answers in microseconds.
const PROBE_BOUND: Duration = Duration::from_secs(3);

/// Generous, like every other suite here: the whole crate's tests run
/// concurrently.
const BOUND: Duration = Duration::from_secs(40);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// Video and audio, long enough that nothing reaches EOS during the hold.
fn media(key: &str) -> ScenarioHandle {
    let video = StreamSpec::new(
        "video_0",
        StreamKind::Video {
            width: 16,
            height: 16,
            fps: gst::Fraction::new(10, 1),
            keyframe_interval: 1,
        },
    );
    ScenarioBuilder::new(key)
        .stream(video)
        .stream(StreamSpec::audio("audio_0"))
        .duration(gst::ClockTime::from_seconds(90))
        .pacing(Pacing::Jitter {
            base_ms: 5,
            jitter_ms: 0,
        })
        .register()
}

/// A sink that stalls once inside `Ready->Paused`, which is the transition
/// `ensure_video_chain`/`ensure_audio_sink` drive it through while routing.
fn stalling_sink() -> FTestSink {
    let sink = FTestSink::new();
    sink.set_property("stall-transition", "ReadyToPaused");
    sink.set_property("stall-ms", HELD.as_millis() as u64);
    sink
}

struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<PlaybinEvent>,
    error: std::cell::RefCell<Option<String>>,
}

impl Harness {
    fn new(video: gst::Element, audio: AudioSink) -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video),
            audio,
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });
        Self {
            playbin,
            events,
            error: std::cell::RefCell::new(None),
        }
    }

    /// The receiver's own pump. Nothing here may block: every call is either
    /// queued to a crate thread or takes locks the route does not hold.
    fn pump(&self) {
        self.playbin.poll_text_policy();
        let (current, pending) = self.playbin.state_summary();
        let quiet = current == gst::State::Playing
            && pending == gst::State::VoidPending
            && !self.playbin.has_async_transition();
        self.playbin.pump_selection(SelectionGate {
            quiet,
            paused: false,
            seekable: false,
        });
        while let Ok(event) = self.events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = event {
                *self.error.borrow_mut() = Some(error.to_string());
            }
        }
    }

    /// Pump until `done`, or until `bound` runs out. Returns whether `done`
    /// became true: a window that is never reached is a NO VERDICT, not a
    /// failure.
    fn pump_until(&self, bound: Duration, mut done: impl FnMut(&Self) -> bool) -> bool {
        let deadline = Instant::now() + bound;
        loop {
            self.pump();
            if done(self) {
                return true;
            }
            if Instant::now() >= deadline {
                return false;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn db3(&self) -> Option<gst::Element> {
        self.playbin.pipeline().by_name("fpb-decodebin")
    }
}

/// Ask decodebin3 for a stream selection and time the answer.
///
/// The event names a stream no collection contains, so `handle_select_streams`
/// takes SELECTION_LOCK, finds no collection, warns and returns: it changes
/// nothing at all, which is what makes it usable as a probe. What it measures
/// is exactly what the crate's own selection dispatch measures, because
/// `FcastPlaybin::select_streams` reaches decodebin3 through the same
/// `send_event`.
///
/// Runs on its own thread so a genuinely wedged decodebin3 reports instead of
/// hanging the harness.
fn selection_answer_time(db3: gst::Element) -> Option<Duration> {
    let (tx, rx) = mpsc::channel();
    std::thread::Builder::new()
        .name("route-join-probe".into())
        .spawn(move || {
            let event =
                gst::event::SelectStreams::builder(["fpb-route-join-probe/no-such-stream"]).build();
            let started = Instant::now();
            db3.send_event(event);
            let _ = tx.send(started.elapsed());
        })
        .expect("spawning the selection probe thread");
    rx.recv_timeout(PROBE_BOUND).ok()
}

/// SUBJECT. The video sink's `Ready->Paused` is held for [`HELD`] while the
/// crate routes the load's video pad. The thread that walks into that
/// transition is decodebin3's, and it is holding decodebin3's SELECTION_LOCK
/// and STATE_LOCK while it waits.
///
/// The assertion is not about the route being fast. It is about decodebin3
/// still answering: a route that hands the blocking call to the worker leaves
/// the streaming thread free and decodebin3 responsive, and the load simply
/// completes once the sink's transition does.
#[test]
fn a_slow_video_sink_transition_does_not_freeze_decodebin3() {
    init();
    let media = media("rjthread1");
    let video_sink = stalling_sink();
    let harness = Harness::new(
        video_sink.clone().upcast(),
        AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
    );

    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );

    let engaged = harness.pump_until(BOUND, |_| {
        video_sink
            .property::<Option<String>>("stalled-thread")
            .is_some()
    });
    let Some(stalled_on) = video_sink.property::<Option<String>>("stalled-thread") else {
        std::mem::forget(harness);
        println!(
            "NO VERDICT: the video sink never entered Ready->Paused within {BOUND:?} \
             (engaged={engaged}), so the window this test measures was never reached"
        );
        return;
    };
    let Some(db3) = harness.db3() else {
        std::mem::forget(harness);
        println!("NO VERDICT: no fpb-decodebin in the pipeline while the sink was stalled");
        return;
    };

    let waited = selection_answer_time(db3);
    // The stall is still running and the pipeline is mid-route; a Drop here
    // would walk a teardown into it. Leak, like `teardown_races.rs` does.
    std::mem::forget(harness);
    // The scenario stays registered on purpose: its source threads are still
    // running inside a leaked pipeline, and pulling the media out from under
    // them would only add noise.

    match waited {
        None => panic!(
            "decodebin3 did not answer a stream selection within {PROBE_BOUND:?} while the video \
             sink's Ready->Paused was held for {HELD:?} on thread {stalled_on:?}. That thread is \
             inside route_db3_pad -> ensure_video_chain -> set_state, holding decodebin3's \
             SELECTION_LOCK and STATE_LOCK: every selection, every other slot's reconfiguration \
             and every teardown is stuck behind it"
        ),
        Some(waited) => {
            // Printed rather than asserted on: the name is the mechanism, and
            // a fixed build reads "fpb-join" here where a broken one reads
            // "multiqueueN:src".
            println!(
                "decodebin3 answered a selection in {waited:?} while the video sink's \
                 Ready->Paused was held on thread {stalled_on:?}"
            );
            assert!(
                waited < PROBE_BOUND,
                "decodebin3 took {waited:?} to answer a stream selection while the video sink's \
                 transition was held on thread {stalled_on:?}, so it waited the hold out instead \
                 of staying off it"
            );
        }
    }
}

/// The same site on the AUDIO arm: `ensure_audio_sink`'s `set_state` is the
/// identical shape one match arm above the video one, and this stalls the audio
/// sink instead. Kept as its own test rather than folded in, because a load
/// routes audio and video from different reconfigurations and only the stalled
/// one is being measured.
#[test]
fn a_slow_audio_sink_transition_does_not_freeze_decodebin3() {
    init();
    let media = media("rjthread2");
    let (sink_tx, sink_rx) = mpsc::channel();
    let harness = Harness::new(
        FTestSink::new().upcast(),
        AudioSink::Factory(Box::new(move || {
            let sink = stalling_sink();
            let _ = sink_tx.send(sink.clone());
            Ok(sink.upcast())
        })),
    );

    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );

    let mut audio_sink: Option<FTestSink> = None;
    harness.pump_until(BOUND, |_| {
        if audio_sink.is_none() {
            audio_sink = sink_rx.try_recv().ok();
        }
        audio_sink
            .as_ref()
            .is_some_and(|sink| sink.property::<Option<String>>("stalled-thread").is_some())
    });
    let stalled_on = audio_sink
        .as_ref()
        .and_then(|sink| sink.property::<Option<String>>("stalled-thread"));
    let (Some(stalled_on), Some(db3)) = (stalled_on, harness.db3()) else {
        std::mem::forget(harness);
        println!(
            "NO VERDICT: the audio sink never entered Ready->Paused inside a route within \
             {BOUND:?}, so the window this test measures was never reached"
        );
        return;
    };

    let waited = selection_answer_time(db3);
    std::mem::forget(harness);
    // The scenario stays registered on purpose: its source threads are still
    // running inside a leaked pipeline, and pulling the media out from under
    // them would only add noise.

    match waited {
        None => panic!(
            "decodebin3 did not answer a stream selection within {PROBE_BOUND:?} while the audio \
             sink's Ready->Paused was held for {HELD:?} on thread {stalled_on:?}: \
             route_db3_pad -> ensure_audio_sink -> set_state is blocking that streaming thread \
             under decodebin3's SELECTION_LOCK"
        ),
        Some(waited) => {
            println!(
                "decodebin3 answered a selection in {waited:?} while the audio sink's \
                 Ready->Paused was held on thread {stalled_on:?}"
            );
            assert!(
                waited < PROBE_BOUND,
                "decodebin3 took {waited:?} to answer a stream selection while the audio sink's \
                 transition was held on thread {stalled_on:?}"
            );
        }
    }
}

/// CONTROL. The identical load with NO stall anywhere. It establishes that the
/// probe is not simply slow on a busy box and that an ordinary load does not
/// hold decodebin3's locks for seconds: the same probe, taken while the load is
/// in flight and again once it has settled, must answer well inside the bound.
#[test]
fn decodebin3_answers_selections_during_an_ordinary_load() {
    init();
    let media = media("rjthread3");
    let harness = Harness::new(
        FTestSink::new().upcast(),
        AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
    );

    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    let reached = harness.pump_until(BOUND, |h| h.db3().is_some());
    assert!(reached, "no fpb-decodebin appeared within {BOUND:?}");
    let mid_load = selection_answer_time(harness.db3().expect("checked above"));

    let settled = harness.pump_until(BOUND, |h| h.playbin.state_summary().0 >= gst::State::Paused);
    assert!(settled, "the load never reached PAUSED within {BOUND:?}");
    let after = selection_answer_time(harness.db3().expect("the core outlives the load"));

    std::mem::forget(harness);
    // The scenario stays registered on purpose: its source threads are still
    // running inside a leaked pipeline, and pulling the media out from under
    // them would only add noise.

    for (what, waited) in [("mid-load", mid_load), ("after the load", after)] {
        let Some(waited) = waited else {
            panic!("decodebin3 did not answer a selection {what} within {PROBE_BOUND:?}");
        };
        assert!(
            waited < PROBE_BOUND,
            "decodebin3 took {waited:?} to answer a selection {what}, so the probe itself is not \
             a usable instrument"
        );
    }
}
