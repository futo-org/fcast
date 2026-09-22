//! Subtitle cues on the desktop lane: the engine's display lists, put in
//! front of the dodvg renderer as its own scene type. Driven from the video
//! sink's presented callback (`element_cues::ElementCues`).
//!
//! The translation and the pooling are `i_slint_cue::engine`'s, because every
//! consumer on this lane needs the same three hundred lines. What is left here
//! is the sink: where a published overlay goes on a real window.

use std::sync::Arc;

use slint::winit_030::cues;

pub(crate) use i_slint_cue::engine::{CueOverlay, CueSink};

/// The dodvg renderer behind a real window.
pub(crate) struct WindowCues<'a>(pub(crate) &'a slint::Window);

impl CueSink for WindowCues<'_> {
    fn set_scene(&self, scene: Option<Arc<cues::CueScene>>) {
        use slint::winit_030::DodvgWindowAccessor;
        if self
            .0
            .with_dodvg_renderer(|r| r.set_cue_overlay(scene))
            .is_none()
        {
            // Not a winit window, or not a dodvg renderer. Nothing to draw on,
            // and a subtitle that never appears is worth exactly one line.
            static ONCE: std::sync::Once = std::sync::Once::new();
            ONCE.call_once(|| {
                tracing::warn!("cue overlay: no dodvg renderer, cues will not be drawn")
            });
        }
    }

    fn set_rank(&self, rank: u16) {
        use slint::winit_030::DodvgWindowAccessor;
        self.0.with_dodvg_renderer(|r| r.set_cue_reveal_rank(rank));
    }
}

/// The harness the lane's own gates below drive. The translation suite that
/// used to live here went with the translation, into `i_slint_cue::engine`.
#[cfg(test)]
pub(crate) mod tests {
    use std::{
        cell::RefCell,
        time::{Duration, Instant},
    };

    use fcast_video::cue::{CueEngine, CueInput, ShownScene, TextFormat};

    use super::*;

    /// A [`CueSink`] that behaves like the renderer's overlay slot: it HOLDS
    /// exactly what it was last handed and drops the rest. Everything else is
    /// a log.
    #[derive(Default)]
    pub(crate) struct Recorder {
        slot: RefCell<Option<Arc<cues::CueScene>>>,
        /// Every scene ever put on the slot, by address, and every take-down
        /// as a zero.
        pub(crate) sets: RefCell<Vec<usize>>,
        pub(crate) ranks: RefCell<Vec<u16>>,
    }

    impl Recorder {
        pub(crate) fn last(&self) -> Arc<cues::CueScene> {
            self.slot
                .borrow()
                .clone()
                .expect("a scene must be on the slot")
        }
    }

    impl CueSink for Recorder {
        fn set_scene(&self, scene: Option<Arc<cues::CueScene>>) {
            self.sets
                .borrow_mut()
                .push(scene.as_ref().map_or(0, |s| Arc::as_ptr(s) as usize));
            *self.slot.borrow_mut() = scene;
        }
        fn set_rank(&self, rank: u16) {
            self.ranks.borrow_mut().push(rank);
        }
    }

    pub(crate) const CANVAS: (u32, u32) = (1280, 720);
    pub(crate) const AT: u64 = 1;

    pub(crate) fn cue(text: &str, start_s: u64, end_s: u64) -> CueInput {
        CueInput {
            format: TextFormat::Utf8,
            text: text.to_owned(),
            start_rt: gst::ClockTime::from_seconds(start_s),
            end_rt: Some(gst::ClockTime::from_seconds(end_s)),
        }
    }

