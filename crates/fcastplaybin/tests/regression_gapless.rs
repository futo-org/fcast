//! Gapless boundary cases the existing suite (`tests/gapless.rs`) does not
//! reach: the SHAPE of the stream collection changing across the swap.
//!
//! `tests/gapless.rs` only ever switches between items of IDENTICAL shape
//! (A/V -> A/V, audio-only -> audio-only, video-only -> video-only) plus one
//! demotion case (A/V -> video-only, which must NOT switch at all). Nothing
//! there carries a text stream, even though `gapless-plan.md`'s "Known
//! hazards" is explicit that "the swap must be validated with text-bearing
//! items on both sides", and nothing there has a stream that ends before its
//! siblings.
//!
//! Those are the shapes where a gapless swap can wedge SILENTLY rather than
//! fail loudly:
//!
//! * `perform_gapless_swap` only demands a successor for VIDEO and AUDIO (the
//!   `routed_kinds` loop in lib.rs). A live TEXT slot with no successor is
//!   released instead, so a text-bearing item followed by a plain one takes a
//!   path with no coverage check behind it at all.
//! * `streamsynchronizer` parks every streaming thread until each non-sparse
//!   stream has delivered its new stream-start, with no timeout. A slot that
//!   dies at the boundary, or a stream that ended early, is exactly the input
//!   shape that turns that wait into a permanent park. A park does not show up
//!   as an error or an EOS: the event stream simply goes quiet, which is why
//!   every case here also asserts that AUDIO KEEPS REACHING THE SINK.
//!
//! The media is encoded the way `tests/gapless.rs` encodes it (vp8 + vorbis,
//! plus a muxed SubRip track where the case needs one), because that is the
//! configuration the gapless path is known to work in. `ftest://` scenario
//! media is NOT usable here: see `control_gapless_switch_between_identical_av_items`.

use std::{
    cell::{Cell, RefCell},
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use gst::prelude::*;

/// Generous bound: the media plays in real time (synced sinks) and the suite
/// runs several pipelines at once. A wedge must FAIL, not hang.
const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

const TEARDOWN_BOUND: Duration = Duration::from_secs(15);

/// Seconds per item. Long enough that the swap happens in steady PLAYING.
const CLIP_SECONDS: u32 = 2;

/// A conservative lower bound for "the item actually PLAYED in real time".
/// A transition that free-wheels finishes a 2s clip in far less than this.
const CLIP_MIN: Duration = Duration::from_millis(900);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        gst::init().unwrap();
        // The receiver's part of the pipeline: fcastaudiostretch is built by
        // the fcastplaybin constructor but registered by the application.
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// The element factories every case here needs.
const REQUIRED_FACTORIES: &[&str] = &[
    "videotestsrc",
    "audiotestsrc",
    "vp8enc",
    "vp8dec",
    "vorbisenc",
    "vorbisdec",
    "matroskamux",
    "matroskademux",
    "subparse",
    "subtitleoverlay",
    "decodebin3",
];

/// Whether the plugins these tests need are present. The skip is OPT-IN.
///
/// This used to skip silently, and a silent skip is indistinguishable from a
/// pass: running the binary outside the devshell prints
///
///     skipping: required GStreamer plugins missing
///     test result: ok. 1 passed; 0 failed
///
/// which is a green suite that exercised nothing. Every case here is about
/// pipeline behaviour, so an environment without the plugins has not "passed"
/// them, it has not run them. Exotic environments can still opt out with
/// `FCASTPLAYBIN_ALLOW_PLUGIN_SKIP=1`, which is a deliberate act rather than
/// the default, and the skip then names what is missing instead of being a
/// blanket "missing".
fn require_plugins() -> bool {
    let missing: Vec<&str> = REQUIRED_FACTORIES
        .iter()
        .copied()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        std::env::var_os("FCASTPLAYBIN_ALLOW_PLUGIN_SKIP").is_some(),
        "required GStreamer plugins are missing: {missing:?}. These tests cannot \
         pass without them, so they fail rather than report a green run that \
         exercised nothing. Set FCASTPLAYBIN_ALLOW_PLUGIN_SKIP=1 to skip instead."
    );
    eprintln!("skipping: required GStreamer plugins missing: {missing:?}");
    false
}

fn tmp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "fcastplaybin-regression-gapless-{}-{}",
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
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("encode finishes");
    if let gst::MessageView::Error(err) = msg.view() {
        panic!("encode pipeline failed: {}", err.error());
    }
    pipeline.set_state(gst::State::Null).unwrap();
}

/// Dense back-to-back cues covering `seconds`, one every 400ms, so a cue is
/// live right up to the item boundary and the text stream is still producing
/// when the swap performs.
fn write_srt(name: &str, seconds: u32) -> std::path::PathBuf {
    let path = tmp_path(name);
    let mut srt = String::new();
    for i in 0..(seconds * 1000 / 400) {
        let start = i * 400;
        let end = start + 380;
        let stamp = |ms: u32| {
            format!(
                "{:02}:{:02}:{:02},{:03}",
                ms / 3_600_000,
                (ms / 60_000) % 60,
                (ms / 1000) % 60,
                ms % 1000
            )
        };
        srt.push_str(&format!(
            "{}\n{} --> {}\nCUE{i:02}\n\n",
            i + 1,
            stamp(start),
            stamp(end)
        ));
    }
    std::fs::write(&path, srt).expect("writing the srt file");
    path
}

