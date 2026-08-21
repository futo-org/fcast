//! Gapless boundary cases the existing suite (`tests/gapless.rs`) does not
//! reach, where the shape of the stream collection changes across the swap.
//!
//! The existing suite only switches between items of identical shape, carries
//! no text stream, and has no stream that ends before its siblings. Those are
//! the shapes where a gapless swap can wedge silently rather than fail
//! loudly:
//!
//! * `perform_gapless_swap` only demands a successor for video and audio. A
//!   live text slot with no successor is released instead, a path with no
//!   coverage check behind it at all.
//! * `streamsynchronizer` parks every streaming thread until each non-sparse
//!   stream has delivered its new stream-start, with no timeout. A slot that
//!   dies at the boundary, or a stream that ended early, turns that wait into a
//!   permanent park. A park is not an error or an EOS, the event stream simply
//!   goes quiet, which is why every case here also asserts that audio keeps
//!   reaching the sink.
//!
//! The media is encoded the way `tests/gapless.rs` encodes it, the
//! configuration the gapless path is known to work in. `ftest://` scenario
//! media is not usable here, see
//! `control_gapless_switch_between_identical_av_items`.

use std::{
    cell::{Cell, RefCell},
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};

#[path = "support/text_arm.rs"]
mod text_arm;
use gst::prelude::*;

/// Generous bound. The media plays in real time and the suite runs several
/// pipelines at once. A wedge must fail, not hang.
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
        // fcastaudiostretch is built by the constructor but must be
        // registered by the application.
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
    "decodebin3",
];

/// Whether the plugins these tests need are present. The skip is opt-in via
/// `FCASTPLAYBIN_ALLOW_PLUGIN_SKIP=1`, because a silent skip is
/// indistinguishable from a pass, and the skip names what is missing.
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

/// A text-bearing clip built for the cue-rendering cases. Long enough that a
/// selection confirming late still leaves seconds of buffers for a cue to
/// appear in.
fn encode_av_text_visible(name: &str, freq: u32) -> String {
    encode_av_text_sized(name, "black", freq, 640, 480, 8)
}

/// [`encode_av_text`] at an explicit resolution and length. The cases that
/// have to see a rendered cue use a longer clip, so the time a selection
/// takes to confirm cannot be mistaken for a track that never renders.
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

/// What [`Harness::install_text_probe`] watches.
///
/// Two points, not one. `rendered` is the video leaving the crate, which
/// answers the user-visible question. `arrivals` is the cue side, which
/// separates cues that reached the renderer and were not shown (a
/// running-time or state problem in the renderer) from cues that never
/// arrived at all (a dead branch upstream). Without `arrivals` a failure can
/// only report "no cue" and leave the reader to guess which.
///
/// Both points come from `support/text_arm.rs`: a video pad rejoined against
/// the cue feed, and the feed itself.
struct TextProbe {
    /// One entry per buffer leaving the video chain: when, and whether a cue
    /// was being shown on it.
    tap: text_arm::TextTap,
    /// One entry per cue reaching the renderer.
    arrivals: text_arm::TextArrivals,
    /// Video buffers seen at the sink, capped. The non-vacuity proof used by
    /// [`Harness::assert_detector_discriminates`].
    video_seen: std::sync::Arc<std::sync::Mutex<usize>>,
}

impl TextProbe {
    fn rendered(&self) -> Vec<(Instant, bool)> {
        self.tap.rendered()
    }

    fn rendered_after(&self, after: Instant) -> usize {
        self.rendered().iter().filter(|(at, _)| *at > after).count()
    }

    fn cue_shown_after(&self, after: Instant) -> bool {
        self.rendered()
            .iter()
            .any(|(at, text)| *text && *at > after)
    }

    fn arrivals_after(&self, after: Instant) -> usize {
        self.arrivals.since(after)
    }
}

/// How many video buffers the non-vacuity counter bothers to count. The
/// question it answers is "did any video flow", which the first frames settle
/// completely, so the counter stops there rather than taking its lock once per
/// frame for the rest of the run.
const CALIBRATION_BUFFERS: usize = 60;