    /// A warm engine on the scene lane, and the display lists it shows at
    /// [`AT`]. The wait is the same one the cue-IR suite uses: layout happens
    /// on the engine's own worker on every lane.
    pub(crate) fn shown(cues: &[CueInput]) -> (CueEngine, Vec<ShownScene>) {
        gst::init().unwrap();
        let engine = CueEngine::for_scene_consumer();
        engine.set_canvas(CANVAS.0, CANVAS.1);
        let wanted = cues.len();
        for cue in cues {
            engine.submit(cue.clone());
        }
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let shown = engine.scenes_for(Some(gst::ClockTime::from_seconds(AT)));
            if shown.len() == wanted {
                return (engine, shown.into_vec());
            }
            assert!(Instant::now() < deadline, "the cue scenes never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    pub(crate) fn one(text: &str) -> (CueEngine, ShownScene) {
        let (engine, mut shown) = shown(&[cue(text, 0, 10)]);
        (engine, shown.remove(0))
    }
}

#[cfg(test)]
mod wave7_gate {
    use std::time::{Duration, Instant};

    use fcast_video::cue::{CueEngine, CueInput};

    use super::{
        tests::{AT, CANVAS, Recorder, cue},
        *,
    };

    /// A warm engine on the RASTER lane, which is what the desktop ran before
    /// this wave: vello_cpu paints every cue and `current_overlays` is what
    /// the video compositor uploaded.
    fn painted(cues: &[CueInput]) -> CueEngine {
        gst::init().unwrap();
        let engine = CueEngine::new();
        engine.set_canvas(CANVAS.0, CANVAS.1);
        for cue in cues {
            engine.submit(cue.clone());
        }
        let at = gst::ClockTime::from_seconds(AT);
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            if engine.overlays_for(Some(at)).len() == cues.len() {
                return engine;
            }
            assert!(Instant::now() < deadline, "the cue rasters never arrived");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    /// Link 1 and link 2 on one cue: the display list the renderer is handed
    /// repaints to the exact pixels libplacebo was given, at the exact place it
    /// was given them.
    #[test]
    fn the_scene_handed_over_is_the_scene_the_raster_lane_painted() {
        let engine = painted(&[cue("the quick brown fox, 42%", 0, 10)]);
        let overlays = engine.current_overlays();
        let shown = engine.shown_scenes();
        assert_eq!(overlays.len(), 1, "one cue, one overlay");
        assert_eq!(shown.len(), 1, "one cue, one display list");
        let (overlay, shown) = (&overlays[0], &shown[0]);
        assert!(overlay.pixels.len() > 4, "the raster lane painted nothing");
        assert!(
            shown.scene.glyph_count() > 0,
            "the display list carries no glyphs"
        );

        assert_eq!(
            (overlay.x, overlay.y),
            (shown.x, shown.y),
            "the scene lane would place the cue somewhere else on screen"
        );
        assert_eq!(
            (overlay.width, overlay.height),
            (shown.scene.size[0], shown.scene.size[1]),
            "the display list's surface is not the one that was uploaded"
        );

        // The pixels, from the display list the renderer gets. Equal means the
        // switch replaced the painter and nothing upstream of it.
        let repaint = fcast_video::cue_scene::VelloBackend::new()
            .rasterize(&shown.scene, shown.rank)
            .expect("the scene the raster lane painted must repaint");
        assert_eq!(
            (repaint.width, repaint.height, repaint.x, repaint.y),
            (overlay.width, overlay.height, overlay.x, overlay.y),
        );
        assert_eq!(
            repaint.pixels.as_slice(),
            overlay.pixels.as_slice(),
            "the display list handed to the renderer paints different pixels than the \
             raster lane composited, so the two lanes are not showing the same cue"
        );

        // And that display list is what lands on the overlay slot, at the
        // placement the raster lane used.
        let recorder = Recorder::default();
        let mut published = CueOverlay::default();
        assert!(published.sync(&recorder, &engine.shown_scenes()));
        let scene = recorder.last();
        assert_eq!(scene.origin, [overlay.x, overlay.y]);
        assert_eq!(scene.size, [overlay.width, overlay.height]);
        assert_eq!(scene.glyph_count(), shown.scene.glyph_count());
        assert_eq!(scene.rect_count(), shown.scene.rect_count());
    }

    /// Link 2 with the stacking pass in it, which is the half of placement the
    /// scene does not carry: two cues at the same anchor, so the second is
    /// pushed off the first by `stacked_y`.
    #[test]
    fn a_stack_lands_where_the_raster_lane_stacked_it() {
        let engine = painted(&[cue("upper line", 0, 10), cue("lower line", 0, 10)]);
        let overlays = engine.current_overlays();
        let shown = engine.shown_scenes();
        assert_eq!(overlays.len(), 2, "both cues must be on screen");
        assert_eq!(shown.len(), 2);
        for (i, (overlay, shown)) in overlays.iter().zip(shown.iter()).enumerate() {
            assert_eq!(
                (overlay.x, overlay.y),
                (shown.x, shown.y),
                "cue {i} of the stack moved when the lane changed"
            );
            assert_eq!(
                (overlay.width, overlay.height),
                (shown.scene.size[0], shown.scene.size[1]),
                "cue {i} of the stack changed size when the lane changed"
            );
        }
        assert_ne!(
            shown[0].y, shown[1].y,
            "the two cues did not stack, so this proves nothing about stacking"
        );
    }

    /// The lane itself: a scene consumer pays for no pixels and still gets the
    /// display list, which is the whole reason the lane exists.
    #[test]
    fn the_scene_lane_pays_for_no_pixels_and_still_shows_the_cue() {
        let line = cue("a line that was painted", 0, 10);
        let painting = painted(&[line.clone()]);
        assert!(painting.cached_pixels() > 0, "the raster lane painted nothing");

        gst::init().unwrap();
        let engine = CueEngine::for_scene_consumer();
        engine.set_canvas(CANVAS.0, CANVAS.1);
        engine.submit(line);
        let at = gst::ClockTime::from_seconds(AT);
        let deadline = Instant::now() + Duration::from_secs(60);
        let shown = loop {
            let shown = engine.scenes_for(Some(at));
            if !shown.is_empty() {
                break shown;
            }
            assert!(Instant::now() < deadline, "the cue's display list never arrived");
            std::thread::sleep(Duration::from_millis(10));
        };

        assert_eq!(shown.len(), 1, "the cue never reached the screen");
        assert!(shown[0].scene.glyph_count() > 0, "the display list carries no glyphs");
        assert_eq!(engine.cached_pixels(), 0, "the scene lane painted anyway");
        assert!(
            engine.current_overlays().is_empty(),
            "a scene consumer's text cue must not also be composited as pixels"
        );
    }
}

/// The allocation gate for wave 6: a cue on screen must cost the frame path
/// nothing at all.
///
/// Separate from the suite above because it needs the counting allocator in
/// `crate::alloc_counter`, and because it is the claim the plan makes
/// (`FRAME_ALLOC_BUDGET` unchanged with cues on screen) rather than a property
/// of the translation.
#[cfg(all(test, not(target_os = "android")))]
mod alloc_gate {
    use std::time::{Duration, Instant};

