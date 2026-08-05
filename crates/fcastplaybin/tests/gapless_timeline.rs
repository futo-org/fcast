//! Running-time instrumentation for the gapless subtitle boundary.
//!
//! `tests/regression_gapless.rs` can say only "a cue rendered" or "no cue
//! rendered". When the answer is no, that tells you nothing about WHY: the
//! selection may never have dispatched, the branch may never have linked, the
//! cue buffers may never have arrived, or they may have arrived on a running
//! time nowhere near the video's. Those four call for four different fixes.
//!
//! This file watches subtitleoverlay's own three pads and records what crosses
//! them, so a failure prints the timeline instead of a bare "no cue". The
//! measurement that matters is the SEGMENT pair: subtitleoverlay composites by
//! RUNNING TIME, so a cue renders only while the video buffer being composited
//! against has a running time inside the cue's own [pts, pts+duration) run.
//! Both sides of that comparison are read here from the sticky segments on the
//! pads themselves, which is the only place the answer is not a guess.
//!
//! Reading the segments off the OVERLAY's pads, rather than off the decodebin3
//! pads the crate aligns, is deliberate. `gst_pad_set_offset` applies its
//! offset on the way OUT to the peer (gstpad.c `gst_pad_push_event_unchecked`
//! calls `apply_pad_offset` before the peer's event function, and the src pad's
//! own stored sticky event keeps the raw base), so the decodebin3 text pad
//! reports the base it would have had WITHOUT the alignment. The overlay's
//! `subtitle_sink` is downstream of the offset and reports the base the overlay
//! actually composites with.

use std::{
    cell::{Cell, RefCell},
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use gst::prelude::*;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);
const TEARDOWN_BOUND: Duration = Duration::from_secs(15);

/// Seconds per item. Matches `regression_gapless.rs`'s visible media, so the
/// timeline this file captures describes the same failure that file sees.
const CLIP_SECONDS: u32 = 8;

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        gst::init().unwrap();
        fcast_gst_elements::fcastaudiostretch::plugin_init().expect("registering fcastaudiostretch");
    });
}

/// Whether the plugins this file needs are present. The skip is OPT-IN, for
/// the reason `regression_gapless.rs::require_plugins` documents: a silent
/// skip reports a green run that measured nothing, which for a file whose only
/// job is measurement is worse than useless.
fn require_plugins() -> bool {
    let missing: Vec<&str> = [
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
    ]
    .into_iter()
    .filter(|f| gst::ElementFactory::find(f).is_none())
    .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        std::env::var_os("FCASTPLAYBIN_ALLOW_PLUGIN_SKIP").is_some(),
        "required GStreamer plugins are missing: {missing:?}. Set \
         FCASTPLAYBIN_ALLOW_PLUGIN_SKIP=1 to skip instead of failing."
    );
    eprintln!("skipping: required GStreamer plugins missing: {missing:?}");
    false
}

fn tmp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "fcastplaybin-gapless-timeline-{}-{}",
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

/// Dense back-to-back cues, one every 400ms, so cue-bearing buffers exist all
/// the way through the clip and a missing render cannot be a gap in the media.
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

