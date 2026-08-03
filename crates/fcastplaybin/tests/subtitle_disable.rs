//! Integration tests: disabling subtitles takes effect immediately.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, ExternalSubId, FcastPlaybin, MediaInput, PlaybinEvent, Seek, SelectionGate, Sinks,
    StartPoint, TrackSlot, TrackTarget,
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

/// Dense back-to-back cues (every 500ms from 0.5s on). Density matters:
/// decodebin3 holds sparse text until the interleave covers the cue gap,
/// and a steady cue stream makes a broken disable visible.
fn srt_content(cues: u32) -> String {
    let mut srt = String::new();
    for i in 0..cues {
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
fn write_srt(name: &str, cues: u32) -> std::path::PathBuf {
    let path = tmp_path(name);
    std::fs::write(&path, srt_content(cues)).expect("writing the srt file");
    path
}

/// Encode a video-only mkv (640x480 vp8 @30fps, `seconds` long).
fn encode_video_mkv(name: &str, seconds: u32) -> String {
    let path = tmp_path(name);
    let desc = format!(
        "videotestsrc num-buffers={} pattern=black \
           ! video/x-raw,width=640,height=480,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! matroskamux ! filesink location={}",
        seconds * 30,
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

/// Encode the same clip with an SRT muxed in as an embedded text track.
fn encode_subtitled_mkv(name: &str, seconds: u32, cues: u32) -> String {
    let srt = write_srt(&format!("{name}.srt"), cues);
    let path = tmp_path(name);
    let desc = format!(
        "videotestsrc num-buffers={} pattern=black \
           ! video/x-raw,width=640,height=480,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         filesrc location={} ! subparse ! mux. \
         matroskamux name=mux ! filesink location={}",
        seconds * 30,
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
    /// the re-emit flush, which is orthogonal to detach timing. The
    /// external re-enable tests keep it false too, ON PURPOSE: the spent
    /// input's replay is input-scoped and must work on unseekable media.
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

    /// Move every already-queued event into the log without matching.
    /// Sequenced dances (pause -> seek -> resume) must not let a wait be
    /// satisfied by a STALE event of the same shape (the load emits its
    /// own RateChanged and settled-state events); drain before issuing
    /// the operation whose outcome the next wait attributes.
    fn drain_events(&self) {
        while let Ok((event, _generation)) = self.events.try_recv() {
            self.log.borrow_mut().push(event);
        }
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

/// A cue-bearing buffer must arrive after `after` (a re-enable), proving
/// the re-selected track actually renders. Topology is not enough: the
/// regression this guards had the branch linked and the selection
/// confirmed while no data, not even caps, ever flowed again.
fn wait_cue_visible_after(harness: &Harness, seen: &TextSeen, after: Instant, what: &str) {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while !seen
        .lock()
        .unwrap()
        .iter()
        .any(|(at, text)| *text && *at > after)
    {
        assert!(
            Instant::now() < deadline,
            "no rendered cue reappeared after the {what}"
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

/// Attach `srt` as an external subtitle, select it, and wait until cues
/// actually render. Returns the external id and the probe log.
fn attach_and_show_external(harness: &Harness, srt: &std::path::Path) -> (ExternalSubId, TextSeen) {
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
    wait_cue_visible(harness, &text_seen);
    (id, text_seen)
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
    let uri = encode_subtitled_mkv("embedded.mkv", 8, 14);
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
    let uri = encode_video_mkv("external.mkv", 8);
    let srt = write_srt("external.srt", 14);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let (_id, text_seen) = attach_and_show_external(&harness, &srt);

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
    let uri = encode_subtitled_mkv("paused.mkv", 8, 14);
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
    let uri = encode_subtitled_mkv("toggle.mkv", 8, 14);
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

/// Disable then re-enable an external subtitle, requiring cues to
/// RENDER again. Regression: the re-select confirmed through
/// decodebin3's drained leftover slot with the branch linked while not
/// a single buffer or caps event ever reached the overlay again.
#[test]
fn external_subtitle_reenable_renders_again() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("reenable.mkv", 12);
    let srt = write_srt("reenable.srt", 22);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let (id, text_seen) = attach_and_show_external(&harness, &srt);

    harness.disable_subtitles_and_measure();

    let t_reenable = Instant::now();
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
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
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "external re-enable link");
    wait_cue_visible_after(&harness, &text_seen, t_reenable, "re-enable");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// The receiver's actual re-enable path: a materialized external is
/// re-selected BY STREAM ID (the receiver resolves an advertised
/// catalog external to a plain stream target). The recovery must fire
/// for that desire shape too, not just `TrackTarget::ExternalSubtitle`.
#[test]
fn external_subtitle_reenable_by_sid_renders_again() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("reenable-sid.mkv", 12);
    let srt = write_srt("reenable-sid.srt", 22);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let (_id, text_seen) = attach_and_show_external(&harness, &srt);
    let sid = harness
        .last_selected_subtitle()
        .flatten()
        .expect("the external subtitle's stream id");

    harness.disable_subtitles_and_measure();

    let t_reenable = Instant::now();
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid)));
    harness.playbin.pump_selection(harness.gate());
    harness.wait_for("by-sid re-enable StreamsSelected", |event| {
        matches!(
            event,
            PlaybinEvent::StreamsSelected {
                subtitle: Some(_),
                ..
            }
        )
    });
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "by-sid re-enable link");
    wait_cue_visible_after(&harness, &text_seen, t_reenable, "by-sid re-enable");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// The same regression back to back: the re-enable request lands while
/// the disable is still confirming. The desire must park until the
/// re-armed input materializes instead of re-binding the dead slot.
#[test]
fn rapid_external_subtitle_toggle_renders_again() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("rapid-ext.mkv", 12);
    let srt = write_srt("rapid-ext.srt", 22);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let (id, text_seen) = attach_and_show_external(&harness, &srt);

    let t_toggle = Instant::now();
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    harness.playbin.pump_selection(harness.gate());
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.playbin.pump_selection(harness.gate());

    harness.wait_subtitle_branch(false, EVENT_TIMEOUT, "rapid external toggle detach");
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "rapid external toggle relink");
    wait_cue_visible_after(&harness, &text_seen, t_toggle, "rapid toggle");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// The materialized stream id of an attached external input, waiting out
/// the attach propagation.
fn external_sid(harness: &Harness, id: ExternalSubId) -> String {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        if let Some(sid) = harness.playbin.subtitle_stream_ids(id).into_iter().next() {
            return sid;
        }
        assert!(
            Instant::now() < deadline,
            "the external stream never materialized"
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
}

/// Request `target`, wait until the LATEST confirmed selection is exactly
/// `expect` (scanning the log, not just fresh events: decodebin3's
/// collection-default auto-select may have confirmed the stream before
/// the request, adopted by the engine with nothing left to dispatch) and
/// the branch is linked, then take the attribution marker a beat AFTER
/// both, so a cue-after assert cannot be satisfied by the outgoing
/// track's final frames.
fn switch_subtitle(harness: &Harness, target: TrackTarget, expect: &str, what: &str) -> Instant {
    harness.playbin.request_track(TrackSlot::Subtitle, target);
    harness.playbin.pump_selection(harness.gate());
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        harness.drain_events();
        if harness.last_selected_subtitle() == Some(Some(expect.to_string())) {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "the selection never confirmed ({what}); log: {:#?}",
            harness.log.borrow()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, what);
    std::thread::sleep(Duration::from_millis(150));
    Instant::now()
}

/// The caller-side seek dance (pause, seek at settled PAUSED, resume), as
/// the receiver performs it. Returns the attribution marker taken after
/// the resume settles.
fn seek_to(harness: &Harness, position: gst::ClockTime) -> Instant {
    // Every wait below must be satisfied by THIS dance's events, not by
    // leftovers of the load or a previous dance.
    harness.drain_events();
    harness.playbin.pause().expect("pause for the seek");
    harness.wait_for("settled PAUSED for the seek", |event| {
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
    harness.playbin.seek_async(Seek::new(Some(position), None));
    harness.wait_for("seek performed", |event| {
        matches!(event, PlaybinEvent::RateChanged(_))
    });
    harness.playbin.play().expect("resume after the seek");
    harness.paused.set(false);
    harness.wait_for("settled PLAYING after the seek", |event| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Playing,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });
    Instant::now()
}

/// Toggle the external off and on TWICE. The spent-input recovery must be
/// repeatable: every deselect re-arms the replay flag, every rejoin
/// consumes it.
#[test]
fn external_subtitle_double_toggle_renders_each_time() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("double-toggle.mkv", 18);
    let srt = write_srt("double-toggle.srt", 34);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let (id, text_seen) = attach_and_show_external(&harness, &srt);

    for round in 1..=2u32 {
        harness.disable_subtitles_and_measure();
        let t_reenable = Instant::now();
        harness
            .playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
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
        harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "double-toggle re-enable");
        wait_cue_visible_after(
            &harness,
            &text_seen,
            t_reenable,
            &format!("re-enable round {round}"),
        );
    }

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// Switch embedded -> external -> embedded -> external, requiring
/// rendered cues on every leg. Switching AWAY (not just off) must flag
/// the external as spent, the embedded track must survive being
/// deselected mid-item, and the return to the external must replay.
#[test]
fn embedded_external_switch_roundtrip_renders() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_subtitled_mkv("roundtrip.mkv", 18, 34);
    let srt = write_srt("roundtrip.srt", 34);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let embedded_sid = enable_and_await_subtitles(&harness);
    let text_seen = harness.install_text_probe();
    harness.wait_position(gst::ClockTime::from_mseconds(2000));
    wait_cue_visible(&harness, &text_seen);

    let id = harness
        .playbin
        .attach_subtitle(&format!("file://{}", srt.display()))
        .expect("attaching the external subtitle");
    let ext_sid = external_sid(&harness, id);

    let t = switch_subtitle(
        &harness,
        TrackTarget::ExternalSubtitle(id),
        &ext_sid,
        "switch to the external",
    );
    wait_cue_visible_after(&harness, &text_seen, t, "external leg");

    let t = switch_subtitle(
        &harness,
        TrackTarget::Stream(Some(embedded_sid.clone())),
        &embedded_sid,
        "switch back to the embedded track",
    );
    wait_cue_visible_after(&harness, &text_seen, t, "embedded return leg");

    let t = switch_subtitle(
        &harness,
        TrackTarget::ExternalSubtitle(id),
        &ext_sid,
        "return to the external",
    );
    wait_cue_visible_after(&harness, &text_seen, t, "external return leg");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// Two attached externals: switch A -> B -> A with rendered cues on every
/// leg. The add-after-deselect class (a second external has no leftover
/// slot to inherit) plus the spent-input replay on the return.
#[test]
fn second_external_switch_and_return_renders() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("two-ext.mkv", 18);
    let srt_a = write_srt("two-ext-a.srt", 34);
    let srt_b = write_srt("two-ext-b.srt", 34);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let (id_a, text_seen) = attach_and_show_external(&harness, &srt_a);
    let a_sid = external_sid(&harness, id_a);

    let id_b = harness
        .playbin
        .attach_subtitle(&format!("file://{}", srt_b.display()))
        .expect("attaching the second external");
    let b_sid = external_sid(&harness, id_b);
    let t = switch_subtitle(
        &harness,
        TrackTarget::ExternalSubtitle(id_b),
        &b_sid,
        "switch to the second external",
    );
    wait_cue_visible_after(&harness, &text_seen, t, "second external leg");

    let t = switch_subtitle(
        &harness,
        TrackTarget::ExternalSubtitle(id_a),
        &a_sid,
        "switch back to the first external",
    );
    wait_cue_visible_after(&harness, &text_seen, t, "first external return leg");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// Disable and re-enable the external while PAUSED, then resume: the
/// reconfiguration must neither wedge the paused pipeline nor lose the
/// track, and cues must render after the resume.
#[test]
fn paused_external_toggle_shows_after_resume() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("paused-ext.mkv", 12);
    let srt = write_srt("paused-ext.srt", 22);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let (id, text_seen) = attach_and_show_external(&harness, &srt);

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

    harness.disable_subtitles_and_measure();
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.playbin.pump_selection(harness.gate());

    // A paused re-select may only confirm once data flows again, so don't
    // insist on ordering: resume and require rendering.
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
    let t_resume = Instant::now();
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "paused re-enable");
    wait_cue_visible_after(&harness, &text_seen, t_resume, "paused re-enable");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// User seeks with an external subtitle showing: cues must keep rendering
/// after a forward and a backward jump. A pipeline seek only travels the
/// sink chains and decodebin3 forwards it up the MAIN input, so without
/// explicit forwarding the external input's segment stays on the old
/// timeline and its cues never sync against the sought video again.
#[test]
fn seek_with_external_subtitles_keeps_rendering() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("seek-ext.mkv", 18);
    let srt = write_srt("seek-ext.srt", 34);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let (_id, text_seen) = attach_and_show_external(&harness, &srt);

    let t = seek_to(&harness, gst::ClockTime::from_seconds(9));
    wait_cue_visible_after(&harness, &text_seen, t, "forward seek");

    let t = seek_to(&harness, gst::ClockTime::from_seconds(3));
    wait_cue_visible_after(&harness, &text_seen, t, "backward seek");

    // No EOS wait: the backward jump would add ~15 real-time seconds and
    // EOS-after-replay is already covered by the re-enable tests.
}

/// An SRT big enough (several MB parsed) that decodebin3's byte-limited
/// sparse queue leaves the source MID-PUSH for the whole test, so a
/// deselect always catches it still pushing (the not-linked death).
fn write_big_srt(name: &str) -> std::path::PathBuf {
    let path = tmp_path(name);
    let mut srt = String::with_capacity(8 << 20);
    let pad = "x".repeat(160);
    let fmt = |ms: u32| {
        format!(
            "{:02}:{:02}:{:02},{:03}",
            ms / 3_600_000,
            (ms / 60_000) % 60,
            (ms / 1000) % 60,
            ms % 1000
        )
    };
    for i in 0..30_000u32 {
        let start = 500 + i * 100;
        srt.push_str(&format!(
            "{}\n{} --> {}\nCUE{i:05} {pad}\n\n",
            i + 1,
            fmt(start),
            fmt(start + 90),
        ));
    }
    std::fs::write(&path, srt).expect("writing the big srt");
    path
}

/// The worker must answer a queued job within a bound: a graph-dump
/// round-trip proves it is not wedged inside a previous job (the field
/// failure mode: every later attach, selection and state change frozen).
fn assert_worker_alive(harness: &Harness, what: &str) {
    let (tx, rx) = mpsc::channel();
    harness.playbin.debug_graph_async(Box::new(move |_| {
        let _ = tx.send(());
    }));
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        match rx.recv_timeout(Duration::from_millis(20)) {
            Ok(()) => return,
            Err(mpsc::RecvTimeoutError::Timeout) => {
                assert!(Instant::now() < deadline, "the worker is wedged: {what}");
                harness.settle_pump();
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died: {what}"),
        }
    }
}

/// The complete-failure field flow, verbatim: a mid-push external is
/// switched away (dies not-linked), subtitles go off, more sources
/// arrive. Nothing may wedge the worker (in the field every later job
/// queued forever: no selection, no pause, no stop), every source must
/// select and RENDER, and transport must keep working.
#[test]
fn still_pushing_deselect_rearm_keeps_worker_alive() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("churn.mkv", 18);
    let big = write_big_srt("churn-big.srt");
    let small_b = write_srt("churn-b.srt", 34);
    let small_c = write_srt("churn-c.srt", 34);
    let harness = Harness::new();
    harness.load_and_play(&uri);

    // A: the big source, selected and rendering, guaranteed mid-push.
    let (id_a, text_seen) = attach_and_show_external(&harness, &big);
    let a_sid = external_sid(&harness, id_a);

    // B: attach + select while A is mid-push. A's task dies not-linked
    // and A is marked for replay, in place.
    let id_b = harness
        .playbin
        .attach_subtitle(&format!("file://{}", small_b.display()))
        .expect("attaching B");
    let b_sid = external_sid(&harness, id_b);
    let t = switch_subtitle(
        &harness,
        TrackTarget::ExternalSubtitle(id_b),
        &b_sid,
        "switch to B while A is mid-push",
    );
    assert_worker_alive(&harness, "after the mid-push switch");
    wait_cue_visible_after(&harness, &text_seen, t, "B after the mid-push switch");

    // Subtitles off; the worker must survive the churn behind it.
    harness.disable_subtitles_and_measure();
    assert_worker_alive(&harness, "after the disable");

    // C: one more source must still attach, select and render.
    let id_c = harness
        .playbin
        .attach_subtitle(&format!("file://{}", small_c.display()))
        .expect("attaching C");
    let c_sid = external_sid(&harness, id_c);
    let t = switch_subtitle(
        &harness,
        TrackTarget::ExternalSubtitle(id_c),
        &c_sid,
        "select C after the churn",
    );
    assert_worker_alive(&harness, "after C's selection");
    wait_cue_visible_after(&harness, &text_seen, t, "C after the churn");

    // Back to A: the replay seek must RESTART the task the race killed
    // and cues must render again.
    let t = switch_subtitle(
        &harness,
        TrackTarget::ExternalSubtitle(id_a),
        &a_sid,
        "return to A after its task died",
    );
    assert_worker_alive(&harness, "after returning to A");
    wait_cue_visible_after(&harness, &text_seen, t, "A after its task died");

    // Transport still works.
    harness.drain_events();
    harness.playbin.pause().expect("pause after the churn");
    harness.wait_for("settled PAUSED after the churn", |event| {
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
    harness.playbin.play().expect("resume after the churn");
    harness.paused.set(false);
    harness.wait_for("settled PLAYING after the churn", |event| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Playing,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// Seek while an external attach is still materializing: the racing
/// flush may kill the young input, and it must still select and render
/// afterwards (retry when never-linked, replay when linked).
#[test]
fn seek_while_external_materializes_renders() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("seek-mat.mkv", 18);
    let srt = write_srt("seek-mat.srt", 34);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let text_seen = harness.install_text_probe();

    // Attach and IMMEDIATELY seek, before the stream materializes.
    let id = harness
        .playbin
        .attach_subtitle(&format!("file://{}", srt.display()))
        .expect("attaching the external subtitle");
    seek_to(&harness, gst::ClockTime::from_seconds(6));

    let sid = external_sid(&harness, id);
    let t = switch_subtitle(
        &harness,
        TrackTarget::ExternalSubtitle(id),
        &sid,
        "select after the mid-materialization seek",
    );
    wait_cue_visible_after(&harness, &text_seen, t, "mid-materialization seek");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// Seek immediately after enabling an external, inside the replay
/// window (the join queued a replay that may not have run yet): the
/// user seek and the replay must compose, cues rendering at the target.
#[test]
fn seek_immediately_after_enable_renders() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("seek-enable.mkv", 18);
    let srt = write_srt("seek-enable.srt", 34);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let text_seen = harness.install_text_probe();

    let id = harness
        .playbin
        .attach_subtitle(&format!("file://{}", srt.display()))
        .expect("attaching the external subtitle");
    let sid = external_sid(&harness, id);
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.playbin.pump_selection(harness.gate());
    let expected = sid.clone();
    harness.wait_for("enable confirmed", move |event| {
        matches!(
            event,
            PlaybinEvent::StreamsSelected { subtitle: Some(s), .. } if *s == expected
        )
    });

    // No cue wait: seek right into the replay window.
    let t = seek_to(&harness, gst::ClockTime::from_seconds(8));
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "branch after the replay-window seek");
    wait_cue_visible_after(&harness, &text_seen, t, "replay-window seek");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// Pause immediately after enabling an external (mid-replay): nothing
/// may wedge, and the cue must show after the resume.
#[test]
fn pause_immediately_after_enable_shows_on_resume() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("pause-enable.mkv", 12);
    let srt = write_srt("pause-enable.srt", 22);
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let text_seen = harness.install_text_probe();

    let id = harness
        .playbin
        .attach_subtitle(&format!("file://{}", srt.display()))
        .expect("attaching the external subtitle");
    let sid = external_sid(&harness, id);
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.playbin.pump_selection(harness.gate());
    let expected = sid.clone();
    harness.wait_for("enable confirmed", move |event| {
        matches!(
            event,
            PlaybinEvent::StreamsSelected { subtitle: Some(s), .. } if *s == expected
        )
    });

    harness.drain_events();
    harness.playbin.pause().expect("pause mid-replay");
    harness.wait_for("settled PAUSED mid-replay", |event| {
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
    assert_worker_alive(&harness, "paused mid-replay");
    harness.playbin.play().expect("resume");
    harness.paused.set(false);
    harness.wait_for("settled PLAYING after the mid-replay pause", |event| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Playing,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });
    let t_resume = Instant::now();
    harness.wait_subtitle_branch(true, EVENT_TIMEOUT, "mid-replay pause");
    wait_cue_visible_after(&harness, &text_seen, t_resume, "mid-replay pause resume");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}

/// stop() with a mid-push external showing must return within a bound
/// (the teardown used to NULL the pipeline with the text chain parked
/// on its own pad locks, the "cannot stop" class).
#[test]
fn stop_with_midpush_external_is_bounded() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_video_mkv("stop-mid.mkv", 18);
    let big = write_big_srt("stop-mid.srt");
    let harness = Harness::new();
    harness.load_and_play(&uri);
    let (_id, _text_seen) = attach_and_show_external(&harness, &big);

    // Stop on a helper thread so a wedge fails the test instead of
    // hanging it. The clone shares the same playbin.
    let playbin = harness.playbin.clone();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(playbin.stop().is_ok());
    });
    match rx.recv_timeout(Duration::from_secs(15)) {
        Ok(ok) => assert!(ok, "stop() failed"),
        Err(_) => panic!("stop() wedged with a mid-push external selected"),
    }
}

/// A new load replaces the media while a mid-push external is selected
/// and rendering: the old core teardown must not wedge, and a fresh
/// external on the new item must select and render.
#[test]
fn load_replaces_media_under_midpush_external() {
    init();
    if !plugins_available() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri_a = encode_video_mkv("swap-a.mkv", 18);
    let uri_b = encode_video_mkv("swap-b.mkv", 12);
    let big = write_big_srt("swap-a.srt");
    let small = write_srt("swap-b.srt", 22);
    let harness = Harness::new();
    harness.load_and_play(&uri_a);
    let (_id_a, text_seen) = attach_and_show_external(&harness, &big);

    // Replace the media out from under the mid-push external.
    harness.drain_events();
    harness.load_and_play(&uri_b);
    assert_worker_alive(&harness, "after the load swap");

    let id_b = harness
        .playbin
        .attach_subtitle(&format!("file://{}", small.display()))
        .expect("attaching on the new item");
    let b_sid = external_sid(&harness, id_b);
    let t = switch_subtitle(
        &harness,
        TrackTarget::ExternalSubtitle(id_b),
        &b_sid,
        "select on the new item",
    );
    assert_worker_alive(&harness, "after selecting on the new item");
    wait_cue_visible_after(&harness, &text_seen, t, "new item after the swap");

    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
}