/// Encode an A/V matroska clip: 64x64 vp8 + a vorbis tone.
/// `audio_buffers` fixes the audio branch's length independently of the
/// video's, which is what the ragged-duration case needs.
fn encode_av(name: &str, pattern: &str, freq: u32, audio_buffers: u32) -> String {
    let path = tmp_path(name);
    let desc = format!(
        "videotestsrc num-buffers={} pattern={pattern} \
           ! video/x-raw,width=64,height=64,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         audiotestsrc num-buffers={audio_buffers} freq={freq} \
           ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! vorbisenc ! mux. \
         matroskamux name=mux ! filesink location={}",
        CLIP_SECONDS * 30,
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

/// [`encode_av`] with equal-length branches (the ordinary item).
fn encode_av_even(name: &str, pattern: &str, freq: u32) -> String {
    // 87 x 1024 samples @ 44.1kHz ~= 2.02s, matching the 2s video branch.
    encode_av(name, pattern, freq, 87)
}

/// An A/V clip with a SubRip track muxed in as an embedded text stream.
fn encode_av_text(name: &str, pattern: &str, freq: u32) -> String {
    encode_av_text_sized(name, pattern, freq, 64, 64, CLIP_SECONDS)
}

/// A text-bearing clip built for the cue-rendering cases: 640x480 black and
/// long enough (8s) that a selection confirming in a few hundred milliseconds
/// still leaves seconds of buffers for a cue to appear in.
fn encode_av_text_visible(name: &str, freq: u32) -> String {
    encode_av_text_sized(name, "black", freq, 640, 480, 8)
}

/// [`encode_av_text`] at an explicit resolution and length. The cases that
/// have to SEE a rendered cue use 640x480 black video, so subtitleoverlay's
/// blend mode leaves detectable white glyphs in an otherwise dark luma plane
/// (the same trick `tests/subtitle_disable.rs` uses), and a longer clip, so
/// the time a selection takes to confirm cannot be mistaken for a track that
/// never renders.
fn encode_av_text_sized(
    name: &str,
    pattern: &str,
    freq: u32,
    width: u32,
    height: u32,
    seconds: u32,
) -> String {
    let srt = write_srt(&format!("{name}.srt"), seconds);
    let path = tmp_path(name);
    let desc = format!(
        "videotestsrc num-buffers={} pattern={pattern} \
           ! video/x-raw,width={width},height={height},framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         audiotestsrc num-buffers={} freq={freq} \
           ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! vorbisenc ! mux. \
         filesrc location={} ! subparse ! mux. \
         matroskamux name=mux ! filesink location={}",
        seconds * 30,
        seconds * 44,
        srt.display(),
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

/// What [`Harness::install_text_probe`] watches at subtitleoverlay.
///
/// Two pads, not one. `rendered` is the overlay's OUTPUT, which answers the
/// user-visible question and nothing else. `arrived` is the overlay's TEXT
/// INPUT, which separates the two ways "no cue" happens: cue buffers that
/// reached the overlay and were not composited (a running-time or state
/// problem inside the overlay) versus cue buffers that never arrived at all
/// (the branch upstream is dead, however healthy it looks).
///
/// That distinction is not academic. Every captured failure of
/// [`subtitle_enable_after_a_gapless_transition_renders`] is the second kind:
/// decodebin3's re-selected text pad pushed nothing at all, not one buffer and
/// not even its sticky segment, while the selection was confirmed and the
/// branch was linked. Without `arrived` the test can only report "no cue" and
/// the reader is left to guess which.
#[derive(Default)]
struct OverlaySeen {
    /// One entry per buffer leaving the overlay: when, and whether it carried
    /// a rendered cue.
    rendered: Vec<(Instant, bool)>,
    /// One entry per buffer reaching the overlay's `subtitle_sink`.
    arrived: Vec<Instant>,
    /// Buffers inspected at the overlay's VIDEO input, and how many of those
    /// the cue detector would have called cue-bearing. The overlay draws
    /// nothing on its input, so any hit here is the MEDIA tripping the
    /// detector rather than a subtitle, which makes every cue assertion in the
    /// file vacuous.
    ///
    /// Capped at [`CALIBRATION_BUFFERS`]. The check is about what the media
    /// looks like, which the first frames answer completely, and the scan is
    /// not free: it reads the whole luma plane, and doing that for every frame
    /// at the input as well as the output doubles a per-frame cost that the
    /// other cases in this binary pay for in scheduling latency, since they
    /// run in parallel with these.
    video_in: usize,
    video_in_bright: usize,
}

/// How many frames the media-brightness calibration inspects. See
/// [`OverlaySeen::video_in`].
const CALIBRATION_BUFFERS: usize = 60;

type TextSeen = std::sync::Arc<std::sync::Mutex<OverlaySeen>>;

/// A playbin under test plus the ordered `(event, generation)` stream its
/// callback produced. Waits keep a log so an assertion can look BACK at what
/// already arrived, and pump the receiver's settle points so the selection
/// engine gets its normal chances to act (`tests/gapless.rs` needs neither,
/// having no subtitles to select).
struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<(PlaybinEvent, u64)>>,
    paused: Cell<bool>,
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
            log: RefCell::new(Vec::new()),
            paused: Cell::new(false),
        }
    }

    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: false,
        }
    }

    /// The receiver's settle-point calls, run from every wait loop.
    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(self.gate());
    }

    fn drain_events(&self) {
        while let Ok(entry) = self.events.try_recv() {
            self.log.borrow_mut().push(entry);
        }
    }

    /// Wait until `pred` matches a newly received event, pumping between
    /// polls. Panics with the log on timeout or pipeline error.
    fn wait_for(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent, u64) -> bool) {
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
                Ok((event, generation)) => {
                    if let PlaybinEvent::Error { error, .. } = &event {
                        panic!(
                            "pipeline error while waiting for {what}: {error} (log: {:#?})",
                            self.log.borrow()
                        );
                    }
                    let hit = pred(&event, generation);
                    self.log.borrow_mut().push((event, generation));
                    if hit {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                    "event channel closed while waiting for {what}; log: {:#?}",
                    self.log.borrow()
                ),
            }
        }
    }

    /// Load `uri`, start playback and wait for the settled PLAYING. Returns
    /// the load's generation.
    fn load_and_play(&self, uri: &str) -> u64 {
        self.drain_events();
        let generation = self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.wait_for("Loaded", |event, seen| {
            matches!(event, PlaybinEvent::Loaded { .. }) && seen == generation
        });
        self.playbin.play().expect("play");
        self.wait_for("settled PLAYING", |event, _| {
            matches!(
                event,
                PlaybinEvent::StateChanged {
                    current: gst::State::Playing,
                    pending: gst::State::VoidPending,
                    ..
                }
            )
        });
        generation
    }

    /// The text stream id of the latest advertised collection in the log.
    fn text_sid(&self) -> Option<String> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|(event, _)| match event {
                PlaybinEvent::StreamCollection(collection) => collection.iter().find_map(|stream| {
                    stream
                        .stream_type()
                        .contains(gst::StreamType::TEXT)
                        .then(|| stream.stream_id().map(|s| s.to_string()))
                        .flatten()
                }),
                _ => None,
            })
    }

    /// The latest `StreamsSelected` in the log (subtitle slot), if any.
    fn last_selected_subtitle(&self) -> Option<Option<String>> {
        self.log
            .borrow()
            .iter()
            .rev()
            .find_map(|(event, _)| match event {
                PlaybinEvent::StreamsSelected { subtitle, .. } => Some(subtitle.clone()),
                _ => None,
            })
    }

    /// Bring the item's text track up and wait until its branch reaches the
    /// overlay, so the swap that follows faces a LIVE (routed) text slot
    /// rather than a parked one. Parked text takes a different path through
    /// the swap and would not exercise the hazard.
    fn select_subtitles(&self) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let sid = loop {
            self.drain_events();
            if let Some(sid) = self.text_sid() {
                break sid;
            }
            assert!(
                Instant::now() < deadline,
                "the collection never advertised a text stream; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        };
        self.playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid.clone())));
        self.playbin.pump_selection(self.gate());
        loop {
            self.drain_events();
            if self.last_selected_subtitle() == Some(Some(sid.clone())) {
                break;
            }
            assert!(
                Instant::now() < deadline,
                "the text stream never confirmed as selected; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let pad = self
            .playbin
            .pipeline()
            .by_name("fpb-suboverlay")
            .expect("subtitleoverlay in the pipeline")
            .static_pad("subtitle_sink")
            .expect("subtitleoverlay has a subtitle_sink pad");
        while !pad.is_linked() {
            assert!(
                Instant::now() < deadline,
                "the text branch never reached the overlay; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The overlay's subtitle input pad. Always present once the overlay is;
    /// only its peer comes and goes.
    fn overlay_subtitle_pad(&self) -> gst::Pad {
        self.playbin
            .pipeline()
            .by_name("fpb-suboverlay")
            .expect("subtitleoverlay in the pipeline")
            .static_pad("subtitle_sink")
            .expect("subtitleoverlay has a subtitle_sink pad")
    }

    /// Probes on subtitleoverlay's src pad (per video buffer, whether it
    /// carries a rendered cue) and on its subtitle_sink (whether cue buffers
    /// reach it at all). Attach mode rides as a
    /// `GstVideoOverlayCompositionMeta`, blend mode as white glyphs in the
    /// black video's luma plane. Same detection as
    /// `tests/subtitle_disable.rs`.
    fn install_text_probe(&self) -> TextSeen {
        let overlay = self
            .playbin
            .pipeline()
            .by_name("fpb-suboverlay")
            .expect("subtitleoverlay in the pipeline");
        let src = overlay
            .static_pad("src")
            .expect("subtitleoverlay has a src pad");
        let seen: TextSeen = Default::default();
        // Installed UPSTREAM FIRST. A buffer that is already between
        // video_sink and src when the probes go in would otherwise be counted
        // at the output and not at the input, and the calibration below reads
        // as "no video reached the overlay" on a run where plenty did.
        let seen_cb = seen.clone();
        if let Some(video_sink) = overlay.static_pad("video_sink") {
            // The overlay's VIDEO input, running the SAME detector on video
            // that has provably had no cue drawn on it yet. See
            // [`Self::assert_detector_discriminates`].
            video_sink.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if let Some(buffer) = info.buffer() {
                    if seen_cb.lock().unwrap().video_in >= CALIBRATION_BUFFERS {
                        return gst::PadProbeReturn::Ok;
                    }
                    let bright = buffer.map_readable().is_ok_and(|map| {
                        let luma = &map[..map.len().min(640 * 480)];
                        luma.iter().filter(|&&y| y > 128).count() >= 10
                    });
                    let mut seen = seen_cb.lock().unwrap();
                    seen.video_in += 1;
                    seen.video_in_bright += usize::from(bright);
                }
                gst::PadProbeReturn::Ok
            });
        }
        // The overlay's own text input. Its pad is static, so this probe
        // survives the branch being detached and relinked, which is exactly
        // the window every case here cares about.
        let seen_cb = seen.clone();
        self.overlay_subtitle_pad()
            .add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if info.buffer().is_some() {
                    seen_cb.lock().unwrap().arrived.push(Instant::now());
                }
                gst::PadProbeReturn::Ok
            });
        let seen_cb = seen.clone();
        src.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
            if let Some(buffer) = info.buffer() {
                let has_meta = buffer
                    .iter_meta::<gst::Meta>()
                    .any(|meta| meta.api().name().contains("VideoOverlayComposition"));
                let has_pixels = !has_meta
                    && buffer.map_readable().is_ok_and(|map| {
                        let luma = &map[..map.len().min(640 * 480)];
                        luma.iter().filter(|&&y| y > 128).count() >= 10
                    });
                seen_cb
                    .lock()
                    .unwrap()
                    .rendered
                    .push((Instant::now(), has_meta || has_pixels));
            }
            gst::PadProbeReturn::Ok
        });
        seen
    }

    /// The cue must be visible before an assertion about removing it can
    /// claim anything.
    fn wait_cue_visible(&self, seen: &TextSeen, what: &str) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !seen.lock().unwrap().rendered.iter().any(|(_, text)| *text) {
            assert!(
                Instant::now() < deadline,
                "the rendered cue never appeared in the video buffers ({what})"
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        self.assert_detector_discriminates(seen, what);
    }

    /// The cue detector must be able to say NO as well as YES.
    ///
    /// `has_pixels` is "at least 10 luma samples above 128", which is true of
    /// EVERY buffer of a video that is not dark. On such media every assertion
    /// built on this probe passes with no subtitle anywhere in the pipeline,
    /// including the post-swap rendering requirement that is the whole point
    /// of the cases that use it. The media is supposed to be `pattern=black`
    /// and a comment says so, but a comment does not fail.
    ///
    /// Checked at the overlay's VIDEO INPUT rather than on the output stream,
    /// which is what makes it deterministic. subtitleoverlay draws nothing on
    /// its input, so a hit there is unambiguously the media and needs no
    /// statistics: "some output buffer had no cue" depends on catching one of
    /// the 20ms gaps between cues and is a coin flip on a short window.
    fn assert_detector_discriminates(&self, seen: &TextSeen, what: &str) {
        // The calibration needs input buffers to look at, and the probes go in
        // while video is already flowing, so wait for the first rather than
        // demanding it be there already.
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while seen.lock().unwrap().video_in == 0 {
            assert!(
                Instant::now() < deadline,
                "no buffer reached subtitleoverlay's video_sink ({what}), so the cue \
                 detector was never calibrated against this run's media"
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let seen = seen.lock().unwrap();
        assert_eq!(
            seen.video_in_bright, 0,
            "the cue detector fired on {} of the {} buffers reaching the overlay's \
             VIDEO input ({what}), where no cue has been drawn yet: the media is not \
             dark enough for white glyphs to be detectable, so every cue assertion \
             built on this probe is vacuous. Use `encode_av_text_visible` media.",
            seen.video_in_bright, seen.video_in
        );
    }

    /// A cue-bearing buffer must arrive after `after`.
    ///
    /// When none does, the message says whether cue buffers REACHED the
    /// overlay in that window. "They arrived and were not drawn" and "they
    /// never arrived" are different bugs in different components, and the
    /// answer costs one counter.
    fn wait_cue_visible_after(&self, seen: &TextSeen, after: Instant, what: &str) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            {
                let seen = seen.lock().unwrap();
                if seen
                    .rendered
                    .iter()
                    .any(|(at, text)| *text && *at > after)
                {
                    return;
                }
                if Instant::now() >= deadline {
                    let arrived = seen.arrived.iter().filter(|at| **at > after).count();
                    let composited = seen.rendered.iter().filter(|(at, _)| *at > after).count();
                    panic!(
                        "no rendered cue appeared after the {what}; in that window \
                         {arrived} cue buffers reached subtitleoverlay's subtitle_sink \
                         and {composited} video buffers left its src. {}",
                        if arrived == 0 {
                            "Zero arrivals: the text branch upstream of the overlay is \
                             dead, so this is not a running-time or compositing problem."
                        } else {
                            "Cue buffers DID arrive and were not drawn, so the overlay \
                             rejected them: suspect running-time alignment."
                        }
                    );
                }
            }
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Cue buffers must reach the overlay's text input after `after`.
    ///
    /// Strictly upstream of [`Self::wait_cue_visible_after`] and strictly
    /// weaker, so it fails FIRST and names the component. A selection that
    /// confirms and a branch that links prove only that the pipeline is wired;
    /// this is the first assertion that proves data moves through it.
    fn wait_text_reaches_overlay_after(&self, seen: &TextSeen, after: Instant, what: &str) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !seen
            .lock()
            .unwrap()
            .arrived
            .iter()
            .any(|at| *at > after)
        {
            assert!(
                Instant::now() < deadline,
                "not one cue buffer reached subtitleoverlay's subtitle_sink after the \
                 {what}, though the selection confirmed and the branch linked: the text \
                 branch upstream of the overlay is delivering nothing"
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Turn subtitles off and wait until the branch leaves the overlay.
    fn disable_subtitles(&self) {
        self.playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
        self.playbin.pump_selection(self.gate());
        let pad = self.overlay_subtitle_pad();
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while pad.is_linked() {
            assert!(
                Instant::now() < deadline,
                "the subtitle branch never left the overlay"
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The worker must answer a queued job within a bound. A wedged
    /// streaming thread shows up here as a job that never completes.
    fn assert_worker_alive(&self, what: &str) {
        let (tx, rx) = mpsc::channel();
        self.playbin.debug_graph_async(Box::new(move |_| {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the worker is wedged: {what}");
                    self.settle_pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died: {what}"),
            }
        }
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + TEARDOWN_BOUND;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "shutdown never completed");
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        }
    }
}

/// No pipeline EOS may appear in the log after `from`.
fn assert_no_eos_since(harness: &Harness, from: usize, context: &str) {
    let log = harness.log.borrow();
    assert!(
        !log[from..]
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::EndOfStream)),
        "unexpected pipeline EOS {context}; log since the prepare: {:#?}",
        &log[from..]
    );
}

/// Whether the latest advertised collection in the log carries a text
/// stream. Panics if no collection has been advertised at all.
fn collection_has_text(harness: &Harness) -> bool {
    harness
        .log
        .borrow()
        .iter()
        .rev()
        .find_map(|(event, _)| match event {
            PlaybinEvent::StreamCollection(collection) => Some(
                collection
                    .iter()
                    .any(|stream| stream.stream_type().contains(gst::StreamType::TEXT)),
            ),
            _ => None,
        })
        .expect("a stream collection was advertised")
}

/// The shared body of every case: play `first`, prepare `second` mid-item,
/// and require the whole gapless contract to hold across the boundary.
/// `select_text` brings the outgoing item's text track up first, so the swap
/// faces a live text slot. `text_before`/`text_after` pin the SHAPE on each
/// side of the boundary, so a case cannot quietly stop testing the shape it
/// is named for (a media-encoding change that dropped the subtitle track
/// would otherwise leave every assertion below still passing).
///
/// `cues_after_swap` additionally requires that text is still RENDERING on
/// the far side of the boundary. Linked is not rendering: a text branch left
/// on the wrong running-time origin stays wired to subtitleoverlay and
/// composites nothing (see
/// [`subtitle_enable_after_a_gapless_transition_renders`] for the mechanism),
/// and that is invisible to every other assertion here. Only meaningful with
/// `select_text && text_after`, and it needs `encode_av_text_visible` media on
/// BOTH sides so a rendered cue is detectable in the luma plane.
fn assert_gapless_across(
    first: &str,
    second: &str,
    select_text: bool,
    text_before: bool,
    text_after: bool,
    cues_after_swap: bool,
    what: &str,
) {
    let harness = Harness::new();
    let first_generation = harness.load_and_play(first);
    if select_text {
        harness.select_subtitles();
    }
    // Installed BEFORE the boundary and confirmed against the OUTGOING item,
    // so the post-swap requirement cannot pass vacuously on media that never
    // rendered anything.
    let text_seen = cues_after_swap.then(|| {
        let seen = harness.install_text_probe();
        harness.wait_cue_visible(&seen, &format!("before the swap ({what})"));
        seen
    });
    harness.drain_events();
    assert_eq!(
        collection_has_text(&harness),
        text_before,
        "the OUTGOING item does not have the shape this case needs ({what})"
    );
    let played_at = Instant::now();

    harness.drain_events();
    let mark = harness.log.borrow().len();
    let prepared_generation = harness
        .playbin
        .prepare_next_async(MediaInput::Uri(second.to_owned()));
    assert!(prepared_generation > first_generation);

    // The switch itself: an activation, with no pipeline EOS anywhere before
    // it. An EOS here means the boundary ENDED playback instead of continuing
    // it, which is the whole point of the feature.
    harness.wait_for("PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    assert_no_eos_since(&harness, mark, &format!("before the activation ({what})"));
    let activated_at = Instant::now();
    assert!(
        activated_at.duration_since(played_at) >= CLIP_MIN,
        "activation after {:?} means the first item did not play in real time ({what})",
        activated_at.duration_since(played_at)
    );

    // The swap must not have parked a streaming thread. A streamsynchronizer
    // park (waiting on a stream-start that never comes) leaves the worker and
    // the event stream looking perfectly healthy while the data plane stops
    // dead, so BOTH are checked: the worker answers, and playback position
    // keeps advancing.
    harness.assert_worker_alive(&format!("after the activation ({what})"));
    // Both samples are taken AFTER the timeline has rebased onto the new
    // item, so this measures progress and not the one-off backwards step the
    // rebase legitimately produces. (Taking the first sample at the
    // activation itself does not work: with a stream that ends early the
    // activation fires while `position()` still reads the OUTGOING item, and
    // the rebase then lands between the two samples.)
    std::thread::sleep(Duration::from_millis(400));
    let before = harness.playbin.position();
    std::thread::sleep(Duration::from_millis(500));
    let after = harness.playbin.position();
    match (before, after) {
        (Some(before), Some(after)) => assert!(
            after > before,
            "playback did not advance in 500ms after the activation ({what}): \
             {before} -> {after}, a parked streaming thread"
        ),
        other => panic!("no position reading after the activation ({what}): {other:?}"),
    }

    // The new item's own collection follows the activation, stamped with the
    // new generation, and it must have the shape this case is named for.
    harness.wait_for("the new item's StreamCollection", |event, generation| {
        matches!(event, PlaybinEvent::StreamCollection(_)) && generation == prepared_generation
    });
    assert_eq!(
        collection_has_text(&harness),
        text_after,
        "the INCOMING item does not have the shape this case needs ({what}): \
         the boundary never changed shape, so this case tested nothing"
    );

    // Text must still be DRAWING on the new item. The reference instant is
    // pushed 700ms into the future on purpose: subtitleoverlay can still be
    // holding the outgoing item's last cue for its remaining duration right
    // at the boundary, and a cue that far past the collection cannot be that
    // straggler, it can only be one of the new item's own.
    if let Some(text_seen) = &text_seen {
        harness.wait_cue_visible_after(
            text_seen,
            Instant::now() + Duration::from_millis(700),
            &format!("gapless swap ({what})"),
        );
    }

    // And the switched-to item reaches its own real end, under its own
    // generation, no earlier than its playtime.
    harness.wait_for("the final EndOfStream", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    let (_, eos_generation) = *harness.log.borrow().last().expect("the EOS was just logged");
    assert_eq!(
        eos_generation, prepared_generation,
        "the final EOS belongs to the activated item ({what})"
    );
    assert!(
        activated_at.elapsed() >= CLIP_MIN,
        "EOS {:?} after activation means the second item did not play in real time ({what})",
        activated_at.elapsed()
    );
    harness.shutdown();
}

/// CONTROL. Two items of IDENTICAL shape (A/V, equal-length branches) over
/// this file's harness and media.
///
/// Nothing about the shape changes here, so this reproduces what
/// `tests/gapless.rs::gapless_switch_produces_no_eos_between_items` already
/// covers, on the pumped harness and in matroska instead of webm. Its only
/// job is to make the shape-change cases below MEAN something: if this passes
/// and one of them fails, the failure is attributable to the shape change. If
/// this fails too, the harness or the container is at fault and nothing below
/// can be attributed.
///
/// (`ftest://` scenario media was the first choice here, for exact per-stream
/// durations. It is unusable: a gapless swap onto a prepared `ftest://` input
/// aborts the process inside decodebin3 —
/// `mq_slot_handle_stream_start: assertion failed:
/// (candidate->is_update || dbin->output_collection == NULL)` — for
/// IDENTICALLY shaped items too, so it discriminates nothing.)
#[test]
fn control_gapless_switch_between_identical_av_items() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let first = encode_av_even("ctrl-a.mkv", "smpte", 440);
    let second = encode_av_even("ctrl-b.mkv", "ball", 880);

    assert_gapless_across(&first, &second, false, false, false, false, "control A/V -> A/V");
}

/// A/V + TEXT -> A/V. The outgoing item's text slot has NO successor, so
/// `perform_gapless_swap` releases its decodebin3 request pad while audio and
/// video continue through reused ones.
///
/// This is the shape `gapless-plan.md` calls out and nothing tested. It is
/// also the shape with no coverage check behind it: the swap only refuses
/// when a live VIDEO or AUDIO slot lacks a successor, so a dying text slot
/// goes through unexamined. If that slot's death is not clean the symptom is
/// a park, not an error.
#[test]
fn gapless_switch_from_text_bearing_item_to_one_without() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let first = encode_av_text("text-a.mkv", "smpte", 440);
    let second = encode_av_even("text-b.mkv", "ball", 880);

    assert_gapless_across(&first, &second, true, true, false, false, "text -> no text");
}

/// A/V -> A/V + TEXT. The incoming item has a stream the outgoing one never
/// had, so its pad takes `perform_gapless_swap`'s `fresh` branch (a brand new
/// decodebin3 request pad) while audio and video reuse the old ones. A new
/// request pad appearing mid-stream is the input-side shape that historically
/// raced `route_db3_pad`'s gate.
#[test]
fn gapless_switch_into_a_text_bearing_item() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let first = encode_av_even("gain-a.mkv", "smpte", 440);
    let second = encode_av_text("gain-b.mkv", "ball", 880);

    assert_gapless_across(&first, &second, false, false, true, false, "no text -> text");
}

/// Text on BOTH sides, selected and live across the boundary: the
/// configuration `gapless-plan.md` names outright. The text slot is reused
/// like the A/V ones, so this is the only shape where subtitleoverlay sees a
/// new SEGMENT on its subtitle input with no flush in front of it — the
/// classic "element resets state on SEGMENT and swallows the boundary" setup.
#[test]
fn gapless_switch_between_text_bearing_items() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let first = encode_av_text("both-a.mkv", "smpte", 440);
    let second = encode_av_text("both-b.mkv", "ball", 880);

    assert_gapless_across(&first, &second, true, true, true, false, "text -> text");
}

/// The same text->text boundary as
/// [`gapless_switch_between_text_bearing_items`], but requiring that text is
/// still DRAWING afterwards rather than merely linked.
///
/// That distinction is the whole point: the case above passes today, and it
/// passes with a text branch that composites nothing. The swap REUSES the
/// text slot, so subtitleoverlay's `subtitle_sink` is never unlinked and the
/// per-stream queue in front of it is the very same element before and after
/// (measured: identical pointer across the boundary). `Inner::poll_text_policy`
/// builds a NEW queue whenever it links, so an unchanged queue is proof that
/// the link path never re-ran -- nothing in the crate revisits this branch at
/// the boundary at all.
///
/// FAILS TODAY (product bug), for the running-time reason measured in
/// [`subtitle_enable_after_a_gapless_transition_renders`]: text bypasses
/// streamsynchronizer, so at the swap the video segment picks up the new
/// group's base while the text segment stays on base 0, and every cue of the
/// new item lands seconds in the past.
///
///     thread 'gapless_switch_between_text_bearing_items_keeps_rendering'
///     panicked at crates/fcastplaybin/tests/regression_gapless.rs:490:13:
///     no rendered cue appeared after the gapless swap (text -> text, rendering)
///
/// This is the case the bug-B fix does NOT reach through
/// `Inner::poll_text_policy` alone (there is no join here to trigger it), and
/// it is why the fix also needs a trigger on the event that actually changes
/// the answer: a SEGMENT reaching subtitleoverlay's `video_sink`. Validated
/// from this harness by shimming that trigger in -- a probe that only posts a
/// ticket, plus the reconcile run off the streaming thread -- which turns this
/// test green.
#[test]
fn gapless_switch_between_text_bearing_items_keeps_rendering() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    // Visible media on both sides: this case requires a RENDERED cue after
    // the swap, not just a linked branch.
    let first = encode_av_text_visible("bothvis-a.mkv", 440);
    let second = encode_av_text_visible("bothvis-b.mkv", 880);

    assert_gapless_across(
        &first,
        &second,
        true,
        true,
        true,
        true,
        "text -> text, rendering",
    );
}

/// A stream that ENDS EARLY at the transition: the outgoing item's audio
/// stops ~0.6s before its video, so the audio pad has long since pushed its
/// EOS into decodebin3 (and had it dropped by the gapless hold) by the time
/// the video pad drains and the swap performs.
///
/// Ragged per-stream durations are ordinary in the field (a trailing silent
/// video tail, a container whose audio track is short) and nothing in
/// `tests/gapless.rs` has them: every clip there is muxed from equal-length
/// branches. The hazard is that the hold has to keep the item alive across a
/// window in which one of its streams is already finished, without ending the
/// item early and without losing the new item's audio when it arrives on that
/// same reused slot.
#[test]
fn gapless_switch_when_a_stream_ends_early() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    // 60 x 1024 @ 44.1kHz ~= 1.39s of audio against a 2s video branch.
    let first = encode_av("ragged-a.mkv", "smpte", 440, 60);
    let second = encode_av_even("ragged-b.mkv", "ball", 880);

    assert_gapless_across(&first, &second, false, false, false, false, "ragged durations");
}