/// 640x480 black video plus a muxed SubRip track, the media
/// `regression_gapless.rs::encode_av_text_visible` builds: black so a rendered
/// cue is detectable as white glyphs in the luma plane.
fn encode_av_text_visible(name: &str, freq: u32) -> String {
    let srt = write_srt(&format!("{name}.srt"), CLIP_SECONDS);
    let path = tmp_path(name);
    let desc = format!(
        "videotestsrc num-buffers={} pattern=black \
           ! video/x-raw,width=640,height=480,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         audiotestsrc num-buffers={} freq={freq} \
           ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! vorbisenc ! mux. \
         filesrc location={} ! subparse ! mux. \
         matroskamux name=mux ! filesink location={}",
        CLIP_SECONDS * 30,
        CLIP_SECONDS * 44,
        srt.display(),
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

// ------------------------------------------------------------------ timeline

/// One observation from a pad probe or the test body.
#[derive(Debug)]
enum Mark {
    /// A SEGMENT crossed `at`. `base` and `start` are what subtitleoverlay
    /// composites with; `origin` is `start - base`, the value
    /// `scenarios.rs::segment_origin` reports.
    Segment {
        at: String,
        base: u64,
        start: u64,
        rate: f64,
    },
    /// Any other event, by name.
    Event { at: String, name: String },
    /// A buffer crossed `at`, with the running time the pad's CURRENT sticky
    /// segment maps its pts onto. `None` means the segment could not map it,
    /// which is itself the answer: the buffer is outside the segment.
    Buffer {
        at: String,
        pts: Option<u64>,
        duration: Option<u64>,
        running: Option<u64>,
    },
    /// The overlay's output changed between "carries a cue" and "does not".
    Render { rendering: bool, running: Option<u64> },
    /// A note from the test body.
    Note(String),
}

/// Ordered observations, each stamped with the elapsed time since the capture
/// started, so the dump reads as a timeline.
#[derive(Clone)]
struct Timeline {
    start: Instant,
    marks: Arc<Mutex<Vec<(Duration, Mark)>>>,
}

impl Timeline {
    fn new() -> Self {
        Self {
            start: Instant::now(),
            marks: Default::default(),
        }
    }

    fn push(&self, mark: Mark) {
        self.marks
            .lock()
            .unwrap()
            .push((self.start.elapsed(), mark));
    }

    fn note(&self, what: impl Into<String>) {
        self.push(Mark::Note(what.into()));
    }

    /// The whole capture, one mark per line, for a panic message.
    fn dump(&self) -> String {
        let marks = self.marks.lock().unwrap();
        let mut out = String::from("\n--- subtitleoverlay timeline ---\n");
        for (at, mark) in marks.iter() {
            out.push_str(&format!("{:>8.3}s  ", at.as_secs_f64()));
            match mark {
                Mark::Segment {
                    at, base, start, rate,
                } => out.push_str(&format!(
                    "SEGMENT   {at:<22} base={} start={} origin={} rate={rate}\n",
                    ms(*base),
                    ms(*start),
                    ms(start.saturating_sub(*base)),
                )),
                Mark::Event { at, name } => out.push_str(&format!("event     {at:<22} {name}\n")),
                Mark::Buffer {
                    at,
                    pts,
                    duration,
                    running,
                } => out.push_str(&format!(
                    "buffer    {at:<22} pts={} dur={} running={}\n",
                    opt(*pts),
                    opt(*duration),
                    opt(*running)
                )),
                Mark::Render { rendering, running } => out.push_str(&format!(
                    "RENDER    {:<22} running={}\n",
                    if *rendering { "cue on" } else { "cue off" },
                    opt(*running)
                )),
                Mark::Note(what) => out.push_str(&format!("note      {what}\n")),
            }
        }
        out
    }

    /// Whether a cue-bearing buffer left the overlay after `after`.
    fn rendered_after(&self, after: Duration) -> bool {
        self.marks.lock().unwrap().iter().any(|(at, mark)| {
            *at > after
                && matches!(
                    mark,
                    Mark::Render {
                        rendering: true,
                        ..
                    }
                )
        })
    }
}

fn ms(ns: u64) -> String {
    format!("{:.3}s", ns as f64 / 1e9)
}

fn opt(ns: Option<u64>) -> String {
    ns.map(ms).unwrap_or_else(|| "-".to_owned())
}

/// The running time the pad's current sticky segment maps `pts` onto. This is
/// exactly the arithmetic subtitleoverlay does, so a text buffer renders only
/// when this lands inside the video's.
fn running_time(pad: &gst::Pad, pts: Option<gst::ClockTime>) -> Option<u64> {
    let event = pad.sticky_event::<gst::event::Segment>(0)?;
    let segment = event.segment().downcast_ref::<gst::ClockTime>()?;
    segment.to_running_time(pts?).map(|t| t.nseconds())
}

fn segment_mark(at: &str, event: &gst::event::Segment) -> Option<Mark> {
    let segment = event.segment().downcast_ref::<gst::ClockTime>()?;
    Some(Mark::Segment {
        at: at.to_owned(),
        base: segment.base().unwrap_or(gst::ClockTime::ZERO).nseconds(),
        start: segment.start().unwrap_or(gst::ClockTime::ZERO).nseconds(),
        rate: segment.rate(),
    })
}

/// Record every event and buffer crossing `pad` under the label `at`.
///
/// `EVENT_FLUSH` is in the mask on purpose. gstpad.c only runs a probe on a
/// flush event when the probe asked for that bit, so `EVENT_DOWNSTREAM` alone
/// silently omits the flush pairs, and the flush pairs are exactly what the
/// text detach and relink send.
fn watch_pad(timeline: &Timeline, pad: &gst::Pad, at: &str) {
    let label = at.to_owned();
    let events = timeline.clone();
    pad.add_probe(
        gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::EVENT_FLUSH,
        move |_, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data {
                match event.view() {
                    gst::EventView::Segment(segment) => {
                        if let Some(mark) = segment_mark(&label, segment) {
                            events.push(mark);
                        }
                    }
                    _ => events.push(Mark::Event {
                        at: label.clone(),
                        name: format!("{:?}", event.type_()),
                    }),
                }
            }
            gst::PadProbeReturn::Ok
        },
    );
    let label = at.to_owned();
    let buffers = timeline.clone();
    pad.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
        if let Some(buffer) = info.buffer() {
            buffers.push(Mark::Buffer {
                at: label.clone(),
                pts: buffer.pts().map(|t| t.nseconds()),
                duration: buffer.duration().map(|t| t.nseconds()),
                running: running_time(pad, buffer.pts()),
            });
        }
        gst::PadProbeReturn::Ok
    });
}

