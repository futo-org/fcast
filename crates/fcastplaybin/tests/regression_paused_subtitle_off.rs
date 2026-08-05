//! Regression for the negotiation error raised when subtitles are turned off
//! while the pipeline is paused and playback is then resumed.
//!
//! Reported from the field:
//!
//! ```text
//! 90.523  text stream joined subtitleoverlay pad=text_2   (state=paused)
//! 96.458  Selecting track id=-1 (subtitles off)
//! 96.458  postponing a text branch disposal: the pipeline is at rest in PAUSED
//! 98.102  ResumeOrPause -> SetState Playing
//! 98.103  disposing of a text branch postponed while paused
//! 98.116  WARN subtitleoverlay: Subtitle sink is blocked but we have no
//!         subtitle caps
//! 98.118  WARN Media warning: GStreamer error: negotiation problem.
//! ```
//!
//! The first line is the load-bearing one, and it is the line the first
//! version of this regression test missed. In the field the text branch
//! JOINED the overlay while the pipeline was already at rest in PAUSED.
//! That ordering leaves subtitleoverlay stalled mid-reconfiguration, and
//! only from that stalled state does the resume raise the warning.
//!
//! The mechanism, from gstsubtitleoverlay.c (with the receiver's patches):
//!
//! * Linking the branch while paused delivers the subtitle CAPS event into
//!   an armed block probe. The pushing thread parks there and
//!   `subtitle_sink_blocked` becomes TRUE (`_pad_blocked_cb`). The overlay
//!   then wants a video block to run the reconfiguration, but the prerolled
//!   video buffer's push is still in flight through the overlay into
//!   `gst_base_sink_wait_preroll`, so neither a data-driven block nor the
//!   patched one-shot IDLE block can fire while the pipeline rests in
//!   PAUSED. The reconfiguration stalls with the flag TRUE.
//! * Turning subtitles off unlinks the branch inline
//!   (`Inner::detach_text_parts`). The overlay's unlink handler clears
//!   `self->subcaps` synchronously. The flag stays TRUE and the parked
//!   push stays parked, because the postponed disposal flush can no longer
//!   reach it through the severed branch.
//! * Resuming releases `wait_preroll`, the video block fires, and
//!   `_pad_blocked_cb` runs with both pads blocked, `subcaps` NULL and the
//!   subtitle peer gone. That is exactly the condition for its
//!   `GST_ELEMENT_WARNING (self, CORE, NEGOTIATION, ...)`, which the
//!   receiver surfaces to the user as "negotiation problem".
//!
//! The control test keeps the OTHER ordering green. Selecting while
//! PLAYING lets the reconfiguration complete, `subtitle_sink_blocked` is
//! FALSE at the off, and the resume is clean. That contrast pins the
//! trigger to the stalled reconfiguration, not to paused subtitle-off in
//! general.

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, ExternalSubId, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks,
    StartPoint, TrackSlot, TrackTarget,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::FTestSink,
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

/// Long enough for the overlay to raise its complaint, which arrived 15 ms
/// after the resume in the field capture.
const SETTLE_AFTER_RESUME: Duration = Duration::from_secs(3);

/// How long the reproduction watches the linked-but-stalled overlay input
/// before moving on. The stall is permanent while paused, so this only
/// needs to outlast scheduling noise.
const STALL_OBSERVATION: Duration = Duration::from_millis(1500);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init().expect("registering fcastaudiostretch");
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

/// A playbin plus the warning/error text it surfaced, which is what the
/// receiver shows the user.
struct Rig {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    complaints: Arc<Mutex<Vec<String>>>,
}

