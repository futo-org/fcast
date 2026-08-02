//! Integration tests: disabling subtitles takes effect immediately.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use gst::prelude::*;

/// Generous bound for real-time playback on a busy CI box.
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

/// Max time for a pumped disable to unlink the branch and confirm the
/// deselect. A healthy eager detach needs milliseconds.
const DISABLE_BOUND: Duration = Duration::from_secs(2);

/// Max time the rendered cue may keep appearing on video buffers after
/// the disable. The historical bug detached only on decodebin3's pad
/// removal, which waits out the current cue (~400-450ms here). An eager
/// detach clears within a frame or two.
const CUE_CLEAR_BOUND: Duration = Duration::from_millis(250);

/// Dense back-to-back cues (every 500ms from 0.5s to 7.5s). Density
/// matters: decodebin3 holds sparse text until the interleave covers the
/// cue gap, and a steady cue stream makes a broken disable visible.
fn srt_content() -> String {
    let mut srt = String::new();
    for i in 0..14u32 {
        let start = 500 + i * 500;
        let end = start + 450;
        srt.push_str(&format!(
            "{}\n00:00:{:02},{:03} --> 00:00:{:02},{:03}\nCUE{i:02}\n\n",
            i + 1,
            start / 1000,
            start % 1000,
            end / 1000,
            end % 1000,
        ));
    }
    srt
}

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // FCASTPLAYBIN_TEST_LOG=debug shows the crate's tracing.
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        gst::init().unwrap();
    });
}

/// Whether the needed plugins are present. Tests skip when missing.
fn plugins_available() -> bool {
    [
        "videotestsrc",
        "vp8enc",
        "vp8dec",
        "matroskamux",
        "matroskademux",
        "subparse",
        "textoverlay",
        "subtitleoverlay",
        "decodebin3",
    ]
    .iter()
    .all(|f| gst::ElementFactory::find(f).is_some())
}

fn tmp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "fcastplaybin-subdetach-{}-{}",
        std::process::id(),
        name
    ))
}

fn run_to_eos(desc: &str) {
    let pipeline = gst::parse::launch(desc).expect("encode pipeline parses");
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("encode finishes");
    if let gst::MessageView::Error(err) = msg.view() {
        panic!("encode pipeline failed: {}", err.error());
    }
    pipeline.set_state(gst::State::Null).unwrap();
}

/// Write the SRT file and return its path.
fn write_srt(name: &str) -> std::path::PathBuf {
    let path = tmp_path(name);
    std::fs::write(&path, srt_content()).expect("writing the srt file");
    path
}