/// The first element in `pipeline` built from `factory`, searched recursively
/// (decodebin3 is nested inside the crate's own bin).
fn find_by_factory(pipeline: &gst::Pipeline, factory: &str) -> Option<gst::Element> {
    let mut iter = pipeline.iterate_recurse();
    loop {
        match iter.next() {
            Ok(Some(element)) => {
                if element
                    .factory()
                    .is_some_and(|f| f.name() == factory)
                {
                    return Some(element);
                }
            }
            Ok(None) => return None,
            Err(gst::IteratorError::Resync) => iter.resync(),
            Err(_) => return None,
        }
    }
}

/// Everything upstream of `pad`, hop by hop, as one line each: which pad, in
/// which element, in which state, carrying which segment. Answers "the buffers
/// never arrived, so where did they stop" without a debug graph.
fn upstream_chain(pad: &gst::Pad) -> String {
    let mut out = String::from("\n--- chain upstream of the overlay's subtitle_sink ---\n");
    let mut current = Some(pad.clone());
    for _ in 0..12 {
        let Some(here) = current else { break };
        let parent = here
            .parent_element()
            .map(|e| {
                let (_, state, pending) = e.state(gst::ClockTime::ZERO);
                format!("{} [{state:?}/{pending:?}]", e.name())
            })
            .unwrap_or_else(|| "<no parent>".to_owned());
        let segment = here
            .sticky_event::<gst::event::Segment>(0)
            .and_then(|event| {
                event
                    .segment()
                    .downcast_ref::<gst::ClockTime>()
                    .map(|s| {
                        format!(
                            "base={} start={}",
                            ms(s.base().unwrap_or(gst::ClockTime::ZERO).nseconds()),
                            ms(s.start().unwrap_or(gst::ClockTime::ZERO).nseconds())
                        )
                    })
            })
            .unwrap_or_else(|| "no segment".to_owned());
        out.push_str(&format!(
            "  {}:{} offset={} linked={} {segment}\n",
            parent,
            here.name(),
            here.offset(),
            here.is_linked()
        ));
        current = match here.direction() {
            // Walk sink -> its peer src pad -> that element's sink pads.
            gst::PadDirection::Sink => here.peer(),
            _ => here
                .parent_element()
                .and_then(|e| e.sink_pads().into_iter().next()),
        };
    }
    out
}