    use crate::alloc_counter::measure;

    use super::{
        tests::{AT, Recorder, cue, one, shown},
        *,
    };

    /// The whole lane-side cue path per frame: advance the schedule, compare
    /// the shown set, publish nothing. Zero, not "small".
    #[test]
    fn a_steady_displayed_cue_costs_the_frame_nothing() {
        let (engine, first) = one("a steady subtitle line");
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        let at = gst::ClockTime::from_seconds(AT);
        // settle: the first pass publishes, and the SmallVec/Vec capacities the
        // steady state reuses are grown here
        for _ in 0..4 {
            let shown = engine.scenes_for(Some(at));
            overlay.sync(&recorder, &shown);
        }
        assert_eq!(
            overlay.builds(),
            1,
            "the settle pass rebuilt more than once"
        );
        assert!(!first.scene.glyph_id.is_empty(), "the cue must have glyphs");

        let mut counts = [0u64; 8];
        for c in counts.iter_mut() {
            let (_, n) = measure(|| {
                let shown = engine.scenes_for(Some(at));
                overlay.sync(&recorder, &shown);
            });
            *c = n;
        }
        eprintln!("cue path allocations per frame: {counts:?}");
        assert_eq!(
            counts, [0u64; 8],
            "a displayed cue allocates per frame, which is what the scene lane exists to avoid"
        );
        assert_eq!(
            overlay.builds(),
            1,
            "the display list was rebuilt per frame"
        );
    }

