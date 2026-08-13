//! Disabling VIDEO while the pipeline rests in PAUSED must still confirm.
//!
//! The FAST case `video_disable_while_paused_v4` on a matroska item with one
//! video, one audio and three text streams: at a settled PAUSED the caller asks
//! for video off, the crate's text-without-video guard drops the subtitle out
//! of the selection too, the branch is parked and its disposal POSTPONED
//! (nothing blocking may run on the caller at rest in PAUSED), the
//! `SELECT_STREAMS` goes out with the audio id only, and then nothing happens
//! for 18 s: decodebin3 never posts `STREAMS_SELECTED` for that seqnum, so the
//! receiver never confirms the track change.
//!
//! # Mechanism (pinned from the decodebin3 and basesink logs plus the source)
//!
//! 1. The crate sends `SELECT_STREAMS` with the audio id only.
//! 2. `handle_stream_switch` finds both the video and the text slot need
//!    deactivating with no alternative to reassign to, and arms an IDLE probe
//!    on each slot's src pad to run `mq_slot_unassign`
//!    (gstdecodebin3.c:4518-4571).
//! 3. The TEXT slot's probe fires at once: the crate has already parked,
//!    unlinked and DROP-probed that branch, so no push is in flight.
//! 4. The VIDEO slot's does NOT. Its multiqueue task is inside `gst_pad_push`
//!    -> decoder -> subtitleoverlay -> video sink, parked in
//!    `gst_base_sink_wait_preroll`, which gstbasesink.c:2438 says exits only on
//!    "flush or PLAYING". A pad mid-push is never idle, so the probe never
//!    fires and `mq_slot_unassign` never runs.
//! 5. `is_selection_done` therefore keeps bailing out with "Stream from
//!    previous selection still active" (:3358) and posts NOTHING. Measured: the
//!    video slot's `mq_slot_reassign` ran 16 s late, at teardown, when the
//!    teardown flush finally released the parked push.
//!
//! The crate's video chain is the caller's video sink alone, with no
//! decoupling queue, while audio has `fpb-aqueue` to absorb its slot's pushes.
//! That is why only VIDEO deselects wedge, and it is also why the `ftestsrc`
//! variant below never reproduced it: `Pacing::Realtime` stops producing at
//! PAUSED, so the multiqueue video queue drains and the slot task sits in
//! `gst_data_queue_pop`, which leaves the pad idle. The real file keeps the
//! demuxer far enough ahead that there is essentially always a queued buffer
//! whose push is parked in the sink.
//!
//! The fix is `Inner::lift_deselected_video_sink`: take the PLAYING exit rather
//! than the flush one (a flush returns FLUSHING into the multiqueue and latches
//! it, which is precisely what the reverted READY descent did). Lever
//! `FCAST_NO_PAUSED_DESELECT_SINK_LIFT` restores the stall. With the fix the
//! video slot reassigns 28 ms after the SELECT_STREAMS instead of never.
//!
//! # Reproduction rate
//!
//! Bare, this failed 3 of 8 runs: it needs a video push to be parked at the
//! instant the selection is dispatched. `GST_DEBUG=decodebin3:6` makes it
//! deterministic (5 of 5 before the fix, 0 of 3 after), which is the cheapest
//! way to re-check this test's premise if it is ever suspected of going green
//! for the wrong reason.
//!
//! Pre-existing, not a regression from the disposal/seat/refresh work: it
//! failed at the same rate with every lever of that work set to its old
//! behaviour (measured 2 of 8).

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

#[path = "support/text_arm.rs"]
mod text_arm;

const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

/// FAST gives the receiver 16 s to settle a track change, so anything the
/// receiver would call a hang has to be inside this.
const CONFIRM_BOUND: Duration = Duration::from_secs(16);

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

fn cues(count: u32, step: gst::ClockTime) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("C{index:02}"))
        })
        .collect()
}

fn gate(paused: bool) -> SelectionGate {
    SelectionGate {
        quiet: true,
        paused,
        seekable: false,
    }
}

struct Rig {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    /// Every selection decodebin3 confirmed, as (video, audio, subtitle).
    selected: Arc<Mutex<Vec<(Option<String>, Option<String>, Option<String>)>>>,
    video: fcasttest::sink::Recording,
}