impl Rig {
    fn new() -> Self {
        let playbin = Arc::new(
            FcastPlaybin::new(Sinks {
                video: Some(FTestSink::new().upcast()),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
            })
            .expect("building fcastplaybin"),
        );
        let complaints: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = complaints.clone();
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            match &event {
                PlaybinEvent::Warning(text) => sink.lock().expect("complaints").push(text.clone()),
                PlaybinEvent::Error { error, .. } => sink
                    .lock()
                    .expect("complaints")
                    .push(format!("error: {error}")),
                _ => {}
            }
            let _ = tx.send(event);
        });
        Self {
            playbin,
            events,
            complaints,
        }
    }

    /// The receiver's settle-point calls plus an event drain.
    fn drain(&self, paused: bool) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(gate(paused));
        while self.events.try_recv().is_ok() {}
    }

    fn wait_for(&self, what: &str, paused: bool, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            self.drain(paused);
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn load(&self, uri: &str) {
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
                loaded |= matches!(event, PlaybinEvent::Loaded { .. });
            }
            thread::sleep(Duration::from_millis(10));
        }
    }

    fn wait_settled(&self, state: gst::State, paused: bool) {
        self.wait_for(&format!("the pipeline to settle at {state:?}"), paused, || {
            let (_, current, pending) = self.playbin.pipeline().state(gst::ClockTime::ZERO);
            current == state && pending == gst::State::VoidPending
        });
    }

    fn attach(&self, uri: &str) -> ExternalSubId {
        let id = self.playbin.attach_subtitle(uri).expect("attach");
        self.wait_for("the external subtitle to materialize", false, || {
            !self.playbin.subtitle_stream_ids(id).is_empty()
        });
        id
    }

    fn overlay_subtitle_pad(&self) -> gst::Pad {
        self.playbin
            .pipeline()
            .by_name("fpb-suboverlay")
            .and_then(|overlay| overlay.static_pad("subtitle_sink"))
            .expect("the overlay's subtitle_sink")
    }

    /// Resume, settle at PLAYING, and give the overlay the window in which
    /// the field warning arrived. Returns everything surfaced in that
    /// window that mentions negotiation.
    fn resume_and_collect_negotiation_complaints(&self) -> (Vec<String>, Vec<String>) {
        self.complaints.lock().expect("complaints").clear();
        self.playbin.play().expect("resume");
        self.wait_settled(gst::State::Playing, false);
        let settle = Instant::now() + SETTLE_AFTER_RESUME;
        while Instant::now() < settle {
            self.drain(false);
            thread::sleep(Duration::from_millis(20));
        }
        let raised = self.complaints.lock().expect("complaints").clone();
        let negotiation = raised
            .iter()
            .filter(|text| text.to_lowercase().contains("negotiation"))
            .cloned()
            .collect();
        (negotiation, raised)
    }

    fn shutdown(&self) {
        let (done_tx, done_rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = done_tx.send(());
        }));
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match done_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(()) => break,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the shutdown never finished");
                    self.playbin.pump_selection(gate(false));
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