    /// The same, with the stack the fork's one-slot overlay has to merge. It
    /// costs a copy when a rank moves and nothing when nothing does.
    #[test]
    fn a_steady_stack_of_cues_costs_the_frame_nothing_either() {
        let (engine, both) = shown(&[cue("upper line", 0, 10), cue("lower line", 0, 10)]);
        assert_eq!(both.len(), 2);
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        let at = gst::ClockTime::from_seconds(AT);
        for _ in 0..4 {
            let shown = engine.scenes_for(Some(at));
            overlay.sync(&recorder, &shown);
        }
        let mut counts = [0u64; 8];
        for c in counts.iter_mut() {
            let (_, n) = measure(|| {
                let shown = engine.scenes_for(Some(at));
                overlay.sync(&recorder, &shown);
            });
            *c = n;
        }
        eprintln!("stacked cue path allocations per frame: {counts:?}");
        assert_eq!(counts, [0u64; 8], "a stack of cues allocates per frame");
        assert_eq!(overlay.builds(), 1);
    }

    /// The scene lane must not keep the vello_cpu paint alive behind it. Its
    /// pixel cache is the megabyte-per-cue lane the dodvg overlay replaces, and
    /// a paint nobody reads is worker milliseconds on every cue.
    #[test]
    fn the_scene_lane_never_paints_a_raster() {
        let (engine, _shown) = one("no pixels here");
        let at = gst::ClockTime::from_seconds(AT);
        // Give the worker every chance: it feeds itself the next wanted job
        // after every publish, so anything it still wants would land in here.
        let deadline = Instant::now() + Duration::from_millis(500);
        while Instant::now() < deadline {
            engine.scenes_for(Some(at));
            std::thread::sleep(Duration::from_millis(10));
        }
        assert_eq!(
            engine.cached_pixels(),
            0,
            "the worker painted rasters for a consumer that draws display lists"
        );
        assert!(
            engine.current_overlays().is_empty(),
            "the overlay lane answered on a scene consumer"
        );
        assert_eq!(
            engine.scene_builds(),
            1,
            "the cue was laid out more than once"
        );
    }
}

/// SUBTITLES ON A LANE THAT HAS NEVER HAD THEM, proved without a window.
///
/// Everything upstream of the renderer is the real thing: the flapjack player
/// owns the pipeline, decodebin3 builds the text branch, the subtitle track is
/// selected the way the app selects it, cues arrive on the branch's own
/// streaming thread through the same consumer `receiver-core`'s `Player::new`
/// installs, the engine lays them out with parley on its own worker, and the
/// display list is translated here into exactly the `Arc` that
/// `DodvgRendererAccess::set_cue_overlay` takes.
///
/// The last hop, the `Arc` becoming pixels, is the fork's own ground
/// (`cues_tests.rs`, wave 4, diffed against the vello_cpu backend at the plan's
/// section 5 thresholds). What is proved here is that it arrives, that it
/// arrives on the frames a delivered cue covers, and that it carries a real cue
/// rather than an empty scene.
///
/// The media is synthesized rather than a file on disk because the receiver's
/// GStreamer is the static PLAYBACK build: it ships no muxer at all
/// (`matroskamux`, `qtmux` and `avimux` are all absent), so a muxed fixture
/// cannot be produced here, and there is no video-plus-subtitle clip in the
/// tree to point `filesrc` at. The scenario driver is the same stand-in
/// `receiver-core`'s own subtitle-delivery suites use.
#[cfg(test)]
mod pipeline_proof {
    use std::{
        sync::{
            Arc as StdArc, Mutex,
            atomic::{AtomicBool, AtomicUsize, Ordering},
        },
        time::{Duration, Instant},
    };

