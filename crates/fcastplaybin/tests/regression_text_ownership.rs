//! One thread decides about the text branch. Every link, park and disposal
//! is the decider's.
//!
//! The disposal-versus-link TOCTOU was retired by ownership rather than by
//! locking harder. The link loop, dead-branch reclaim, eager park and
//! postponed disposals all run on the decider, and other threads only get to
//! say that something may have changed. The test drives a schedule that
//! reaches all three surgery sites and reads `text_surgery_off_decider()`,
//! the counter behind `Inner::decider_only`. Zero in the default arm,
//! positive under `FCAST_INLINE_TEXT_POLL` or `FCAST_INLINE_DISPATCH`. That
//! A/B keeps the zero from being vacuous.
//!
//! A second test covering a postponed disposal drained with a link pending
//! behind it was removed. That interleaving does not exist on the consumer
//! transport, where a branch is disposed of inline even at a resting PAUSED.
//! The arm's own contract on that event lives in `sink_subtitles`.
//!
//! # Verification
//!
//! * Green: no env vars (strict).
//! * `FCAST_INLINE_DISPATCH=1` or `FCAST_INLINE_TEXT_POLL=1`: flips to
//!   demanding a non-zero count.

use std::{
    sync::{Arc, mpsc},
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

#[path = "support/text_arm.rs"]
mod text_arm;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

/// The levers that put text-branch surgery back on a foreign thread. Kept in
/// sync with `Inner::text_ownership_levered`. Under any of them the
/// ownership claim is suspended by request, so the assertions become
/// observations.
const OWNERSHIP_LEVERS: [&str; 6] = [
    "FCAST_INLINE_TEXT_POLL",
    "FCAST_INLINE_ROUTE_TEXT_POLL",
    "FCAST_INLINE_DISPATCH",
    "FCAST_INLINE_REPLAY_OUTCOME",
    "FCAST_INLINE_VIDEO_CHAIN_TEARDOWN",
    // The v1 replay loop it restores runs the whole outcome tail on
    // `fpb-replay`, and that tail pokes the text policy.
    "FCAST_NO_HANDS",
];

/// The two levers whose surgery this schedule is guaranteed to reach, so a
/// levered run can assert a positive count rather than merely tolerate one.
/// `FCAST_INLINE_TEXT_POLL` puts the whole policy on the pumping caller, and
/// `FCAST_INLINE_DISPATCH` puts the eager park there. The other levers move
/// paths this schedule does not exercise, so they get no verdict here.
const LEVERS_THIS_SCHEDULE_MOVES: [&str; 2] = ["FCAST_INLINE_TEXT_POLL", "FCAST_INLINE_DISPATCH"];

fn levered() -> Vec<&'static str> {
    OWNERSHIP_LEVERS
        .into_iter()
        .filter(|lever| std::env::var_os(lever).is_some())
        .collect()
}

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
    /// Every cue payload that reaches the renderer, on whichever transport,
    /// from [`Harness::tap_text`].
    text: std::cell::RefCell<Option<text_arm::CueTap>>,
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
        // The cue feed is established before anything can flow. An unsynced
        // external hands its whole file over the moment its branch links,
        // and the tap backfills out of the feed rather than starting empty.
        text_arm::arm(&playbin);
        Self {
            playbin: Arc::new(playbin),
            events,
            video,
            paused: std::cell::Cell::new(false),
            text: std::cell::RefCell::new(None),
        }
    }

    /// Start reading cue payloads. Called once the branch is linked. Nothing
    /// is lost by the wait because [`text_arm::tap_cue_payloads`] backfills
    /// what the feed already took.
    fn tap_text(&self) {
        *self.text.borrow_mut() = Some(text_arm::tap_cue_payloads(&self.playbin));
    }

    /// The receiver's gate, minus the state machine it does not have here.
    /// The test item never buffers and is unseekable, so the only honest
    /// variable left is `paused`.
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
                "timed out waiting for {what}; video buffers {}, \
                 surgeries off the decider {}, pending disposals {}, text tail peers {:?}",
                self.video.buffer_count(),
                self.playbin.text_surgery_off_decider(),
                self.playbin.pending_text_disposals(),
                self.text_tail_peers(),
            );
            self.pump();
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// What is wired into the text renderer right now, for a failure message.
    fn text_tail_peers(&self) -> Vec<String> {
        text_arm::text_tail_pads(&self.playbin)
            .iter()
            .filter_map(|pad| pad.peer())
            .map(|peer| peer.name().to_string())
            .collect()
    }

    /// Cues that reached the renderer carrying `tag`, so a cue proves the
    /// branch DELIVERS rather than merely being linked.
    fn cues_seen(&self, tag: &str) -> usize {
        self.text
            .borrow()
            .as_ref()
            .expect("the cue tap is installed")
            .lock()
            .expect("text tap")
            .iter()
            .filter(|(payload, _)| payload.trim_start().starts_with(tag))
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

/// Load, play, attach the external subtitle input and select it. Returns the
/// harness with a live text branch and at least one cue seen.
fn play_with_external_subtitles(
    tag: &str,
) -> (
    Harness,
    fcasttest::scenario::ScenarioHandle,
    fcasttest::scenario::ScenarioHandle,
    fcastplaybin::ExternalSubId,
) {
    let media = ScenarioBuilder::new(&format!("{tag}main"))
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(120))
        .pacing(Pacing::Realtime)
        .register();
    // Dense cues pushed as fast as possible, so the branch has data the
    // instant it links and a stalled branch is unambiguous.
    let subs = ScenarioBuilder::new(&format!("{tag}subs"))
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
    harness.wait_for("the text branch to join the renderer", || {
        text_arm::text_branch_linked(&harness.playbin)
    });
    harness.tap_text();
    harness.wait_for("the first cue to reach the renderer", || {
        harness.cues_seen("S") > 0
    });
    (harness, media, subs, id)
}