/// Encode an 8s video-only mkv (240 x 640x480 vp8 frames @30fps).
fn encode_video_mkv(name: &str) -> String {
    let path = tmp_path(name);
    let desc = format!(
        "videotestsrc num-buffers=240 pattern=black \
           ! video/x-raw,width=640,height=480,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! matroskamux ! filesink location={}",
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

/// Encode the same clip with the SRT muxed in as an embedded text track.
fn encode_subtitled_mkv(name: &str) -> String {
    let srt = write_srt(&format!("{name}.srt"));
    let path = tmp_path(name);
    let desc = format!(
        "videotestsrc num-buffers=240 pattern=black \
           ! video/x-raw,width=640,height=480,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         filesrc location={} ! subparse ! mux. \
         matroskamux name=mux ! filesink location={}",
        srt.display(),
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

/// A playbin under test plus every event its callback produced. Events
/// can arrive during preroll, before the waits that need them, so waits
/// look back through `log`.
struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: std::cell::RefCell<Vec<PlaybinEvent>>,
    /// Transport state for the [`SelectionGate`]. Tests flip it around
    /// `pause()`/`play()`.
    paused: std::cell::Cell<bool>,
}

impl Harness {
    fn new() -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: None, // internal synced fakesink
            audio: AudioSink::Factory(Box::new(|| {
                let sink = gst::ElementFactory::make("fakesink")
                    .property("sync", true)
                    .build()?;
                Ok(sink)
            })),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, generation| {
            let _ = tx.send((event, generation));
        });
        Self {
            playbin,
            events,
            log: std::cell::RefCell::new(Vec::new()),
            paused: std::cell::Cell::new(false),
        }
    }

    /// Settled gate with `seekable: false` so the engine never schedules
    /// the re-emit flush, which is orthogonal to detach timing.
    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: false,
        }
    }

    /// The receiver's settle-point calls, run from every wait loop so the
    /// link policy gets constant chances to relink mid-disable.
    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(self.gate());
    }

    /// Wait until `pred` matches a newly received event, pumping between
    /// polls. Panics with the log on timeout or pipeline error.
    fn wait_for(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent) -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what}; log: {:#?}",
                    self.log.borrow()
                );
            }
            self.settle_pump();
            match self.events.recv_timeout(Duration::from_millis(20)) {
                Ok((event, _generation)) => {
                    if let PlaybinEvent::Error { error, .. } = &event {
                        panic!(
                            "pipeline error while waiting for {what}: {error} (log: {:#?})",
                            self.log.borrow()
                        );
                    }
                    let hit = pred(&event);
                    self.log.borrow_mut().push(event);
                    if hit {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!(
                        "event channel closed while waiting for {what}; log: {:#?}",
                        self.log.borrow()
                    )
                }
            }
        }
    }

    /// The latest `StreamsSelected` in the log (subtitle slot), if any.
    fn last_selected_subtitle(&self) -> Option<Option<String>> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamsSelected { subtitle, .. } => Some(subtitle.clone()),
                _ => None,
            })
    }

    /// The text stream id of the latest advertised collection in the log.
    fn text_sid(&self) -> Option<String> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|event| match event {
                PlaybinEvent::StreamCollection(collection) => {
                    collection.iter().find_map(|stream| {
                        stream
                            .stream_type()
                            .contains(gst::StreamType::TEXT)
                            .then(|| stream.stream_id().map(|s| s.to_string()))
                            .flatten()
                    })
                }
                _ => None,
            })
    }

    /// Load `uri`, start playback and wait for the settled PLAYING.
    fn load_and_play(&self, uri: &str) {
        self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.wait_for("Loaded", |event| {
            matches!(event, PlaybinEvent::Loaded { .. })
        });
        self.playbin.play().expect("play");
        self.wait_for("settled PLAYING", |event| {
            matches!(
                event,
                PlaybinEvent::StateChanged {
                    current: gst::State::Playing,
                    pending: gst::State::VoidPending,
                    ..
                }
            )
        });
    }

    /// The overlay's subtitle input pad. Always present, only its peer
    /// comes and goes.
    fn overlay_subtitle_pad(&self) -> gst::Pad {
        self.playbin
            .pipeline()
            .by_name("fpb-suboverlay")
            .expect("subtitleoverlay in the pipeline")
            .static_pad("subtitle_sink")
            .expect("subtitleoverlay has a subtitle_sink pad")
    }

    /// Wait (pumping) until the overlay's subtitle input is linked/unlinked,
    /// returning how long it took. Panics after `bound`.
    fn wait_subtitle_branch(&self, want_linked: bool, bound: Duration, what: &str) -> Duration {
        let pad = self.overlay_subtitle_pad();
        let start = Instant::now();
        while pad.is_linked() != want_linked {
            if start.elapsed() >= bound {
                panic!(
                    "subtitle branch did not become {} within {bound:?} ({what})",
                    if want_linked { "linked" } else { "unlinked" },
                );
            }
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        start.elapsed()
    }

    /// Probe on subtitleoverlay's src pad recording, per video buffer,
    /// whether it carries a rendered cue. Topology checks alone can pass
    /// while frames still carry the cue. Attach mode rides as a
    /// `GstVideoOverlayCompositionMeta`, blend mode as white glyphs in the
    /// black video's luma plane.
    fn install_text_probe(&self) -> TextSeen {
        let src = self
            .playbin
            .pipeline()
            .by_name("fpb-suboverlay")
            .expect("subtitleoverlay in the pipeline")
            .static_pad("src")
            .expect("subtitleoverlay has a src pad");
        let seen: TextSeen = Default::default();
        let seen_cb = seen.clone();
        src.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                let has_meta = buffer
                    .iter_meta::<gst::Meta>()
                    .any(|meta| meta.api().name().contains("VideoOverlayComposition"));
                // Blend mode: white glyphs in the otherwise black luma
                // plane (stays well under 100 through vp8).
                let has_pixels = !has_meta
                    && buffer.map_readable().is_ok_and(|map| {
                        let luma = &map[..map.len().min(640 * 480)];
                        luma.iter().filter(|&&y| y > 128).count() >= 10
                    });
                seen_cb
                    .lock()
                    .unwrap()
                    .push((Instant::now(), has_meta || has_pixels));
            }
            gst::PadProbeReturn::Ok
        });
        seen
    }

    /// Wait (pumping) until playback position reaches `target`.
    fn wait_position(&self, target: gst::ClockTime) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Some(pos) = self.playbin.position()
                && pos >= target
            {
                return;
            }
            assert!(Instant::now() < deadline, "position never reached {target}");
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Issue the subtitle-off request and measure until the branch leaves
    /// the overlay and the deselect confirms, pumping throughout.
    fn disable_subtitles_and_measure(&self) -> (Duration, Duration) {
        let t0 = Instant::now();
        self.playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
        self.playbin.pump_selection(self.gate());

        let unlink = self.wait_subtitle_branch(false, EVENT_TIMEOUT, "subtitle disable");
        self.wait_for("deselect StreamsSelected", |event| {
            matches!(event, PlaybinEvent::StreamsSelected { subtitle: None, .. })
        });
        let confirm = t0.elapsed();

        // The disable must hold while settle calls keep coming. The routed
        // pad outlives the detach and must not be relinked.
        let hold = Instant::now() + Duration::from_millis(500);
        let pad = self.overlay_subtitle_pad();
        while Instant::now() < hold {
            self.settle_pump();
            assert!(
                !pad.is_linked(),
                "the link policy relinked a disabled subtitle stream"
            );
            std::thread::sleep(Duration::from_millis(5));
        }
        (unlink, confirm)
    }
}