    use fcast_video::cue::{CueEngine, CueInput, TextFormat};
    use flapjack::{
        AudioSink, MediaInput, Player, PlayerEvent, SelectionGate, Sinks, StartPoint,
        SubtitleFeedItem, SubtitleTrack, TrackSlot, VideoSink,
    };
    use simulator::{
        scenario::ScenarioBuilder,
        sink::FTestSink,
        spec::{CueSpec, Pacing, StreamSpec},
    };
    use gst::prelude::Cast;

    use super::{tests::Recorder, *};

    const CANVAS: (u32, u32) = (1280, 720);
    /// Cues start late enough that the track is selected and its branch built
    /// well before the first one, which is what the delivery assert below
    /// depends on.
    const FIRST_CUE_MS: u64 = 4_000;
    const CUE_MS: u64 = 500;
    const CUES: u64 = 12;

    fn sid_of(sid: &str) -> Option<flapjack::StreamId> {
        sid.strip_prefix("stream#")?
            .parse()
            .ok()
            .map(flapjack::StreamId::from_raw)
    }

    /// No audio ever leaves the process: both sinks are the test sink, which
    /// records and drops.
    #[test]
    fn a_subtitle_track_reaches_the_renderers_overlay_slot() {
        gst::init().unwrap();
        crate::gstreamer::init_and_load_plugins();
        static INIT: std::sync::Once = std::sync::Once::new();
        INIT.call_once(|| {
            simulator::register_for_tests();
            let _ = flapjack::audiostretch::plugin_init();
        });

        let cues: Vec<CueSpec> = (0..CUES)
            .map(|i| {
                CueSpec::new(
                    gst::ClockTime::from_mseconds(FIRST_CUE_MS + CUE_MS * 2 * i),
                    gst::ClockTime::from_mseconds(FIRST_CUE_MS + CUE_MS * (2 * i + 1)),
                    format!("subtitle line {i}"),
                )
            })
            .collect();
        let scenario = ScenarioBuilder::new("wgpu_lane_subtitles")
            .stream(StreamSpec::video("video_0").with_pacing(Pacing::Realtime))
            // A fixed hold per item, so the text branch stays alive across the
            // selection while every cue still leads its own running time, which
            // is the shape a real container gives. See the note on the same
            // pacing in receiver-core's contiguous-cue suite.
            .stream(
                StreamSpec::text("text_0", cues).with_pacing(Pacing::Jitter {
                    base_ms: 100,
                    jitter_ms: 0,
                }),
            )
            .duration(gst::ClockTime::from_seconds(30))
            .register();

        let video_sink = FTestSink::new();
        let frames = video_sink.recording();
        let player = StdArc::new(
            Player::new(Sinks {
                video: VideoSink::Element(video_sink.upcast()),
                audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
                subtitle: flapjack::SubtitleSink::None,
            })
            .expect("building flapjack"),
        );

        // THE ENGINE THE LANE OWNS, wired exactly as `make_sink` arms it and fed
        // exactly as `receiver-core`'s `Player::new` feeds it.
        let engine = CueEngine::for_scene_consumer();
        engine.set_canvas(CANVAS.0, CANVAS.1);
        engine.warm();

        let delivered: StdArc<Mutex<Vec<(gst::ClockTime, gst::ClockTime)>>> =
            StdArc::new(Mutex::new(Vec::new()));
        let clears = StdArc::new(AtomicUsize::new(0));
        {
            let engine = engine.clone();
            let log = StdArc::clone(&delivered);
            let clear_count = StdArc::clone(&clears);
            player.set_subtitle_consumer(move |item| match item {
                SubtitleFeedItem::Cue {
                    format,
                    text,
                    start_rt,
                    end_rt,
                    ..
                } => {
                    if let Some(end) = end_rt {
                        log.lock().unwrap().push((start_rt, end));
                    }
                    engine.submit(CueInput {
                        format: match format {
                            flapjack::SubtitleTextFormat::Utf8 => TextFormat::Utf8,
                            flapjack::SubtitleTextFormat::PangoMarkup => TextFormat::PangoMarkup,
                            flapjack::SubtitleTextFormat::CueIr { ir, pts_start } => {
                                TextFormat::CueIr { ir, pts_start }
                            }
                        },
                        text,
                        start_rt,
                        end_rt,
                    });
                }
                SubtitleFeedItem::Clear => {
                    log.lock().unwrap().clear();
                    clear_count.fetch_add(1, Ordering::Release);
                    engine.clear();
                }
                _ => {}
            });
        }

        let errors: StdArc<Mutex<Vec<String>>> = StdArc::new(Mutex::new(Vec::new()));
        let loaded = StdArc::new(AtomicBool::new(false));
        let text_sids: StdArc<Mutex<Vec<String>>> = StdArc::new(Mutex::new(Vec::new()));
        {
            let errors = StdArc::clone(&errors);
            let loaded = StdArc::clone(&loaded);
            let sids = StdArc::clone(&text_sids);
            player.set_event_handler(None, move |flapjack::Event { kind: event, .. }| match event {
                PlayerEvent::Error { message, .. } => errors.lock().unwrap().push(message),
                PlayerEvent::Loaded { .. } => loaded.store(true, Ordering::Release),
                PlayerEvent::StreamCollection(collection) => {
                    *sids.lock().unwrap() = collection
                        .iter()
                        .filter(|s| s.slot == TrackSlot::Subtitle)
                        .map(|s| s.id.to_string())
                        .collect();
                }
                _ => {}
            });
        }
        let gate = SelectionGate {
            quiet: true,
            paused: false,
            seekable: true,
        };
        let pump = || {
            player.poll_text_policy();
            player.pump_selection(gate);
            assert!(
                errors.lock().unwrap().is_empty(),
                "pipeline error: {:?}",
                errors.lock().unwrap()
            );
        };

        player.load(
            MediaInput::uri(scenario.uri()),
            StartPoint::at(gst::ClockTime::ZERO),
        );
        let deadline = Instant::now() + Duration::from_secs(30);
        while !loaded.load(Ordering::Acquire) {
            assert!(Instant::now() < deadline, "the load never finished");
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        player.play(player.allocate_op());

        // The subtitle track is off until something asks for it, exactly as in
        // the app.
        let deadline = Instant::now() + Duration::from_secs(30);
        while text_sids.lock().unwrap().is_empty() {
            assert!(
                Instant::now() < deadline,
                "the text stream was never advertised"
            );
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }
        let sid = text_sids.lock().unwrap()[0].clone();
        player.set_subtitle_track(sid_of(&sid).map_or(SubtitleTrack::Off, SubtitleTrack::Stream));

        // Nothing can be claimed until the branch has actually carried cues.
        let deadline = Instant::now() + Duration::from_secs(60);
        while delivered.lock().unwrap().len() < 4 {
            assert!(
                Instant::now() < deadline,
                "no cue ever reached the consumer, so the branch never carried the track"
            );
            pump();
            std::thread::sleep(Duration::from_millis(10));
        }

        // THE LANE'S OWN LOOP, per frame, exactly as `Sink::present` runs it:
        // the frame's running time in, the schedule advanced, the display list
        // put on the renderer's slot.
        let recorder = Recorder::default();
        let mut overlay = CueOverlay::default();
        let last = gst::ClockTime::from_mseconds(FIRST_CUE_MS + CUE_MS * 2 * CUES);
        let deadline = Instant::now() + Duration::from_secs(120);
        let mut seen = 0usize;
        // (frame running time, glyphs on the slot, the slot's origin)
        let mut walk: Vec<(gst::ClockTime, usize, [i32; 2])> = Vec::new();
        while Instant::now() < deadline {
            pump();
            let log = frames.snapshot();
            for entry in log[seen.min(log.len())..].iter() {
                let Some(pts) = entry.pts().filter(|_| entry.is_buffer()) else {
                    continue;
                };
                let shown = engine.scenes_for(Some(pts));
                let up = overlay.sync(&recorder, &shown);
                let (glyphs, origin) = match up {
                    true => {
                        let scene = recorder.last();
                        (scene.glyph_count(), scene.origin)
                    }
                    false => (0, [0, 0]),
                };
                walk.push((pts, glyphs, origin));
            }
            seen = log.len();
            if walk.last().is_some_and(|(pts, ..)| *pts >= last) {
                break;
            }
            std::thread::sleep(Duration::from_millis(5));
        }

        // Read before the shutdown: tearing down flushes the branch, and the
        // flush is a `Clear` that would wipe the record.
        let windows = delivered.lock().unwrap().clone();
        let (tx, rx) = std::sync::mpsc::channel();
        player.shutdown(Box::new(move || {
            let _ = tx.send(());
        }));
        let _ = rx.recv_timeout(Duration::from_secs(30));

        assert!(
            windows.len() >= 4,
            "only {} cues were delivered; too thin to claim anything",
            windows.len()
        );
        // A cue's first 100 ms are exempt: its layout can still be in flight on
        // the worker, which is a latency question and not this one.
        let grace = gst::ClockTime::from_mseconds(100);

        let mut covered = 0usize;
        let mut drawn = 0usize;
        let mut uncovered_with_a_cue = 0usize;
        for (pts, glyphs, origin) in &walk {
            match windows.iter().any(|(s, e)| pts >= s && pts < e) {
                true => {
                    let inside_grace = windows
                        .iter()
                        .any(|(s, e)| pts >= s && pts < e && *pts < *s + grace);
                    if inside_grace {
                        continue;
                    }
                    covered += 1;
                    if *glyphs > 0 {
                        drawn += 1;
                        // The house style puts an unpositioned cue in the lower
                        // part of the picture, and it must be on the canvas.
                        assert!(
                            origin[0] >= 0 && (origin[0] as u32) < CANVAS.0,
                            "the cue landed off the canvas horizontally at {pts}: {origin:?}"
                        );
                        assert!(
                            origin[1] >= 0 && (origin[1] as u32) < CANVAS.1,
                            "the cue landed off the canvas vertically at {pts}: {origin:?}"
                        );
                        assert!(
                            (origin[1] as u32) > CANVAS.1 / 2,
                            "an unpositioned cue is not in the lower half at {pts}: {origin:?}"
                        );
                    }
                }
                // Outside every delivered window nothing may be on the slot.
                false => {
                    if *glyphs > 0 {
                        uncovered_with_a_cue += 1;
                    }
                }
            }
        }

        eprintln!(
            "walked {} frames, {covered} covered by a delivered cue, {drawn} of them with a \
             display list on the renderer's slot",
            walk.len()
        );
        assert!(
            covered >= 8,
            "only {covered} frames of {} fell inside a delivered cue; the run is too thin",
            walk.len()
        );
        // THE CLAIM: a frame a cue covers gets that cue's display list.
        assert!(
            drawn * 10 >= covered * 9,
            "only {drawn} of {covered} covered frames carried a cue on the renderer's slot"
        );
        assert_eq!(
            uncovered_with_a_cue, 0,
            "a cue stayed on the slot past its own window"
        );
        // ...and the lane never asked the engine for a single pixel.
        assert_eq!(engine.cached_pixels(), 0);
    }
}