type TextSeen = std::sync::Arc<TextProbe>;

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
        // The consumer arm's cue feed, established before anything can flow
        // (see `support/text_arm.rs`).
        text_arm::arm(&playbin);
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
        while !text_arm::text_branch_linked(&self.playbin) {
            assert!(
                Instant::now() < deadline,
                "the text branch never reached its renderer; log: {:#?}",
                self.log.borrow()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The per-buffer cue probe, the cue-arrival counter, and this file's own
    /// proof that video actually flowed. The per-buffer half is
    /// `text_arm::TextTap`, which rejoins a video buffer's running time
    /// against the cue feed.
    fn install_text_probe(&self) -> TextSeen {
        let video_seen: std::sync::Arc<std::sync::Mutex<usize>> = Default::default();
        if let Some(pad) = text_arm::video_tap_pad(&self.playbin) {
            let counter = video_seen.clone();
            pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                if info.buffer().is_some() {
                    let mut counter = counter.lock().unwrap();
                    if *counter < CALIBRATION_BUFFERS {
                        *counter += 1;
                    }
                }
                gst::PadProbeReturn::Ok
            });
        }
        std::sync::Arc::new(TextProbe {
            // The cue feed has been armed since the playbin was built,
            // because an unsynced branch hands its whole file over the moment
            // it links.
            arrivals: text_arm::count_text_arrivals(&self.playbin),
            tap: text_arm::TextTap::install(&self.playbin),
            video_seen,
        })
    }

    /// The cue must be visible before an assertion about removing it can
    /// claim anything.
    fn wait_cue_visible(&self, seen: &TextSeen, what: &str) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !seen.rendered().iter().any(|(_, text)| *text) {
            assert!(
                Instant::now() < deadline,
                "the rendered cue never appeared in the video buffers ({what})"
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        self.assert_detector_discriminates(seen, what);
    }

    /// The cue detector must have had something to look at.
    ///
    /// The detector asks whether a delivered cue covers a video buffer's
    /// running time, so with no video buffer it can only ever answer "no cue"
    /// and every absence assertion built on it would pass having measured
    /// nothing. This is the non-vacuity check.
    fn assert_detector_discriminates(&self, seen: &TextSeen, what: &str) {
        // The probes go in while video is already flowing, so wait for the
        // first buffer rather than demanding it be there already.
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while *seen.video_seen.lock().unwrap() == 0 {
            assert!(
                Instant::now() < deadline,
                "no buffer reached the video sink ({what}), so the cue \
                 detector was never given anything to answer about"
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
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
            if seen.cue_shown_after(after) {
                return;
            }
            if Instant::now() >= deadline {
                let arrived = seen.arrivals_after(after);
                let composited = seen.rendered_after(after);
                panic!(
                    "no rendered cue appeared after the {what}; in that window \
                     {arrived} cues reached the renderer and {composited} video \
                     buffers left the crate. {}",
                    if arrived == 0 {
                        "Zero arrivals: the text branch upstream of the renderer is \
                         dead, so this is not a running-time or compositing problem."
                    } else {
                        "Cues DID arrive and were not shown, so the renderer rejected \
                         them: suspect running-time alignment."
                    }
                );
            }
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Cue buffers must reach the overlay's text input after `after`.
    ///
    /// Strictly upstream of [`Self::wait_cue_visible_after`] and strictly
    /// weaker, so it fails first and names the component. A selection that
    /// confirms and a branch that links prove only that the pipeline is
    /// wired. This is the first assertion that proves data moves through it.
    fn wait_text_reaches_the_renderer_after(&self, seen: &TextSeen, after: Instant, what: &str) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while seen.arrivals_after(after) == 0 {
            assert!(
                Instant::now() < deadline,
                "not one cue reached the renderer after the {what}, though the \
                 selection confirmed and the branch linked: the text branch upstream \
                 of the renderer is delivering nothing"
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
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while text_arm::text_branch_linked(&self.playbin) {
            assert!(
                Instant::now() < deadline,
                "the subtitle branch never left its renderer"
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    /// The worker must answer a queued job within a bound. A wedged
    /// streaming thread shows up here as a job that never completes.
    fn assert_worker_alive(&self, what: &str) {
        let (tx, rx) = mpsc::channel();
        self.playbin.barrier_async(Box::new(move || {
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
/// `cues_after_swap` additionally requires that text is still rendering on
/// the far side of the boundary. Linked is not rendering. A text branch left
/// on the wrong running-time origin stays wired to its renderer and shows
/// nothing, which is invisible to every other assertion here. Only meaningful
/// with `select_text && text_after`.
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

    // The switch itself is an activation, with no pipeline EOS anywhere
    // before it. An EOS here means the boundary ended playback instead of
    // continuing it.
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
    // Both samples are taken after the timeline has rebased onto the new
    // item, so this measures progress and not the one-off backwards step the
    // rebase legitimately produces. Sampling at the activation itself does
    // not work, since with a stream that ends early the rebase can land
    // between the two samples.
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

    // Text must still be drawing on the new item. The reference instant is
    // pushed into the future on purpose. The renderer can still be holding
    // the outgoing item's last cue right at the boundary, and a cue that far
    // past the collection cannot be that straggler.
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
    let (_, eos_generation) = *harness
        .log
        .borrow()
        .last()
        .expect("the EOS was just logged");
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

/// CONTROL. Two items of identical shape (A/V, equal-length branches) over
/// this file's harness and media.
///
/// Its only job is to make the shape-change cases below mean something. If
/// this passes and one of them fails, the failure is attributable to the
/// shape change. If this fails too, the harness or the container is at fault
/// and nothing below can be attributed.
///
/// `ftest://` scenario media is unusable here. A gapless swap onto a prepared
/// `ftest://` input aborts the process inside decodebin3 on a stream-start
/// assertion, for identically shaped items too, so it discriminates nothing.
#[test]
fn control_gapless_switch_between_identical_av_items() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let first = encode_av_even("ctrl-a.mkv", "smpte", 440);
    let second = encode_av_even("ctrl-b.mkv", "ball", 880);

    assert_gapless_across(
        &first,
        &second,
        false,
        false,
        false,
        false,
        "control A/V -> A/V",
    );
}

/// A/V + TEXT -> A/V. The outgoing item's text slot has NO successor, so
/// `perform_gapless_swap` releases its decodebin3 request pad while audio and
/// video continue through reused ones.
///
/// This is the shape with no coverage check behind it. The swap only refuses
/// when a live video or audio slot lacks a successor, so a dying text slot
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
/// request pad appearing mid-stream is the input-side shape that can race the
/// routing gate.
#[test]
fn gapless_switch_into_a_text_bearing_item() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let first = encode_av_even("gain-a.mkv", "smpte", 440);
    let second = encode_av_text("gain-b.mkv", "ball", 880);

    assert_gapless_across(
        &first,
        &second,
        false,
        false,
        true,
        false,
        "no text -> text",
    );
}

/// Text on both sides, selected and live across the boundary. The text slot
/// is reused like the A/V ones, so this is the only shape where the renderer
/// sees a new SEGMENT on a live text branch with no flush in front of it,
/// the classic "element resets state on SEGMENT and swallows the boundary"
/// setup.
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
/// That distinction is the whole point. The case above can pass with a text
/// branch that composites nothing. The swap reuses the text slot, so the
/// branch is never unlinked and its link path never re-runs. Nothing in the
/// crate revisits this branch at the boundary at all.
///
/// The hazard: text bypasses streamsynchronizer, so at the swap the video
/// segment picks up the new group's base while the text segment stays on its
/// old base, and every cue of the new item lands in the past. The fix needs
/// a trigger on the event that actually changes the answer, a SEGMENT
/// reaching the video sink.
#[test]
fn gapless_switch_between_text_bearing_items_keeps_rendering() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    // Visible media on both sides. This case requires a rendered cue after
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

/// A stream that ends early at the transition. The outgoing item's audio
/// stops before its video, so the audio pad has long since pushed its EOS
/// into decodebin3 (and had it dropped by the gapless hold) by the time the
/// video pad drains and the swap performs.
///
/// Ragged per-stream durations are ordinary in the field. The hazard is that
/// the hold has to keep the item alive across a window in which one of its
/// streams is already finished, without ending the item early and without
/// losing the new item's audio when it arrives on that same reused slot.
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

    assert_gapless_across(
        &first,
        &second,
        false,
        false,
        false,
        false,
        "ragged durations",
    );
}

// ------------------------------------ subtitles across the gapless boundary

/// A subtitle disable issued while a gapless prepare is pending must survive
/// the transition. Subtitles stay off across the swap and on the new item.
///
/// The two pieces of pad surgery overlap here. The disable's eager detach
/// unlinks the text branch from the overlay while `perform_gapless_swap` is
/// relinking the whole input side into the same decodebin3.
///
/// The hazard: the activation resets the per-item selection state like a
/// load's reset, which discards the user's subtitle-off desire, and nothing
/// re-applies it. The engine then seeds the text slot with the new
/// collection's default and the branch relinks. A gapless boundary is the
/// crate's own decision, not a new user request, so the explicit slot
/// disable has to survive it while stream ids must not.
///
/// A second bug can mask the first by keeping the relinked branch from
/// drawing anything, so this case also asserts at the renderer's text input,
/// not only on rendered output. A cue buffer that reaches the renderer after
/// a disable is the disable having failed, drawn or not.
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
    // Not vacuous. The cue is genuinely on screen before the disable.
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

    // The user-visible claim first. Nothing may render after the disable.
    // The margin is the eager detach's own clearing window, the same
    // allowance `tests/subtitle_disable.rs::CUE_CLEAR_BOUND` makes.
    let cutoff = t_disable + Duration::from_millis(250);
    let (rendered_after_disable, arrived_after_disable, composited_after_disable) = (
        text_seen
            .rendered()
            .iter()
            .filter(|(at, text)| *text && *at > cutoff)
            .count(),
        text_seen.arrivals_after(cutoff),
        text_seen.rendered_after(cutoff),
    );
    let relinked = text_arm::text_branch_linked(&harness.playbin);
    // A dead pipeline renders no cues either, so "zero cues" only means the
    // disable held if the overlay was COMPOSITING throughout the window the
    // claim covers. Without this the whole assertion below passes on a
    // pipeline that stopped at the boundary, which is one of the two failure
    // modes this file exists to catch.
    assert!(
        composited_after_disable > 0,
        "no video buffer reached the video sink in the {:?} after the disable, so \
         'no cue rendered' says nothing: nothing was being displayed at all",
        settle.saturating_duration_since(cutoff)
    );
    assert_eq!(
        rendered_after_disable, 0,
        "cues rendered again across the gapless boundary after a subtitle disable \
         (text branch relinked: {relinked}): the user's subtitle-off did \
         not survive the queue transition"
    );
    // Tighter than the pixel check and independent of it. A cue buffer that
    // reaches the overlay after a disable is the disable having failed, even
    // when nothing is drawn because the branch is on the wrong running time.
    assert_eq!(
        arrived_after_disable, 0,
        "{arrived_after_disable} cue buffers reached the renderer \
         after the disable (relinked: {relinked}): the text branch is still feeding \
         the renderer across the gapless boundary, whether or not anything was drawn"
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

/// A subtitle enable issued on the item a gapless swap just activated must
/// render. The activation resets the per-item selection state, so the enable
/// acts on a freshly re-seeded engine over a decodebin3 that was never
/// rebuilt, a combination no other test reaches. It requires a rendered cue
/// rather than a confirmed selection.
///
/// The known failure mode is upstream of the overlay, inside decodebin3. The
/// selection confirms and the branch links, yet the re-added text pad pushes
/// nothing at all, not even its sticky segment, while every link is up and
/// every element is PLAYING. It is a race in the window where a
/// default-selected text pad is dropped and re-selected, not a running-time
/// alignment problem and not a decision the crate gets wrong.
/// [`control_subtitle_enable_on_a_plain_load_renders`] attributes it to the
/// transition. Reproduction is load-dependent, so a green run is not a fix.
///
/// [`gapless_switch_between_text_bearing_items_keeps_rendering`] covers the
/// separate reused-slot case where only the segment base is wrong. This case
/// destroys and recreates the pad, a different failure with a different fix.
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

    // The new item's collection carries its own text stream id. Select that.
    harness.wait_for("the new item's StreamCollection", |event, generation| {
        matches!(event, PlaybinEvent::StreamCollection(_)) && generation == prepared_generation
    });
    let t_enable = Instant::now();
    harness.select_subtitles();
    harness.assert_worker_alive("after enabling subtitles on the swapped-in item");
    // Data before pixels. `select_subtitles` proves the pipeline is wired.
    // This is the first thing that proves anything moves through it, and a
    // zero-arrival failure here rules out the overlay and the running-time
    // alignment, putting the fault upstream of the overlay.
    harness.wait_text_reaches_the_renderer_after(
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

/// CONTROL for [`subtitle_enable_after_a_gapless_transition_renders`]. The
/// same media and enable sequence, on an ordinary load instead of a gapless
/// activation.
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