/// Every text-branch surgery ran on the deciding thread.
///
/// The schedule reaches all three assertion sites of `Inner::decider_only`:
/// the policy's link loop (selecting the external), the eager park
/// (subtitles off), and the postponed-disposal drain (off while PAUSED, then
/// PLAYING). In a debug build a violation panics inside the crate. The
/// counter is what makes the levered arm readable.
#[test]
fn text_branch_surgery_stays_on_the_deciding_thread() {
    init();
    let (harness, media, subs, id) = play_with_external_subtitles("ownership");

    // (1) the eager park, and (2) a disposal postponed onto the worker.
    // `detach_text_parts` defers its blocking half at a pipeline resting in
    // PAUSED.
    harness.set_paused(true);
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    harness.pump();
    harness.wait_for("the text branch to leave the renderer", || {
        !text_arm::text_branch_linked(&harness.playbin)
    });

    // (3) the drain disposes at a settled PLAYING, and (4) the link loop runs
    // again for the re-selected external.
    harness.set_paused(false);
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    let seen = harness.cues_seen("S");
    harness.wait_for("the re-selected external to deliver a cue again", || {
        harness.cues_seen("S") > seen
    });
    harness.wait_for("the postponed disposal to drain", || {
        harness.playbin.pending_text_disposals() == 0
    });

    let off_decider = harness.playbin.text_surgery_off_decider();
    let levers = levered();
    if levers.is_empty() {
        assert_eq!(
            off_decider, 0,
            "text-branch surgery ran on a thread other than the decider {off_decider} time(s)"
        );
    } else if levers
        .iter()
        .any(|lever| LEVERS_THIS_SCHEDULE_MOVES.contains(lever))
    {
        assert!(
            off_decider > 0,
            "{levers:?} restore the v1 threading for work this schedule performs, so the \
             ownership counter must see it; it stayed at {off_decider}, which would mean the \
             assertions are wired to nothing"
        );
        println!("levers {levers:?}: {off_decider} surgeries off the decider, as expected");
    } else {
        println!(
            "NO VERDICT: {levers:?} move no surgery this schedule reaches \
             (count {off_decider})"
        );
    }

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}