/// Per-buffer record from [`Harness::install_text_probe`]: arrival time and
/// whether the buffer carried a rendered cue.
type TextSeen = std::sync::Arc<std::sync::Mutex<Vec<(Instant, bool)>>>;

/// The cue must be visible on buffers before a disable can claim to have
/// removed it.
fn wait_cue_visible(harness: &Harness, seen: &TextSeen) {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while !seen.lock().unwrap().iter().any(|(_, text)| *text) {
        assert!(
            Instant::now() < deadline,
            "the rendered cue never appeared in the video buffers"
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Once buffers past the bound exist, none may still carry the cue
/// (see [`CUE_CLEAR_BOUND`]).
fn assert_cue_cleared(harness: &Harness, seen: &TextSeen, t_disable: Instant, what: &str) {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        let log = seen.lock().unwrap();
        if log.iter().any(|(at, _)| *at > t_disable + CUE_CLEAR_BOUND) {
            let last_text = log
                .iter()
                .filter(|(_, text)| *text)
                .map(|(at, _)| *at)
                .max();
            if let Some(last) = last_text {
                let after = last.saturating_duration_since(t_disable);
                eprintln!("{what}: last cue-bearing buffer {after:?} after the disable");
                assert!(
                    after <= CUE_CLEAR_BOUND,
                    "video buffers still carried the cue {after:?} after the disable"
                );
            }
            return;
        }
        drop(log);
        assert!(
            Instant::now() < deadline,
            "no video buffers flowed after the disable"
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Bring the subtitle track up, whether or not decodebin3 auto-selected
/// text, and wait for its branch to reach the overlay. Returns the
/// selected stream id.
fn enable_and_await_subtitles(harness: &Harness) -> String {
    if harness.last_selected_subtitle().is_none() {
        harness.wait_for("initial StreamsSelected", |event| {
            matches!(event, PlaybinEvent::StreamsSelected { .. })
        });
    }
    let sid = match harness.last_selected_subtitle().expect("a selection") {
        Some(sid) => sid,
        None => {
            let sid = harness
                .text_sid()
                .expect("collection advertises a text stream");
            harness
                .playbin
                .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid.clone())));
            harness.playbin.pump_selection(harness.gate());
            harness.wait_for("subtitle StreamsSelected", |event| {
                matches!(
                    event,
                    PlaybinEvent::StreamsSelected {
                        subtitle: Some(_),
                        ..
                    }
                )
            });
            sid
        }
    };
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "initial subtitle link");
    sid
}

/// Embedded text track: disable mid-cue, verify the branch leaves within
/// the bound, then re-enable and play out to EOS.
#[test]
fn embedded_subtitle_disable_is_immediate() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_subtitled_mkv("embedded.mkv");
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let sid = enable_and_await_subtitles(&harness);
    let text_seen = harness.install_text_probe();

    // Disable with a cue on screen, and confirm the probe sees it so the
    // cleared assert cannot pass vacuously.
    harness.wait_position(gst::ClockTime::from_mseconds(2000));
    wait_cue_visible(&harness, &text_seen);

    let t_disable = Instant::now();
    let (unlink, confirm) = harness.disable_subtitles_and_measure();
    eprintln!("embedded: unlink after {unlink:?}, deselect confirmed after {confirm:?}");
    assert!(
        unlink <= DISABLE_BOUND,
        "subtitle branch stayed in the overlay for {unlink:?} after the disable"
    );
    assert!(
        confirm <= DISABLE_BOUND,
        "subtitle deselect took {confirm:?} to confirm"
    );
    assert_cue_cleared(&harness, &text_seen, t_disable, "embedded");

    // Round trip: the same track must come back on request.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid)));
    harness.playbin.pump_selection(harness.gate());
    harness.wait_for("re-enable StreamsSelected", |event| {
        matches!(
            event,
            PlaybinEvent::StreamsSelected {
                subtitle: Some(_),
                ..
            }
        )
    });
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "re-enable");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// External subtitles: subparse delivers every cue instantly, so the
/// pre-fix drain-gated detach is at its slowest.
#[test]
fn external_subtitle_disable_is_immediate() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("external.mkv");
    let srt = write_srt("external.srt");
    let harness = Harness::new();
    harness.load_and_play(&uri);

    let id = harness
        .playbin
        .attach_subtitle(&format!("file://{}", srt.display()))
        .expect("attaching the external subtitle");
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.playbin.pump_selection(harness.gate());
    harness.wait_for("external subtitle selected", |event| {
        matches!(
            event,
            PlaybinEvent::StreamsSelected {
                subtitle: Some(_),
                ..
            }
        )
    });
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "external subtitle link");
    let text_seen = harness.install_text_probe();
    harness.wait_position(gst::ClockTime::from_mseconds(2000));
    wait_cue_visible(&harness, &text_seen);

    let t_disable = Instant::now();
    let (unlink, confirm) = harness.disable_subtitles_and_measure();
    eprintln!("external: unlink after {unlink:?}, deselect confirmed after {confirm:?}");
    assert!(
        unlink <= DISABLE_BOUND,
        "subtitle branch stayed in the overlay for {unlink:?} after the disable"
    );
    assert!(
        confirm <= DISABLE_BOUND,
        "subtitle deselect took {confirm:?} to confirm"
    );
    assert_cue_cleared(&harness, &text_seen, t_disable, "external");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// Disable while paused with a cue held in the overlay. The teardown must