// ------------------------------------ subtitles across the gapless boundary

/// A subtitle DISABLE issued while a gapless prepare is pending must survive
/// the transition: subtitles stay off across the swap and on the new item.
///
/// Neither suite covers this intersection. `tests/gapless.rs` has no
/// subtitles at all; `tests/subtitle_disable.rs` never prepares a next item,
/// so every disable it measures happens on a stable core. Here the two pieces
/// of pad surgery overlap: the disable's eager detach unlinks the text branch
/// from the overlay while `perform_gapless_swap` is relinking the whole input
/// side into the same decodebin3.
///
/// FAILS TODAY (product bug). `activate_prepared_now` resets the per-item
/// selection state "exactly like a load's reset" (`selection.lock().reset()`,
/// `last_applied_subtitle = None`), which DISCARDS the user's subtitle-off
/// desire, and nothing re-applies it: `receiver-core`'s
/// `apply_subtitle_target` is reachable only from an incoming sender packet,
/// not from the `GaplessActivated` handler, and receiver-core keeps no
/// subtitle desire of its own to replay (it mirrors the player's). With
/// `desired_subtitle` back to UNSET, `SelectionEngine::collection_changed`
/// seeds the text slot with the new collection's default, the pump dispatches
/// it, and `poll_text_policy` relinks the branch into subtitleoverlay:
///
///     thread 'subtitle_disable_survives_a_gapless_transition' panicked at
///     crates/fcastplaybin/tests/regression_gapless.rs:892:5:
///     the gapless transition relinked a subtitle stream the user turned off
///
/// The fix is in `Inner::activate_prepared_now`: a gapless boundary is the
/// crate's own decision, not a new user request, so the explicit slot
/// DISABLE has to survive it (stream ids must not — they belong to the
/// retired item).
///
/// Note that the cue-rendering half of this test PASSES: nothing actually
/// draws after the disable. That is not the disable working, it is the second
/// bug masking the first — see
/// [`subtitle_enable_after_a_gapless_transition_renders`], where the same
/// relinked branch is provably dead. MEASURED: running this sequence with no
/// post-transition enable at all and then correcting the text branch's
/// running time by hand (the bug-B fix, applied from a test probe) made 32
/// cue-bearing buffers render. Fixing that one alone really does turn this
/// into visible subtitles the user switched off.
///
/// That masking is why this case now also asserts at the overlay's TEXT INPUT
/// and not only on its output pixels. The two original assertions are "no cue
/// was drawn" and "the pad is not linked at the end", and both are blind to a
/// branch that fed the overlay all the way through the window and was taken
/// away again before the sample.
///
/// PROVEN by neutering: relink the subtitle the user switched off, push its
/// running time an hour ahead with a pad offset so nothing draws, let cue
/// buffers flow for 900ms, then disable again. That run reports
/// `relinked: false` with zero cues drawn, so BOTH original assertions pass,
/// while the arrival count is 2 and the new assertion fails 3 runs in 3.
#[test]
fn subtitle_disable_survives_a_gapless_transition() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let first = encode_av_text_visible("suboff-a.mkv", 440);
    let second = encode_av_text_visible("suboff-b.mkv", 880);

    let harness = Harness::new();
    harness.load_and_play(&first);
    harness.select_subtitles();
    let text_seen = harness.install_text_probe();
    // Not vacuous: the cue is genuinely on screen before the disable.
    harness.wait_cue_visible(&text_seen, "before the disable");

    // Arm the gapless prepare FIRST, then disable while it is pending, so the
    // disable and the swap contend.
    harness.drain_events();
    let mark = harness.log.borrow().len();
    let prepared_generation = harness
        .playbin
        .prepare_next_async(MediaInput::Uri(second.clone()));
    harness.disable_subtitles();
    let t_disable = Instant::now();

    harness.wait_for("PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    assert_no_eos_since(&harness, mark, "before the activation (subtitle disable)");
    harness.assert_worker_alive("after a disable + gapless activation");

    // Let the new item settle, pumping the receiver's settle points the whole
    // time (a relink can only happen through them).
    let settle = Instant::now() + Duration::from_millis(1500);
    while Instant::now() < settle {
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }

    // The user-visible claim first: nothing may render after the disable.
    // The 250ms margin is the eager detach's own clearing window, the same
    // allowance `tests/subtitle_disable.rs::CUE_CLEAR_BOUND` makes.
    let cutoff = t_disable + Duration::from_millis(250);
    let (rendered_after_disable, arrived_after_disable, composited_after_disable) = {
        let seen = text_seen.lock().unwrap();
        (
            seen.rendered
                .iter()
                .filter(|(at, text)| *text && *at > cutoff)
                .count(),
            seen.arrived.iter().filter(|at| **at > cutoff).count(),
            seen.rendered.iter().filter(|(at, _)| *at > cutoff).count(),
        )
    };
    let relinked = harness.overlay_subtitle_pad().is_linked();
    // A dead pipeline renders no cues either, so "zero cues" only means the
    // disable held if the overlay was COMPOSITING throughout the window the
    // claim covers. Without this the whole assertion below passes on a
    // pipeline that stopped at the boundary, which is one of the two failure
    // modes this file exists to catch.
    assert!(
        composited_after_disable > 0,
        "no video buffer left subtitleoverlay in the {:?} after the disable, so \
         'no cue rendered' says nothing: the overlay was not compositing at all",
        settle.saturating_duration_since(cutoff)
    );
    assert_eq!(
        rendered_after_disable, 0,
        "cues rendered again across the gapless boundary after a subtitle disable \
         (overlay subtitle pad relinked: {relinked}): the user's subtitle-off did \
         not survive the queue transition"
    );
    // Tighter than the pixel check and independent of it. A cue buffer that
    // reaches the overlay after a disable is the disable having failed, even
    // when nothing is drawn because the branch is on the wrong running time.
    // That combination is exactly what the captured gapless failures look
    // like, and the pixel assertion above cannot see it.
    assert_eq!(
        arrived_after_disable, 0,
        "{arrived_after_disable} cue buffers reached subtitleoverlay's subtitle_sink \
         after the disable (relinked: {relinked}): the text branch is still feeding \
         the overlay across the gapless boundary, whether or not anything was drawn"
    );
    assert!(
        !relinked,
        "the gapless transition relinked a subtitle stream the user turned off"
    );

    harness.wait_for("the final EndOfStream", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    harness.shutdown();
}

/// A subtitle ENABLE issued on the item a gapless swap just activated must
/// render. The activation resets the per-item selection state
/// (`activate_prepared_now` clears `selection` and `last_applied_subtitle`
/// "exactly like a load's reset"), so the enable that follows is acting on a
/// freshly re-seeded engine over a decodebin3 that was never rebuilt — a
/// combination no other test reaches.
///
/// This is the "linked but dead" class (`tests/subtitle_disable.rs`'s
/// `external_subtitle_reenable_renders_again`) moved onto the gapless
/// boundary, so it requires a RENDERED cue rather than a confirmed selection.
///
/// FAILS TODAY (product bug). The selection CONFIRMS and the branch links --
/// the test gets past both waits -- and then not one cue ever reaches the
/// overlay:
///
///     thread 'subtitle_enable_after_a_gapless_transition_renders' panicked at
///     crates/fcastplaybin/tests/regression_gapless.rs:490:13:
///     no rendered cue appeared after the post-transition subtitle enable
///
/// Attributed to the transition by
/// [`control_subtitle_enable_on_a_plain_load_renders`], which runs the
/// IDENTICAL media and the IDENTICAL disable-then-enable sequence on an
/// ordinary load and passes. Ruled out as timing: the items are 8s with a cue
/// every 400ms, so seconds of cue-bearing buffers follow the enable.
///
/// NOT the running-time alignment. That was the standing hypothesis and it is
/// WRONG for this case, disproven by instrumenting subtitleoverlay's three
/// pads plus decodebin3's text output (`tests/gapless_timeline.rs`) and
/// capturing four failing runs. In every one:
///
/// * the alignment is computed and applied, with the RIGHT value. The crate
///   logs `offset=8189219955 video_base=8189219955 text_base=0`, so the offset
///   is exactly the video's base and lands on decodebin3's text pad,
/// * NOT ONE cue buffer reaches the overlay. `subtitle_sink` sees no SEGMENT
///   and no buffer for the whole 30s wait,
/// * the branch is fully wired the whole time. Walking upstream from
///   `subtitle_sink` at the moment of failure:
///
///       fpb-suboverlay:subtitle_sink linked=true  no segment
///       queue2:src                   linked=true  no segment
///       queue2:sink                  linked=true  no segment
///       fpb-decodebin:text_2         linked=true  offset=8189219955 base=0
///
///   Every link is up and every element is PLAYING. decodebin3's text pad has
///   its own sticky segment and its offset, and has never pushed it: the queue
///   one hop downstream has no segment at all, which only happens if nothing
///   was ever pushed through.
///
/// So the cues never arrive, and an offset on a pad that pushes nothing is
/// beside the point. The fault is upstream of the overlay, inside decodebin3.
///
/// WHAT DRIVES IT. The crate does not set decodebin3's `auto-select-text`
/// property (added by `xtask/patches/decodebin3-auto-select-text-property.patch`
/// and never used), so decodebin3's default selection picks a text stream for
/// the collection the swap brings in. The crate answers with a corrective
/// SELECT_STREAMS that drops it, one millisecond after the group change, and
/// this test's enable re-selects it 200ms later. decodebin3 adds a text pad,
/// removes it, and adds another; the third one is the dead one. The passing
/// runs take the IDENTICAL path (verified in the debug log line for line), so
/// it is a race inside that window and not a decision the crate gets wrong.
///
/// REPRODUCTION IS LOAD-DEPENDENT and the window is not always open: 9 failures
/// in 26 runs over one twenty-minute stretch, then 0 in 104 across solo,
/// whole-binary, 8-way concurrent and CPU-loaded shapes later the same day, on
/// binaries built from the same tree. Do not read a green run as a fix.
///
/// Text bypassing streamsynchronizer IS real, and
/// [`gapless_switch_between_text_bearing_items_keeps_rendering`] is the case
/// that exercises it: there the text slot is REUSED across the boundary with
/// no deselect, so no pad is destroyed and the only thing wrong is the base.
/// This case destroys and recreates the pad, which is a different failure with
/// a different fix.
#[test]
fn subtitle_enable_after_a_gapless_transition_renders() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let first = encode_av_text_visible("subon-a.mkv", 440);
    let second = encode_av_text_visible("subon-b.mkv", 880);

    let harness = Harness::new();
    harness.load_and_play(&first);
    harness.select_subtitles();
    let text_seen = harness.install_text_probe();
    harness.wait_cue_visible(&text_seen, "before the disable");
    harness.disable_subtitles();

    harness.drain_events();
    let mark = harness.log.borrow().len();
    let prepared_generation = harness
        .playbin
        .prepare_next_async(MediaInput::Uri(second.clone()));
    harness.wait_for("PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    assert_no_eos_since(&harness, mark, "before the activation (subtitle enable)");

    // The new item's collection carries its own text stream id; select THAT.
    harness.wait_for("the new item's StreamCollection", |event, generation| {
        matches!(event, PlaybinEvent::StreamCollection(_)) && generation == prepared_generation
    });
    let t_enable = Instant::now();
    harness.select_subtitles();
    harness.assert_worker_alive("after enabling subtitles on the swapped-in item");
    // Data before pixels. `select_subtitles` proves the pipeline is WIRED (the
    // selection confirmed, the branch linked); this is the first thing that
    // proves anything moves through it. Both captured failure modes are
    // distinguishable here and only here: every failure recorded so far stops
    // at THIS assertion with zero arrivals, which rules out the overlay and
    // the running-time alignment and puts the fault upstream of the overlay.
    harness.wait_text_reaches_overlay_after(
        &text_seen,
        t_enable,
        "post-transition subtitle enable",
    );
    harness.wait_cue_visible_after(&text_seen, t_enable, "post-transition subtitle enable");

    harness.wait_for("the final EndOfStream", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    harness.shutdown();
}

/// CONTROL for [`subtitle_enable_after_a_gapless_transition_renders`]: the
/// SAME media, the SAME enable sequence, on an ordinary load instead of a
/// gapless activation.
///
/// This is what makes the gapless failure attributable. If enabling subtitles
/// renders here and not there, the difference is the transition, not the
/// media, the harness or the selection call.
#[test]
fn control_subtitle_enable_on_a_plain_load_renders() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let uri = encode_av_text_visible("plain-enable.mkv", 880);

    // Exactly the gapless test's sequence with the prepare/activate removed.
    let harness = Harness::new();
    harness.load_and_play(&uri);
    harness.select_subtitles();
    let text_seen = harness.install_text_probe();
    harness.wait_cue_visible(&text_seen, "before the disable");
    harness.disable_subtitles();

    let t_enable = Instant::now();
    harness.select_subtitles();
    harness.assert_worker_alive("after enabling subtitles on a plain load");
    harness.wait_cue_visible_after(&text_seen, t_enable, "plain-load subtitle enable");
    harness.shutdown();
}
