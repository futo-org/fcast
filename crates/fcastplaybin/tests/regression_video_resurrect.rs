//! RED regression test for the resurrected-video wedge, found by
//! `fuzz_buffering` seed 300002 and reduced here to a deterministic sequence.
//!
//! The shape. With video explicitly DESELECTED (chain parked at READY and
//! removed), attaching an external subtitle input makes decodebin3 post a
//! fresh collection and auto-select its defaults, which resurrects the video
//! stream as a brand-new src pad. `route_db3_pad` rebuilds the video chain
//! unconditionally, so the pipeline drops into an async re-preroll for a sink
//! whose stream has nothing left to deliver (here a held stall gate, in the
//! field a drained or live-edge stream). The selection engine SEES the
//! divergence and sets `dirty`, but the corrective re-assert only dispatches
//! from `pump` under `gate.quiet`, and the receiver derives quiet from
//! `running && !has_async_transition()`, which the unfinished re-preroll
//! holds false forever. The postponed re-assert is the only thing that would
//! unblock the re-preroll it is postponed behind, and the pipeline rests at
//! (Paused, pending Paused) for good while the receiver reports Buffering
//! and refuses transport work.
//!
//! `fuzz_scenarios` could never reach this because it pumps with
//! `quiet: true` unconditionally, which lets the re-assert dispatch mid
//! transition and self-heal the wedge. The two passing CONTROLS live in
//! `tests/video_resurrect_controls.rs` (their own binary, because this
//! red test ends in a process exit): one proves the attach is harmless
//! while video stays selected, the other proves the ungated pump
//! self-heals, so the pump gate is established as the load-bearing half
//! of the bug, next to the unconditional chain rebuild.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{
    scenario::{ScenarioBuilder, ScenarioHandle},
    sink::FTestSink,
    spec::{CueSpec, Fault, Pacing, StreamKind, StreamSpec},
};
use gst::prelude::*;

const BOUND: Duration = Duration::from_secs(15);
/// How long the settle assertion waits after the attach. Generous, the
/// healthy path settles in well under a second.
const SETTLE_BOUND: Duration = Duration::from_secs(12);
const GATE: &str = "starve";
/// Where the video stream parks. Past preroll and the first rendered
/// frames, so the wedge is a mid-playback state and not a load artifact.
const STALL_AT: u64 = 30;

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

/// AV media whose video stream parks on a held sync point at [`STALL_AT`],
/// so no video data can arrive after that, deterministically. Audio flows on
/// (ftestsrc runs one task per stream with no flow combiner).
fn starved_video_media(key: &str) -> ScenarioHandle {
    let video = StreamSpec::new(
        "video_0",
        StreamKind::Video {
            width: 16,
            height: 16,
            fps: gst::Fraction::new(25, 1),
            keyframe_interval: 1,
        },
    )
    .with_fault(Fault::StallAt {
        buffer_index: STALL_AT,
        sync_point: GATE.to_owned(),
    });
    ScenarioBuilder::new(key)
        .stream(video)
        .stream(StreamSpec::audio("audio_0"))
        .duration(gst::ClockTime::from_seconds(8))
        .pacing(Pacing::Realtime)
        .register()
}

fn external_text(key: &str) -> ScenarioHandle {
    let cues = (0..40)
        .map(|index| {
            let start = gst::ClockTime::from_mseconds(200) * (index + 1);
            CueSpec::new(
                start,
                start + gst::ClockTime::from_mseconds(100),
                format!("EXT{index:02}"),
            )
        })
        .collect();
    ScenarioBuilder::new(key)
        .text("text_0", cues)
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register()
}

/// Releases every sync point of its scenarios when dropped, so a panicking
/// assertion (the red one especially) cannot leave a parked source push to
/// wedge the unwinding test's pipeline teardown.
struct ReleaseOnDrop(Vec<ScenarioHandle>);

impl Drop for ReleaseOnDrop {
    fn drop(&mut self) {
        for handle in &self.0 {
            handle.release_all();
        }
    }
}

struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<PlaybinEvent>,
    /// Latest StreamsSelected triple.
    selected: std::cell::RefCell<Option<(Option<String>, Option<String>, Option<String>)>>,
    loaded: std::cell::Cell<bool>,
    /// Pump with the receiver's gate (false) or the fuzz driver's
    /// unconditional quiet (true).
    ungated: bool,
}

