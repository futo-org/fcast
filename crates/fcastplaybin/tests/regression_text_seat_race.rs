//! The overlay's subtitle seat is decided by TWO threads, and they used to
//! share no lock.
//!
//! `Inner::poll_text_policy` links a fresh `fpb-tqueue-* -> subtitle_sink` on
//! the CALLER thread. `Inner::dispose_text_branch_on` checks
//! `subtitle_sink.is_linked()` and, if it reads unlinked, sends that pad a
//! FLUSH_START/FLUSH_STOP pair from the WORKER. Lose that race and the pair
//! lands on the branch that just joined: the fresh queue's `srcresult` latches
//! FLUSHING (`gstqueue.c` `out_flushing`), every later multiqueue push into it
//! returns FLUSHING, decodebin3's single-queue latches too, and on an adaptive
//! main input adaptivedemux2's ONE output task pauses for good with no error
//! posted anywhere. That is FREEZE-DIAGN.md's top surviving candidate for the
//! field's silent DASH freeze (section 8.2 #1).
//!
//! # The interleaving this drives
//!
//! A disposal only runs on the worker when it was POSTPONED, which
//! `detach_text_parts` does at a pipeline resting in PAUSED. So each cycle is:
//! pause, subtitles off (park -> detach -> disposal postponed), play (the
//! worker's `run_deferred_text_work` disposes), and subtitles back on with the
//! caller pumping as tightly as it can (which links the fresh branch). The two
//! critical sections then run at the same time on two threads, which is exactly
//! what the TOCTOU needs.
//!
//! # What it asserts, and how to read it
//!
//! Liveness after every cycle: a cue of the re-selected track reaches the
//! overlay again and video keeps advancing. A won race wedges the text branch
//! (and, on an adaptive input, everything else), so a cue that never comes back
//! IS the signature.
//!
//! `text_seat_contentions()` reports how often the two sections actually
//! overlapped. It appears in every failure message but is NOT asserted on: a
//! run that never overlaps is a lucky run, not a passing one, and asserting it
//! would make the test flake on the schedule rather than on the crate.
//!
//! MEASURED on this choreography (4 cycles, debug build): 1 real overlap of the
//! two critical sections, and the disposal-to-link gap is 200-600 US in all
//! four cycles (`disposing of a text branch postponed while paused` at
//! 15:27:01.035208 followed by `text stream joined subtitleoverlay` at
//! .035491). So the window the lock closes is real and routinely sub-millisecond
//! wide, which is what makes the old lock-free `is_linked()` check a coin
//! flip rather than a theoretical concern.
//!
//! # Verification
//!
//! * Green: no env vars. The lock serializes the two sections.
//! * A/B: `FCAST_NO_TEXT_SEAT_LOCK=1` restores the unsynchronized check. The
//!   race must then be WON to fail, so a green run under the lever proves
//!   nothing either way. What it does show, with
//!   `FCASTPLAYBIN_TEST_LOG=debug`, is the two threads in the window: the
//!   worker's "disposing of a text branch postponed while paused" interleaved
//!   with the caller's "text stream joined subtitleoverlay".

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::FTestSink,
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

/// Off/on cycles. Each one is a full pause/play round trip, so this is a
/// balance between winning the race and keeping the suite quick.
const CYCLES: usize = 4;

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

fn cues(count: u32, step: gst::ClockTime, tag: &str) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{tag}{index:02}"))
        })
        .collect()
}

struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    video: fcasttest::sink::Recording,
    paused: std::cell::Cell<bool>,
    text: Arc<Mutex<Vec<String>>>,
}