/// The FIELD ordering. The external subtitle is selected while the pipeline
/// is already at rest in PAUSED, so subtitleoverlay's reconfiguration
/// stalls with `subtitle_sink_blocked` TRUE, then subtitles go off while
/// still paused, then playback resumes. The resume must not surface a
/// negotiation problem to the user.
#[test]
fn selecting_while_paused_then_off_then_resume_raises_no_negotiation_error() {
    init();
    let media = ScenarioBuilder::new("pausedselmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    let subs = ScenarioBuilder::new("pausedselsubs")
        .text("text_0", cues(200, gst::ClockTime::from_mseconds(100)))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let rig = Rig::new();
    rig.load(&media.uri());
    rig.playbin.play().expect("play");
    rig.wait_settled(gst::State::Playing, false);

    let id = rig.attach(&subs.uri());

    // Come to rest in PAUSED with video prerolled through the overlay.
    // From here on the video path holds a push parked in the sink's
    // preroll wait, which is what keeps the overlay's video block from
    // firing while paused.
    rig.playbin.pause().expect("pause");
    rig.wait_settled(gst::State::Paused, true);

    // Select the external subtitle WHILE PAUSED. This is the field's
    // "text stream joined subtitleoverlay" at state=paused.
    rig.playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    rig.wait_for("the subtitle branch to reach the overlay", true, || {
        rig.playbin
            .pipeline()
            .by_name("fpb-suboverlay")
            .and_then(|overlay| overlay.static_pad("subtitle_sink"))
            .is_some_and(|pad| pad.is_linked())
    });
    let overlay_subtitle = rig.overlay_subtitle_pad();

    // The stalled-reconfiguration precondition. The branch's CAPS event is
    // parked in the overlay's subtitle block probe, so it never completes
    // its trip through the ghost pad and no sticky caps appear on
    // subtitle_sink for as long as the pipeline rests in PAUSED. If caps
    // DO appear the overlay completed its reconfiguration while paused and
    // this test is not exercising the field state, so say that instead of
    // reporting a pass that proved nothing.
    let observe_until = Instant::now() + STALL_OBSERVATION;
    let mut caps_appeared = None;
    while Instant::now() < observe_until {
        if let Some(caps) = overlay_subtitle.current_caps() {
            caps_appeared = Some(caps);
            break;
        }
        rig.drain(true);
        thread::sleep(Duration::from_millis(20));
    }
    assert!(
        caps_appeared.is_none(),
        "subtitleoverlay completed its reconfiguration while the pipeline was at rest in \
         PAUSED (subtitle_sink stored caps {caps_appeared:?}), so this run never entered the \
         stalled state the field bug needs. The reproduction premise does not hold in this \
         environment."
    );

    // The branch queue, grabbed while the link still exists, so the
    // postponement of its disposal is observable after the off.
    let tqueue = overlay_subtitle
        .peer()
        .and_then(|peer| peer.parent_element())
        .expect("the overlay's subtitle feed queue");

    // Subtitles off, still at rest in PAUSED. This is the receiver's
    // "Selecting track id=-1". The unlink runs inline and the disposal is
    // postponed.
    rig.playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    rig.playbin.pump_selection(gate(true));
    rig.wait_for("the branch to unlink from the overlay", true, || {
        !overlay_subtitle.is_linked()
    });
    assert!(
        tqueue.parent().is_some(),
        "the text branch queue left the pipeline while paused, so the disposal ran inline \
         instead of being postponed and this test is not on the postponed-disposal path"
    );

    let (negotiation, raised) = rig.resume_and_collect_negotiation_complaints();
    assert!(
        negotiation.is_empty(),
        "resuming after subtitles were selected and then turned off at rest in PAUSED \
         surfaced a negotiation problem to the user. subtitleoverlay was left blocked on a \
         subtitle sink whose caps were cleared by the unlink and whose branch is gone: \
         {negotiation:?} (all complaints: {raised:?})"
    );

    rig.shutdown();
    media.unregister();
    subs.unregister();
}

/// The CONTROL ordering. The subtitle is selected while PLAYING, so the
/// overlay's reconfiguration completes before the pause, and the off plus
/// resume is clean. This stays green with and without the disposal
/// deferral, which the first version of this file learned the hard way, so
/// it guarantees nothing about the field bug. It pins the contrast that
/// makes the reproduction above a statement about the STALLED overlay
/// rather than about paused subtitle-off in general.
#[test]
fn control_off_while_paused_after_selecting_while_playing_is_clean() {
    init();
    let media = ScenarioBuilder::new("pausedoffmain")
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::Realtime)
        .register();
    let subs = ScenarioBuilder::new("pausedoffsubs")
        .text("text_0", cues(200, gst::ClockTime::from_mseconds(100)))
        .duration(gst::ClockTime::from_seconds(30))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let rig = Rig::new();
    rig.load(&media.uri());
    rig.playbin.play().expect("play");

    let id = rig.attach(&subs.uri());
    rig.playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    rig.wait_for("the subtitle branch to reach the overlay", false, || {
        rig.playbin
            .pipeline()
            .by_name("fpb-suboverlay")
            .and_then(|overlay| overlay.static_pad("subtitle_sink"))
            .is_some_and(|pad| pad.is_linked())
    });
    let overlay_subtitle = rig.overlay_subtitle_pad();

    // Text must actually be flowing, so the overlay finished its renderer
    // reconfiguration and is mid-render rather than stalled when the
    // branch is pulled out from under it.
    let seen = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counter = seen.clone();
    overlay_subtitle
        .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
            counter.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            gst::PadProbeReturn::Ok
        })
        .expect("counting text into the overlay");
    rig.wait_for("text to flow into the overlay", false, || {
        seen.load(std::sync::atomic::Ordering::SeqCst) >= 2
    });

    rig.playbin.pause().expect("pause");
    rig.wait_settled(gst::State::Paused, true);

    rig.playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    rig.playbin.pump_selection(gate(true));

    let (negotiation, raised) = rig.resume_and_collect_negotiation_complaints();
    assert!(
        negotiation.is_empty(),
        "the control ordering (select while playing, off while paused, resume) surfaced a \
         negotiation problem, so the field bug no longer needs the stalled reconfiguration \
         and the reproduction test's premise should be revisited: {negotiation:?} \
         (all complaints: {raised:?})"
    );

    rig.shutdown();
    media.unregister();
    subs.unregister();
}