impl Harness {
    fn new(ungated: bool) -> Self {
        let video_sink = FTestSink::new();
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
            playbin,
            events,
            selected: std::cell::RefCell::new(None),
            loaded: std::cell::Cell::new(false),
            ungated,
        }
    }

    /// The receiver's gate, approximated from the pipeline alone. The
    /// receiver computes `quiet = running && !has_async_transition()`, and
    /// in this test the machine is running iff the pipeline rests settled in
    /// PLAYING (nothing here ever pauses on purpose).
    fn pump(&self) {
        self.playbin.poll_text_policy();
        let (current, pending) = self.playbin.state_summary();
        let quiet = self.ungated
            || (current == gst::State::Playing
                && pending == gst::State::VoidPending
                && !self.playbin.has_async_transition());
        self.playbin.pump_selection(SelectionGate {
            quiet,
            paused: false,
            seekable: false,
        });
        while let Ok(event) = self.events.try_recv() {
            match event {
                PlaybinEvent::StreamsSelected {
                    video,
                    audio,
                    subtitle,
                    ..
                } => *self.selected.borrow_mut() = Some((video, audio, subtitle)),
                PlaybinEvent::Loaded { .. } => self.loaded.set(true),
                PlaybinEvent::Error { error, .. } => panic!("pipeline error: {error}"),
                _ => {}
            }
        }
    }

    fn wait_until(&self, what: &str, mut done: impl FnMut(&Self) -> bool) {
        let deadline = Instant::now() + BOUND;
        loop {
            self.pump();
            if done(self) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what} (pipeline {:?}, selected {:?})",
                self.playbin.state_summary(),
                self.selected.borrow()
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(50)) {
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

/// Play to a settled PLAYING, park the video stream on its gate, deselect
/// video and confirm it, then attach an external subtitle. Returns after
/// the attach's stream materialized in a collection.
fn drive_to_attach(harness: &Harness, media: &ScenarioHandle, external: &ScenarioHandle) {
    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_until("the load to report Loaded", |h| h.loaded.get());
    // The receiver's post-load transport commit.
    harness.playbin.play().expect("play after the load");
    harness.wait_until("a settled PLAYING", |h| {
        h.playbin.state_summary() == (gst::State::Playing, gst::State::VoidPending)
    });
    // The video stream must be provably OUT of data before anything else,
    // otherwise the re-preroll under test could complete by luck.
    assert!(
        media.sync_point(GATE).wait_for_arrival(BOUND),
        "the video stream never reached its stall gate"
    );

    harness
        .playbin
        .request_track(TrackSlot::Video, TrackTarget::Stream(None));
    harness.wait_until("the video deselect to confirm", |h| {
        matches!(&*h.selected.borrow(), Some((None, Some(_), _)))
    });
    // Let the parked chain's removal (Job::VideoChainGone) run its course.
    let removal = Instant::now() + Duration::from_millis(500);
    while Instant::now() < removal {
        harness.pump();
        std::thread::sleep(Duration::from_millis(10));
    }

    let id = harness
        .playbin
        .attach_subtitle(&external.uri())
        .expect("attaching the external subtitle");
    harness.wait_until("the external stream to materialize", |h| {
        !h.playbin.subtitle_stream_ids(id).is_empty()
    });
}

/// The bug. After the attach, decodebin3's collection-default auto-select
/// resurrects the deselected video stream, the crate rebuilds the video
/// chain, and the pipeline must still come back to a settled state with the
/// desired (video off) selection re-asserted. Today it never does: it rests
/// at (Paused, pending Paused) forever behind a video preroll no data can
/// finish, because the corrective re-assert is gated on the very settle it
/// would produce.
#[test]
fn attaching_a_subtitle_with_video_deselected_must_settle_again() {
    init();
    let media = starved_video_media("vresmain");
    let external = external_text("vresext");
    // Declared BEFORE the guard so the unwind releases the gates first and
    // the playbin's teardown never waits on a parked source push.
    let harness = Harness::new(false);
    let _guard = ReleaseOnDrop(vec![media.clone(), external.clone()]);

    drive_to_attach(&harness, &media, &external);

    let deadline = Instant::now() + SETTLE_BOUND;
    loop {
        harness.pump();
        let (current, pending) = harness.playbin.state_summary();
        let settled = pending == gst::State::VoidPending && !harness.playbin.has_async_transition();
        let video_off = matches!(&*harness.selected.borrow(), Some((None, Some(_), _)));
        if settled && video_off && current == gst::State::Playing {
            break;
        }
        if Instant::now() >= deadline {
            eprintln!(
                "FAILED: the pipeline never settled again after the attach \
                 resurrected the deselected video stream: pipeline {:?}, selected \
                 {:?}. The engine noted the divergence but its re-assert is \
                 pump-gated on a quiet the unfinished video re-preroll prevents, \
                 so the wedge is permanent.",
                harness.playbin.state_summary(),
                harness.selected.borrow()
            );
            // A hard exit instead of a panic. Unwinding drops the playbin at
            // a pipeline resting below PLAYING with live sources, which hits
            // the separate teardown wedge this campaign also found (a source
            // push re-blocks into decodebin3's multiqueue between the drop's
            // flush PAIR and the input NULL, and pad deactivation then waits
            // on the stream lock that push holds). Exiting keeps this red
            // gate from hanging the suite on the bug it reports.
            std::process::exit(101);
        }
        std::thread::sleep(Duration::from_millis(10));
    }

    media.release_all();
    harness.shutdown();
    media.unregister();
    external.unregister();
}