impl Rig {
    fn new() -> Self {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let playbin = Arc::new(
            FcastPlaybin::new(Sinks {
                video: Some(video_sink.upcast()),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
            })
            .expect("building fcastplaybin"),
        );
        let selected: Arc<Mutex<Vec<_>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = selected.clone();
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            if let PlaybinEvent::StreamsSelected {
                video,
                audio,
                subtitle,
                ..
            } = &event
            {
                sink.lock().expect("selections").push((
                    video.clone(),
                    audio.clone(),
                    subtitle.clone(),
                ));
            }
            let _ = tx.send(event);
        });
        Self {
            playbin,
            events,
            selected,
            video,
        }
    }

    fn drain(&self, paused: bool) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(gate(paused));
        while let Ok(event) = self.events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error: {error}");
            }
        }
    }

    fn wait_for(&self, what: &str, paused: bool, bound: Duration, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + bound;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; pipeline {:?}, unsettled {:?}, \
                 selections {:?}, video buffers {}",
                self.playbin.state_summary(),
                self.playbin.unsettled_elements(),
                self.selected.lock().expect("selections"),
                self.video.buffer_count(),
            );
            self.drain(paused);
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn load_and_play(&self, uri: &str) {
        self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut loaded = false;
        while !loaded {
            assert!(Instant::now() < deadline, "the load never finished");
            self.playbin.poll_text_policy();
            self.playbin.pump_selection(gate(false));
            while let Ok(event) = self.events.try_recv() {
                if let PlaybinEvent::Error { error, .. } = &event {
                    panic!("pipeline error during the load: {error}");
                }
                loaded |= matches!(event, PlaybinEvent::Loaded { .. });
            }
            thread::sleep(Duration::from_millis(10));
        }
        self.playbin.play().expect("play");
        self.wait_settled(gst::State::Playing, false);
    }

    fn wait_settled(&self, state: gst::State, paused: bool) {
        self.wait_for(
            &format!("the pipeline to settle at {state:?}"),
            paused,
            EVENT_TIMEOUT,
            || self.playbin.state_summary() == (state, gst::State::VoidPending),
        );
    }

    fn video_disabled_confirmed(&self) -> bool {
        self.selected
            .lock()
            .expect("selections")
            .iter()
            .any(|(video, ..)| video.is_none())
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
                    self.drain(false);
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

/// The FAST case's own media: `video_with_subs.mkv` (1 video, 1 audio, 3 text)
/// through the real matroskademux/parsebin/decoder topology. `ftestsrc` pushes
/// every elementary stream from its own task, which is NOT how a demuxer feeds
/// decodebin3, and the deactivation this test is about is a multiqueue-level
/// property, so the container matters here.
///
/// Skipped, loudly, when the sample tree is not checked out next to the repo.
fn sample_media() -> Option<String> {
    let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../../../fcast-sample-media/video/video_with_subs.mkv");
    let path = path.canonicalize().ok()?;
    path.is_file().then(|| format!("file://{}", path.display()))
}

#[test]
fn disabling_video_while_paused_confirms_the_track_change_on_matroska() {
    init();
    let Some(uri) = sample_media() else {
        eprintln!("skipping: ../fcast-sample-media/video/video_with_subs.mkv is not present");
        return;
    };

    let rig = Rig::new();
    rig.load_and_play(&uri);
    rig.wait_for(
        "the text branch to join the renderer",
        false,
        EVENT_TIMEOUT,
        || text_arm::text_branch_linked(&rig.playbin),
    );
    rig.wait_for("video to render", false, EVENT_TIMEOUT, || {
        rig.video.buffer_count() >= 5
    });

    rig.playbin.pause().expect("pause");
    rig.wait_settled(gst::State::Paused, true);

    rig.playbin
        .request_track(TrackSlot::Video, TrackTarget::Stream(None));
    rig.drain(true);
    rig.wait_for(
        "decodebin3 to confirm the video disable while paused",
        true,
        CONFIRM_BOUND,
        || rig.video_disabled_confirmed(),
    );

    rig.shutdown();
}

/// The FAST sequence: play, pause, video off. The confirmation must arrive
/// while the pipeline stays paused.
#[test]
fn disabling_video_while_paused_confirms_the_track_change() {
    init();
    let media = ScenarioBuilder::new("pausedvidoff")
        .video("video_0")
        .audio("audio_0")
        .text("text_0", cues(200, gst::ClockTime::from_mseconds(500)))
        .duration(gst::ClockTime::from_seconds(60))
        .pacing(Pacing::Realtime)
        .register();

    let rig = Rig::new();
    rig.load_and_play(&media.uri());
    // Real playback with the text branch live, so the disable below detaches a
    // branch that is being fed (the FAST case's state).
    rig.wait_for(
        "the text branch to join the renderer",
        false,
        EVENT_TIMEOUT,
        || text_arm::text_branch_linked(&rig.playbin),
    );
    rig.wait_for("video to render", false, EVENT_TIMEOUT, || {
        rig.video.buffer_count() >= 5
    });

    rig.playbin.pause().expect("pause");
    rig.wait_settled(gst::State::Paused, true);

    // Video off at rest in PAUSED. The crate's text-without-video guard drags
    // the subtitle out of the selection with it.
    rig.playbin
        .request_track(TrackSlot::Video, TrackTarget::Stream(None));
    rig.drain(true);
    rig.wait_for(
        "decodebin3 to confirm the video disable while paused",
        true,
        CONFIRM_BOUND,
        || rig.video_disabled_confirmed(),
    );

    media.release_all();
    rig.shutdown();
    media.unregister();
}