// ------------------------------------------------------------------- harness

struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<(PlaybinEvent, u64)>>,
    paused: Cell<bool>,
    timeline: Timeline,
}

impl Harness {
    fn new() -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: None,
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
            timeline: Timeline::new(),
        }
    }

    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: false,
        }
    }

    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(self.gate());
    }

    fn drain_events(&self) {
        while let Ok(entry) = self.events.try_recv() {
            self.log.borrow_mut().push(entry);
        }
    }

    fn wait_for(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent, u64) -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                panic!("timed out waiting for {what}{}", self.timeline.dump());
            }
            self.settle_pump();
            match self.events.recv_timeout(Duration::from_millis(20)) {
                Ok((event, generation)) => {
                    if let PlaybinEvent::Error { error, .. } = &event {
                        panic!(
                            "pipeline error while waiting for {what}: {error}{}",
                            self.timeline.dump()
                        );
                    }
                    let hit = pred(&event, generation);
                    self.log.borrow_mut().push((event, generation));
                    if hit {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => {
                    panic!("event channel closed while waiting for {what}")
                }
            }
        }
    }

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

    /// subtitleoverlay is built with the video chain, so a settled PLAYING is
    /// not proof it is in the pipeline yet. Wait for it rather than racing it.
    fn overlay(&self) -> gst::Element {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if let Some(overlay) = self.playbin.pipeline().by_name("fpb-suboverlay") {
                return overlay;
            }
            assert!(
                Instant::now() < deadline,
                "subtitleoverlay never appeared in the pipeline{}",
                self.timeline.dump()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(5));
        }
    }

    fn overlay_pad(&self, name: &str) -> gst::Pad {
        self.overlay()
            .static_pad(name)
            .unwrap_or_else(|| panic!("subtitleoverlay has a {name} pad"))
    }

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

    fn select_subtitles(&self) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let sid = loop {
            self.drain_events();
            if let Some(sid) = self.text_sid() {
                break sid;
            }
            assert!(
                Instant::now() < deadline,
                "the collection never advertised a text stream{}",
                self.timeline.dump()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        };
        self.timeline.note(format!("request subtitle {sid}"));
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
                "the text stream never confirmed as selected{}",
                self.timeline.dump()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        self.timeline.note("subtitle selection confirmed");
        let pad = self.overlay_pad("subtitle_sink");
        while !pad.is_linked() {
            assert!(
                Instant::now() < deadline,
                "the text branch never reached the overlay{}",
                self.timeline.dump()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        self.timeline.note("subtitle branch linked into the overlay");
    }

    fn disable_subtitles(&self) {
        self.timeline.note("request subtitle off");
        self.playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
        self.playbin.pump_selection(self.gate());
        let pad = self.overlay_pad("subtitle_sink");
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while pad.is_linked() {
            assert!(
                Instant::now() < deadline,
                "the subtitle branch never left the overlay{}",
                self.timeline.dump()
            );
            self.settle_pump();
            std::thread::sleep(Duration::from_millis(5));
        }
        self.timeline.note("subtitle branch left the overlay");
    }

    /// Watch all three of subtitleoverlay's pads.
    ///
    /// Video buffers are sampled rather than recorded whole: at 30fps over two
    /// 8s items they would bury everything else, and one every half second is
    /// enough to say where the video running time is when a cue does or does
    /// not appear. Text buffers are recorded in full (one cue per 400ms).
    /// A/B lever for the diagnosis in
    /// [`gapless_subtitle_enable_running_time_timeline`].
    ///
    /// The receiver ships
    /// `xtask/patches/decodebin3-auto-select-text-property.patch`, which adds
    /// an `auto-select-text` property to decodebin3, and the crate never sets
    /// it. So decodebin3's default selection picks a TEXT stream for every new
    /// collection, including the one a gapless swap brings in, and the crate
    /// answers with a corrective SELECT_STREAMS that drops it again. That
    /// deselect lands within a millisecond of the group change, and the
    /// reselect a fifth of a second later, which is the churn this test's
    /// failures run through.
    ///
    /// Setting the property here isolates that churn from everything else on
    /// the boundary: nothing about the media, the timing or the crate's own
    /// sequence changes, only whether decodebin3 volunteers the text stream.
    /// Set on one binary via the env lever so the comparison can be
    /// INTERLEAVED, which on this machine is the only kind that means
    /// anything.
    fn apply_auto_select_text_lever(&self, db3: &gst::Element) {
        if std::env::var_os("FCAST_TL_NO_AUTO_TEXT").is_none() {
            return;
        }
        if db3.has_property("auto-select-text") {
            db3.set_property("auto-select-text", false);
            self.timeline.note("decodebin3 auto-select-text set to false");
        } else {
            // Unpatched GStreamer. Say so rather than silently measuring
            // nothing: an A/B where the lever does not exist reports two
            // identical arms and looks like "no effect".
            self.timeline
                .note("decodebin3 has NO auto-select-text property (unpatched build)");
        }
    }

    fn install_probes(&self) {
        let video_sink = self.overlay_pad("video_sink");
        let subtitle_sink = self.overlay_pad("subtitle_sink");
        let src = self.overlay_pad("src");

        watch_pad(&self.timeline, &subtitle_sink, "overlay:subtitle_sink");

        // Video events only. Its buffers are sampled below, because at 30fps
        // over two 8s items every one of them would bury everything else, and
        // one every half second already says where the video running time is.
        let timeline = self.timeline.clone();
        video_sink.add_probe(
            gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::EVENT_FLUSH,
            move |_, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data {
                    match event.view() {
                        gst::EventView::Segment(segment) => {
                            if let Some(mark) = segment_mark("overlay:video_sink", segment) {
                                timeline.push(mark);
                            }
                        }
                        _ => timeline.push(Mark::Event {
                            at: "overlay:video_sink".to_owned(),
                            name: format!("{:?}", event.type_()),
                        }),
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );

        let timeline = self.timeline.clone();
        let counter = Arc::new(Mutex::new(0u64));
        video_sink.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(buffer) = info.buffer() {
                let mut n = counter.lock().unwrap();
                *n += 1;
                if *n % 15 == 1 {
                    timeline.push(Mark::Buffer {
                        at: "overlay:video_sink".to_owned(),
                        pts: buffer.pts().map(|t| t.nseconds()),
                        duration: buffer.duration().map(|t| t.nseconds()),
                        running: running_time(pad, buffer.pts()),
                    });
                }
            }
            gst::PadProbeReturn::Ok
        });

        // decodebin3's own text output, the pad the crate sets its alignment
        // offset on. This is the discriminator: if cues cross here but not the
        // overlay's subtitle_sink, the branch between them swallowed them; if
        // they never cross here at all, decodebin3 never produced them and the
        // alignment is beside the point.
        if let Some(db3) = find_by_factory(self.playbin.pipeline(), "decodebin3") {
            for pad in db3.src_pads() {
                if pad.name().starts_with("text") {
                    watch_pad(&self.timeline, &pad, &format!("db3:{}", pad.name()));
                }
            }
            let timeline = self.timeline.clone();
            db3.connect_pad_added(move |_, pad| {
                if pad.name().starts_with("text") {
                    timeline.note(format!("decodebin3 added {}", pad.name()));
                    watch_pad(&timeline, pad, &format!("db3:{}", pad.name()));
                }
            });
            let timeline = self.timeline.clone();
            db3.connect_pad_removed(move |_, pad| {
                if pad.name().starts_with("text") {
                    timeline.note(format!("decodebin3 removed {}", pad.name()));
                }
            });
            self.apply_auto_select_text_lever(&db3);
        }

        // Only the TRANSITIONS are recorded: one mark per buffer would be 480
        // lines of noise, while "the cue came on at 3.2s and went off at 3.6s"
        // is the whole content.
        let timeline = self.timeline.clone();
        let last = Arc::new(Mutex::new(false));
        src.add_probe(gst::PadProbeType::BUFFER, move |pad, info| {
            if let Some(buffer) = info.buffer() {
                let has_meta = buffer
                    .iter_meta::<gst::Meta>()
                    .any(|meta| meta.api().name().contains("VideoOverlayComposition"));
                let has_pixels = !has_meta
                    && buffer.map_readable().is_ok_and(|map| {
                        let luma = &map[..map.len().min(640 * 480)];
                        luma.iter().filter(|&&y| y > 128).count() >= 10
                    });
                let rendering = has_meta || has_pixels;
                let mut last = last.lock().unwrap();
                if *last != rendering {
                    *last = rendering;
                    timeline.push(Mark::Render {
                        rendering,
                        running: running_time(pad, buffer.pts()),
                    });
                }
            }
            gst::PadProbeReturn::Ok
        });
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

/// The failing sequence from
/// `regression_gapless.rs::subtitle_enable_after_a_gapless_transition_renders`,
/// carrying the full pad instrumentation.
///
/// Same claim as that test, so it fails exactly when that one does. The
/// difference is the failure message: this prints every SEGMENT, every text
/// buffer and every render transition at subtitleoverlay's pads, which says
/// which of the four candidate causes it is (never dispatched, never linked,
/// never arrived, or arrived on the wrong running time).
#[test]
fn gapless_subtitle_enable_running_time_timeline() {
    init();
    if !require_plugins() {
        eprintln!("skipping: required GStreamer plugins missing");
        return;
    }
    let first = encode_av_text_visible("tl-a.mkv", 440);
    let second = encode_av_text_visible("tl-b.mkv", 880);

    let harness = Harness::new();
    harness.load_and_play(&first);
    harness.install_probes();
    harness.select_subtitles();

    // The cue must be on screen BEFORE anything else, so the media and the
    // detection are both proven on this very run.
    let deadline = Instant::now() + EVENT_TIMEOUT;
    while !harness.timeline.rendered_after(Duration::ZERO) {
        assert!(
            Instant::now() < deadline,
            "no cue rendered before the disable{}",
            harness.timeline.dump()
        );
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.disable_subtitles();

    harness.drain_events();
    let prepared_generation = harness
        .playbin
        .prepare_next_async(MediaInput::Uri(second.clone()));
    harness.timeline.note("prepare_next issued");
    harness.wait_for("PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    harness.timeline.note("PreparedActivated");
    harness.wait_for("the new item's StreamCollection", |event, generation| {
        matches!(event, PlaybinEvent::StreamCollection(_)) && generation == prepared_generation
    });
    harness.timeline.note("the new item's collection arrived");

    let enabled_at = harness.timeline.start.elapsed();
    harness.select_subtitles();

    let deadline = Instant::now() + EVENT_TIMEOUT;
    while !harness.timeline.rendered_after(enabled_at) {
        if Instant::now() >= deadline {
            let dump = format!(
                "{}{}",
                harness.timeline.dump(),
                upstream_chain(&harness.overlay_pad("subtitle_sink"))
            );
            harness.shutdown();
            panic!("no cue rendered after the post-transition subtitle enable{dump}");
        }
        harness.settle_pump();
        std::thread::sleep(Duration::from_millis(10));
    }
    harness.shutdown();
}