impl Harness {
    fn new() -> Self {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });
        Self {
            playbin: Arc::new(playbin),
            events,
            video,
            paused: std::cell::Cell::new(false),
            text: Arc::new(Mutex::new(Vec::new())),
        }
    }

    /// The receiver's gate, minus the state machine it does not have here: an
    /// ftestsrc item never buffers and the scenario is unseekable, so the only
    /// honest variable left is `paused`.
    fn pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: false,
        });
        while let Ok(event) = self.events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error: {error}");
            }
        }
    }

    fn wait_for(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; video buffers {}, seat contentions {}, \
                 subtitle_sink peer {:?}",
                self.video.buffer_count(),
                self.playbin.text_seat_contentions(),
                self.subtitle_peer(),
            );
            self.pump();
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn subtitle_peer(&self) -> Option<String> {
        self.subtitle_sink()?
            .peer()
            .map(|peer| peer.name().to_string())
    }

    fn subtitle_sink(&self) -> Option<gst::Pad> {
        self.playbin
            .pipeline()
            .by_name("fpb-suboverlay")?
            .static_pad("subtitle_sink")
    }

    /// Every payload that crosses into the overlay, so a cue proves the branch
    /// delivers rather than merely being linked.
    fn tap_text(&self) {
        let seen = self.text.clone();
        self.subtitle_sink()
            .expect("subtitleoverlay has a subtitle_sink pad")
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, info| {
                if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data
                    && let Ok(map) = buffer.map_readable()
                {
                    seen.lock()
                        .expect("text tap")
                        .push(String::from_utf8_lossy(map.as_slice()).into_owned());
                }
                gst::PadProbeReturn::Ok
            })
            .expect("tapping the overlay's subtitle input");
    }

    fn cues_seen(&self, tag: &str) -> usize {
        self.text
            .lock()
            .expect("text tap")
            .iter()
            .filter(|payload| payload.trim_start().starts_with(tag))
            .count()
    }

    fn set_paused(&self, paused: bool) {
        if paused {
            self.playbin.pause().expect("pause");
        } else {
            self.playbin.play().expect("play");
        }
        self.paused.set(paused);
        let want = if paused {
            gst::State::Paused
        } else {
            gst::State::Playing
        };
        self.wait_for("the transport to settle", || {
            self.playbin.state_summary() == (want, gst::State::VoidPending)
                && !self.playbin.has_async_transition()
        });
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the shutdown never finished");
                    self.pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

#[test]
fn a_disposal_racing_the_overlay_seat_keeps_the_text_branch_alive() {
    init();
    let media = ScenarioBuilder::new("seatracemain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(120))
        .pacing(Pacing::Realtime)
        .register();
    // Dense cues pushed as fast as the source can, so a text push is in flight
    // (or blocked in the branch's queue) whenever the disposal lands.
    let subs = ScenarioBuilder::new("seatracesubs")
        .text("text_0", cues(600, gst::ClockTime::from_mseconds(100), "S"))
        .duration(gst::ClockTime::from_seconds(120))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let harness = Harness::new();
    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    {
        let mut loaded = false;
        harness.wait_for("the load to report Loaded", || {
            while let Ok(event) = harness.events.try_recv() {
                if let PlaybinEvent::Error { error, .. } = &event {
                    panic!("pipeline error during the load: {error}");
                }
                loaded |= matches!(event, PlaybinEvent::Loaded { .. });
            }
            loaded
        });
    }
    harness.set_paused(false);

    let id = harness
        .playbin
        .attach_subtitle(&subs.uri())
        .expect("attaching the external subtitle input");
    harness.wait_for("the external subtitle stream to materialize", || {
        !harness.playbin.subtitle_stream_ids(id).is_empty()
    });
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.pump();
    // subtitleoverlay joins the graph with the first text branch, so the tap
    // can only be installed once the branch is linked.
    harness.wait_for("the text branch to join subtitleoverlay", || {
        harness
            .subtitle_sink()
            .is_some_and(|pad| pad.is_linked())
    });
    harness.tap_text();
    let mut seen = 0;
    harness.wait_for("the first cue to reach the overlay", || {
        harness.cues_seen("S") > seen
    });
    seen = harness.cues_seen("S");

    for cycle in 0..CYCLES {
        // Postpone the disposal onto the worker: `detach_text_parts` defers the
        // blocking half at a pipeline resting in PAUSED.
        harness.set_paused(true);
        harness
            .playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
        harness.pump();
        harness.wait_for("the text branch to leave the overlay", || {
            harness.subtitle_peer().is_none()
        });

        // The worker drains the disposal on this state edge, and the request
        // right after makes the caller link a fresh branch into the same seat.
        harness.playbin.play().expect("play");
        harness.paused.set(false);
        harness
            .playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
        // No sleeping in this loop: pumping flat out is what puts the link and
        // the disposal in the same instant.
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while harness.cues_seen("S") <= seen {
            assert!(
                Instant::now() < deadline,
                "cycle {cycle}: no cue came back after the off/on; video buffers {}, \
                 seat contentions {}, subtitle_sink peer {:?}",
                harness.video.buffer_count(),
                harness.playbin.text_seat_contentions(),
                harness.subtitle_peer(),
            );
            harness.pump();
        }
        seen = harness.cues_seen("S");

        // Text alive is not enough: the hazard kills the whole output loop of a
        // shared demuxer, so video has to keep moving too.
        let video_before = harness.video.buffer_count();
        harness.wait_for("video to keep advancing", || {
            harness.video.buffer_count() > video_before
        });
    }

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}