/// not wait on video time, and resume must keep subtitles off.
#[test]
fn paused_subtitle_disable_is_immediate() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_subtitled_mkv("paused.mkv");
    let harness = Harness::new();
    harness.load_and_play(&uri);
    enable_and_await_subtitles(&harness);
    let text_seen = harness.install_text_probe();
    harness.wait_position(gst::ClockTime::from_mseconds(2000));
    wait_cue_visible(&harness, &text_seen);

    harness.playbin.pause().expect("pause");
    harness.wait_for("settled PAUSED", |event| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Paused,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });
    harness.paused.set(true);

    let (unlink, confirm) = harness.disable_subtitles_and_measure();
    eprintln!("paused: unlink after {unlink:?}, deselect confirmed after {confirm:?}");
    assert!(
        unlink <= DISABLE_BOUND,
        "subtitle branch stayed in the overlay for {unlink:?} after a paused disable"
    );
    assert!(
        confirm <= DISABLE_BOUND,
        "paused subtitle deselect took {confirm:?} to confirm"
    );

    let t_resume = Instant::now();
    harness.playbin.play().expect("resume");
    harness.paused.set(false);
    harness.wait_for("settled PLAYING after resume", |event| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Playing,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });
    // The disable must hold across the resume.
    assert!(
        !harness.overlay_subtitle_pad().is_linked(),
        "the subtitle branch came back on resume"
    );
    assert_cue_cleared(&harness, &text_seen, t_resume, "paused");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// Disable and re-enable back to back. The branch must end up linked and
/// playback must survive to EOS.
#[test]
fn rapid_subtitle_toggle_recovers() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_subtitled_mkv("toggle.mkv");
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let sid = enable_and_await_subtitles(&harness);
    harness.wait_position(gst::ClockTime::from_mseconds(2000));

    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    harness.playbin.pump_selection(harness.gate());
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid.clone())));
    harness.playbin.pump_selection(harness.gate());

    // The disable dispatches first, so the branch detaches once and must
    // come back and stay. Wait out the detach before judging the relink.
    harness.wait_subtitle_branch(false, EVENT_TIMEOUT, "rapid toggle detach");
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "rapid toggle relink");
    let hold = Instant::now() + Duration::from_millis(500);
    let pad = harness.overlay_subtitle_pad();
    while Instant::now() < hold {
        harness.settle_pump();
        assert!(
            pad.is_linked(),
            "the re-enabled subtitle branch was torn down again"
        );
        std::thread::sleep(Duration::from_millis(5));
    }

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}
