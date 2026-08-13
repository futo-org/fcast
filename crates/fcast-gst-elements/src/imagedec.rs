//! fimagedec: decodes still images and animations (GIF, animated WebP, APNG)
//! into raw RGBA video frames.
//!
//! - GIF decodes streaming. Every other format's decoder needs Seek, so those
//!   buffer until upstream EOS.
//! - Animations loop by re-decoding the retained bytes (no seeks) with PTS
//!   accumulating monotonically. Upstream EOS is swallowed; the element never
//!   sends EOS downstream. A still pushes its one frame and parks.
//! - Posts a "fcast-image-stream" element message (format, dimensions,
//!   animated).

use gst::{glib, prelude::*};

pub mod imp {
    use std::{
        io::{Cursor, Read, Seek, SeekFrom},
        sync::{
            Arc, LazyLock,
            atomic::{AtomicI64, Ordering},
        },
    };

    use gst::{glib, prelude::*, subclass::prelude::*};
    use gst_video::VideoFormat;
    use image::{
        AnimationDecoder, ImageDecoder, ImageFormat, ImageReader,
        codecs::{gif::GifDecoder, png::PngDecoder, webp::WebPDecoder},
    };
    use parking_lot::{Condvar, Mutex};
    use tracing::{debug, warn};

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "fimagedec",
            gst::DebugColorFlags::empty(),
            Some("FCast image/animation decoder"),
        )
    });

    /// Browser convention: a GIF delay at or below 10ms renders at 100ms.
    const MIN_DELAY_MS: u64 = 10;
    const DEFAULT_DELAY_MS: u64 = 100;

    /// The bus message the application uses to classify an image load.
    pub const IMAGE_STREAM_MESSAGE: &str = "fcast-image-stream";

    struct InputState {
        /// Retained after EOS so animations can loop by re-decoding.
        data: Vec<u8>,
        eos: bool,
        /// Unblocks every wait and makes the decode loop bail out.
        aborted: bool,
    }

    struct Input {
        state: Mutex<InputState>,
        cond: Condvar,
    }

    impl Input {
        fn new() -> Self {
            Self {
                state: Mutex::new(InputState {
                    data: Vec::new(),
                    eos: false,
                    aborted: false,
                }),
                cond: Condvar::new(),
            }
        }

        fn push(&self, bytes: &[u8]) {
            let mut state = self.state.lock();
            state.data.extend_from_slice(bytes);
            self.cond.notify_all();
        }

        fn set_eos(&self) {
            self.state.lock().eos = true;
            self.cond.notify_all();
        }

        fn abort(&self) {
            self.state.lock().aborted = true;
            self.cond.notify_all();
        }

        fn aborted(&self) -> bool {
            self.state.lock().aborted
        }

        /// Block until upstream EOS and take the bytes. None when aborted.
        fn wait_full(&self) -> Option<Vec<u8>> {
            let mut state = self.state.lock();
            loop {
                if state.aborted {
                    return None;
                }
                if state.eos {
                    return Some(std::mem::take(&mut state.data));
                }
                self.cond.wait(&mut state);
            }
        }

        /// Block until the decode loop should die.
        fn park(&self) {
            let mut state = self.state.lock();
            while !state.aborted {
                self.cond.wait(&mut state);
            }
        }
    }

    /// Blocking reader over the shared input buffer (streaming GIF path).
    struct InputReader {
        input: Arc<Input>,
        pos: usize,
    }

    impl Read for InputReader {
        fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
            let mut state = self.input.state.lock();
            loop {
                if state.aborted {
                    return Err(std::io::Error::other("fimagedec flushing"));
                }
                if self.pos < state.data.len() {
                    let n = buf.len().min(state.data.len() - self.pos);
                    buf[..n].copy_from_slice(&state.data[self.pos..self.pos + n]);
                    self.pos += n;
                    return Ok(n);
                }
                if state.eos {
                    return Ok(0);
                }
                self.input.cond.wait(&mut state);
            }
        }
    }

    /// An End-relative seek needs the total length, so it blocks until EOS.
    impl Seek for InputReader {
        fn seek(&mut self, pos: SeekFrom) -> std::io::Result<u64> {
            let mut state = self.input.state.lock();
            let new_pos = match pos {
                SeekFrom::Start(p) => p as i128,
                SeekFrom::Current(off) => self.pos as i128 + off as i128,
                SeekFrom::End(off) => {
                    loop {
                        if state.aborted {
                            return Err(std::io::Error::other("fimagedec flushing"));
                        }
                        if state.eos {
                            break;
                        }
                        self.input.cond.wait(&mut state);
                    }
                    state.data.len() as i128 + off as i128
                }
            };
            if new_pos < 0 {
                return Err(std::io::Error::from(std::io::ErrorKind::InvalidInput));
            }
            self.pos = new_pos as usize;
            Ok(self.pos as u64)
        }
    }

    #[derive(Clone, Copy, Debug, PartialEq, Eq)]
    pub(super) enum FormatHint {
        Gif,
        Png,
        WebP,
        Jxl,
        Jp2,
        /// HEIC/HEIF via the libheif hooks (see `image::init_extra_decoders`).
        #[cfg(not(target_os = "android"))]
        Heif,
        /// Still-only formats the image crate reads natively.
        Other(ImageFormat),
    }

    impl FormatHint {
        /// Canonical typefound caps names. image/jpeg is deliberately absent
        /// because MJPEG video carries those caps and must stay with its video
        /// decoder, so still JPEGs arrive as the private image/x-fcast-jpeg
        /// suggested by our own typefinder (see `imagetypefind`).
        pub(super) fn from_caps_name(name: &str) -> Option<Self> {
            match name {
                "image/gif" => Some(Self::Gif),
                "image/png" | "image/apng" => Some(Self::Png),
                "image/webp" => Some(Self::WebP),
                // With VA hardware the baseline caps is claimed at a higher rank
                // by fvajpegdec. Both private JPEG caps still decode here.
                "image/x-fcast-jpeg" | "image/x-fcast-jpeg-sw" => {
                    Some(Self::Other(ImageFormat::Jpeg))
                }
                "image/jxl" => Some(Self::Jxl),
                "image/jp2" | "image/x-jpc" => Some(Self::Jp2),
                #[cfg(not(target_os = "android"))]
                "image/heic" | "image/heif" => Some(Self::Heif),
                "image/bmp" => Some(Self::Other(ImageFormat::Bmp)),
                "image/tiff" => Some(Self::Other(ImageFormat::Tiff)),
                "image/x-icon" => Some(Self::Other(ImageFormat::Ico)),
                "image/avif" => Some(Self::Other(ImageFormat::Avif)),
                "image/qoi" => Some(Self::Other(ImageFormat::Qoi)),
                "image/x-farbfeld" => Some(Self::Other(ImageFormat::Farbfeld)),
                "image/x-dds" => Some(Self::Other(ImageFormat::Dds)),
                "image/x-portable-bitmap"
                | "image/x-portable-graymap"
                | "image/x-portable-pixmap"
                | "image/x-portable-anymap" => Some(Self::Other(ImageFormat::Pnm)),
                _ => None,
            }
        }

        /// Sink pad template caps names, kept in sync with `from_caps_name`.
        pub(super) fn caps_names() -> Vec<&'static str> {
            let base = [
                "image/gif",
                "image/png",
                "image/apng",
                "image/webp",
                "image/x-fcast-jpeg",
                "image/x-fcast-jpeg-sw",
                "image/jxl",
                "image/jp2",
                "image/x-jpc",
                "image/bmp",
                "image/tiff",
                "image/x-icon",
                "image/avif",
                "image/qoi",
                "image/x-farbfeld",
                "image/x-dds",
                "image/x-portable-bitmap",
                "image/x-portable-graymap",
                "image/x-portable-pixmap",
                "image/x-portable-anymap",
            ];
            let heif: &[&str] = if cfg!(not(target_os = "android")) {
                &["image/heic", "image/heif"]
            } else {
                &[]
            };
            base.iter().chain(heif).copied().collect()
        }
    }

    struct DecodeTask {
        handle: std::thread::JoinHandle<()>,
    }

    #[derive(Default)]
    struct State {
        input: Option<Arc<Input>>,
        task: Option<DecodeTask>,
        format: Option<FormatHint>,
    }

    pub struct FImageDec {
        sinkpad: gst::Pad,
        srcpad: gst::Pad,
        state: Mutex<State>,
        /// Worst downstream lateness (ns) since the last schedule rebase.
        /// Animation frames cannot be skipped (each builds on the previous
        /// canvas), so lateness shifts the timeline instead of dropping frames.
        qos_lateness: Arc<AtomicI64>,
    }

    /// Decode thread state. Holds strong refs; the thread is joined on the
    /// downward state change so no cycle outlives the element.
    struct TaskCtx {
        element: super::FImageDec,
        srcpad: gst::Pad,
        input: Arc<Input>,
        format: FormatHint,
        /// Running output timestamp, monotonic across animation loops.
        pts: gst::ClockTime,
        out_info: Option<gst_video::VideoInfo>,
        /// See `FImageDec::qos_lateness`.
        qos_lateness: Arc<AtomicI64>,
    }

    enum PassOutcome {
        /// Pushed this many frames. Repeatable for looping.
        Done { frames: u64 },
        /// Downstream or flush ended it; the loop must exit.
        Stop,
    }

    impl TaskCtx {
        fn post_stream_info(&self, format: &str, width: u32, height: u32, animated: bool) {
            let s = gst::Structure::builder(IMAGE_STREAM_MESSAGE)
                .field("format", format)
                .field("width", width as i32)
                .field("height", height as i32)
                .field("animated", animated)
                .build();
            let msg = gst::message::Element::builder(s).src(&self.element).build();
            if self.element.post_message(msg).is_err() {
                warn!("fimagedec: element not in a bin, image info message dropped");
            }
        }

        /// Configure src caps and push the TIME segment on the first frame.
        fn ensure_output(&mut self, width: u32, height: u32) -> Result<(), gst::FlowError> {
            if self
                .out_info
                .as_ref()
                .is_some_and(|i| i.width() == width && i.height() == height)
            {
                return Ok(());
            }
            let info = gst_video::VideoInfo::builder(VideoFormat::Rgba, width, height)
                // Variable framerate, frame durations carry the timing.
                .fps(gst::Fraction::new(0, 1))
                .build()
                .map_err(|_| gst::FlowError::NotNegotiated)?;
            let caps = info.to_caps().map_err(|_| gst::FlowError::NotNegotiated)?;

            // Sticky-event order: stream-start must precede caps.
            if self
                .srcpad
                .sticky_event::<gst::event::StreamStart>(0)
                .is_none()
            {
                let id = format!("{}-imagedec", self.element.name());
                self.srcpad.push_event(gst::event::StreamStart::new(&id));
            }
            if !self.srcpad.push_event(gst::event::Caps::new(&caps)) {
                return Err(gst::FlowError::NotNegotiated);
            }
            if self.out_info.is_none() {
                let segment = gst::FormattedSegment::<gst::ClockTime>::new();
                if !self.srcpad.push_event(gst::event::Segment::new(&segment)) {
                    return Err(gst::FlowError::Error);
                }
            }
            self.out_info = Some(info);
            Ok(())
        }

        fn push_frame(
            &mut self,
            frame: image::RgbaImage,
            delay_ms: u64,
        ) -> Result<(), gst::FlowError> {
            let (width, height) = frame.dimensions();
            self.ensure_output(width, height)?;

            let duration = gst::ClockTime::from_mseconds(delay_ms);

            // Rebase on reported lateness plus one frame of margin, so the
            // schedule converges just above the achievable production rate.
            let late = self.qos_lateness.swap(0, Ordering::Relaxed);
            if late > 0 {
                let shift = gst::ClockTime::from_nseconds(late as u64) + duration;
                gst::debug!(
                    CAT,
                    obj = self.element,
                    "Rebasing the schedule by {shift} after downstream lateness"
                );
                self.pts += shift;
            }
            let mut buffer = gst::Buffer::from_mut_slice(frame.into_raw());
            {
                let buffer = buffer.get_mut().unwrap();
                buffer.set_pts(self.pts);
                buffer.set_duration(duration);
            }
            self.pts += duration;
            self.srcpad.push(buffer).map(|_| ())
        }

        /// Push every frame of one animation pass.
        fn push_pass<'a>(
            &mut self,
            decoder: impl AnimationDecoder<'a>,
        ) -> Result<PassOutcome, gst::FlowError> {
            let mut frames = 0u64;
            let mut iter = decoder.into_frames();
            loop {
                if self.input.aborted() {
                    return Ok(PassOutcome::Stop);
                }
                // Decode cost for this frame only (excludes the sync wait in the
                // previous push). Paces the schedule to the achievable rate.
                let decode_started = std::time::Instant::now();
                let Some(frame) = iter.next() else {
                    break;
                };
                let frame = match frame {
                    Ok(f) => f,
                    Err(err) => {
                        // A truncated tail is common. Keep the frames we got.
                        warn!(?err, "fimagedec: animation frame decode failed");
                        break;
                    }
                };
                let decode_ms = decode_started.elapsed().as_millis() as u64;
                let (num, denom) = frame.delay().numer_denom_ms();
                let mut delay_ms = if denom == 0 {
                    DEFAULT_DELAY_MS
                } else {
                    (num as u64) / (denom as u64)
                };
                if delay_ms <= MIN_DELAY_MS {
                    delay_ms = DEFAULT_DELAY_MS;
                }
                match self.push_frame(frame.into_buffer(), delay_ms.max(decode_ms)) {
                    Ok(()) => frames += 1,
                    Err(gst::FlowError::Flushing | gst::FlowError::Eos) => {
                        return Ok(PassOutcome::Stop);
                    }
                    Err(err) => return Err(err),
                }
            }
            Ok(PassOutcome::Done { frames })
        }

        /// Streaming GIF. Frames go out as their bytes arrive, loops re-decode.
        fn run_gif(&mut self) -> Result<(), DecodeError> {
            let mut first_pass = true;
            loop {
                let reader = std::io::BufReader::new(InputReader {
                    input: self.input.clone(),
                    pos: 0,
                });
                if self.input.aborted() {
                    return Ok(());
                }
                let decoder = GifDecoder::new(reader)?;
                if first_pass {
                    let (w, h) = decoder.dimensions();
                    self.post_stream_info("gif", w, h, true);
                }
                match self.push_pass(decoder)? {
                    PassOutcome::Stop => return Ok(()),
                    PassOutcome::Done { frames } => {
                        if first_pass && frames == 0 {
                            return Err(DecodeError::NoFrames);
                        }
                        // A single-frame GIF is a still. Show it and park.
                        if frames <= 1 {
                            self.input.park();
                            return Ok(());
                        }
                        first_pass = false;
                    }
                }
            }
        }

        /// Decode a still (EXIF orientation applied), push it, park until
        /// teardown.
        fn push_still(
            &mut self,
            mut decoder: impl ImageDecoder,
            format: &'static str,
        ) -> Result<(), DecodeError> {
            let orientation = decoder
                .orientation()
                .unwrap_or(image::metadata::Orientation::NoTransforms);
            let (w, h) = decoder.dimensions();
            self.post_stream_info(format, w, h, false);
            let mut img = image::DynamicImage::from_decoder(decoder)?;
            img.apply_orientation(orientation);
            match self.push_frame(img.into_rgba8(), DEFAULT_DELAY_MS) {
                Ok(()) | Err(gst::FlowError::Flushing | gst::FlowError::Eos) => {}
                Err(err) => return Err(err.into()),
            }
            self.input.park();
            Ok(())
        }

        /// Loop animation passes. `make_decoder` builds a fresh decoder per
        /// pass because the frame iterators consume them.
        fn run_animation_passes<'b, D: AnimationDecoder<'b>>(
            &mut self,
            format: &'static str,
            dimensions: (u32, u32),
            mut make_decoder: impl FnMut(&mut Self) -> Result<D, DecodeError>,
        ) -> Result<(), DecodeError> {
            let mut first_pass = true;
            loop {
                if self.input.aborted() {
                    return Ok(());
                }
                if first_pass {
                    self.post_stream_info(format, dimensions.0, dimensions.1, true);
                }
                let decoder = make_decoder(self)?;
                match self.push_pass(decoder)? {
                    PassOutcome::Stop => return Ok(()),
                    PassOutcome::Done { frames } => {
                        if first_pass && frames == 0 {
                            return Err(DecodeError::NoFrames);
                        }
                        if frames <= 1 {
                            self.input.park();
                            return Ok(());
                        }
                        first_pass = false;
                    }
                }
            }
        }

        /// Buffered path for every non-GIF format. Their decoders need Seek.
        fn run_buffered(&mut self) -> Result<(), DecodeError> {
            let Some(bytes) = self.input.wait_full() else {
                return Ok(());
            };
            match self.format {
                FormatHint::Gif => unreachable!("gif takes the streaming path"),
                FormatHint::Png => {
                    let probe = PngDecoder::new(Cursor::new(&bytes))?;
                    if probe.is_apng().unwrap_or(false) {
                        let dims = probe.dimensions();
                        self.run_animation_passes("apng", dims, |_| {
                            Ok(PngDecoder::new(Cursor::new(&bytes))?.apng()?)
                        })
                    } else {
                        drop(probe);
                        let decoder =
                            ImageReader::with_format(Cursor::new(&bytes), ImageFormat::Png)
                                .into_decoder()?;
                        self.push_still(decoder, "png")
                    }
                }
                FormatHint::WebP => {
                    let probe = WebPDecoder::new(Cursor::new(&bytes))?;
                    if probe.has_animation() {
                        let dims = probe.dimensions();
                        self.run_animation_passes("webp", dims, |_| {
                            Ok(WebPDecoder::new(Cursor::new(&bytes))?)
                        })
                    } else {
                        self.push_still(probe, "webp")
                    }
                }
                FormatHint::Jxl => {
                    // Still-only. Animated JXL is not supported by the decoder.
                    let decoder = jxl_oxide::integration::JxlDecoder::new(Cursor::new(&bytes))
                        .map_err(|err| DecodeError::Other(format!("JPEG XL: {err}")))?;
                    self.push_still(decoder, "jxl")
                }
                FormatHint::Jp2 => {
                    let decoder = hayro_jpeg2000::Image::new(
                        &bytes,
                        &hayro_jpeg2000::DecodeSettings {
                            resolve_palette_indices: true,
                            strict: false,
                            target_resolution: None,
                        },
                    )
                    .map_err(|err| DecodeError::Other(format!("JPEG 2000: {err:?}")))?;
                    self.push_still(decoder, "jp2")
                }
                #[cfg(not(target_os = "android"))]
                FormatHint::Heif => {
                    // The libheif decoding hooks key off the guessed format.
                    let decoder = ImageReader::new(Cursor::new(&bytes))
                        .with_guessed_format()
                        .map_err(image::ImageError::IoError)?
                        .into_decoder()?;
                    self.push_still(decoder, "heif")
                }
                FormatHint::Other(format) => {
                    let decoder =
                        ImageReader::with_format(Cursor::new(&bytes), format).into_decoder()?;
                    self.push_still(decoder, still_name(format))
                }
            }
        }

        fn run(&mut self) {
            let result = match self.format {
                FormatHint::Gif => self.run_gif(),
                _ => self.run_buffered(),
            };
            match result {
                Ok(()) => debug!("fimagedec: decode task done"),
                Err(err) => {
                    if self.input.aborted() {
                        // Flush/teardown raced the decode, not a real error.
                        debug!(?err, "fimagedec: decode aborted");
                        return;
                    }
                    gst::element_error!(
                        self.element,
                        gst::StreamError::Decode,
                        ["image decode failed: {err}"]
                    );
                }
            }
        }
    }

    /// Announcement name for the still-only formats.
    fn still_name(format: ImageFormat) -> &'static str {
        match format {
            ImageFormat::Jpeg => "jpeg",
            ImageFormat::Bmp => "bmp",
            ImageFormat::Tiff => "tiff",
            ImageFormat::Ico => "ico",
            ImageFormat::Avif => "avif",
            ImageFormat::Qoi => "qoi",
            ImageFormat::Farbfeld => "farbfeld",
            ImageFormat::Dds => "dds",
            ImageFormat::Pnm => "pnm",
            _ => "image",
        }
    }

    #[derive(Debug)]
    enum DecodeError {
        Image(image::ImageError),
        Flow(gst::FlowError),
        NoFrames,
        Other(String),
    }

    impl std::fmt::Display for DecodeError {
        fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            match self {
                Self::Image(err) => write!(f, "{err}"),
                Self::Flow(err) => write!(f, "downstream flow error: {err}"),
                Self::NoFrames => write!(f, "no decodable frames"),
                Self::Other(msg) => write!(f, "{msg}"),
            }
        }
    }

    impl From<image::ImageError> for DecodeError {
        fn from(err: image::ImageError) -> Self {
            Self::Image(err)
        }
    }

    impl From<gst::FlowError> for DecodeError {
        fn from(err: gst::FlowError) -> Self {
            Self::Flow(err)
        }
    }

    impl FImageDec {
        fn sink_chain(
            &self,
            _pad: &gst::Pad,
            buffer: gst::Buffer,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
            let mut state = self.state.lock();
            let Some(format) = state.format else {
                gst::error!(CAT, imp = self, "buffer before caps");
                return Err(gst::FlowError::NotNegotiated);
            };
            let input = state
                .input
                .get_or_insert_with(|| Arc::new(Input::new()))
                .clone();
            if input.aborted() {
                return Err(gst::FlowError::Flushing);
            }
            input.push(&map);
            if state.task.is_none() {
                let mut ctx = TaskCtx {
                    element: self.obj().clone(),
                    srcpad: self.srcpad.clone(),
                    input,
                    format,
                    pts: gst::ClockTime::ZERO,
                    out_info: None,
                    qos_lateness: self.qos_lateness.clone(),
                };
                let handle = std::thread::Builder::new()
                    .name("fimagedec".into())
                    .spawn(move || ctx.run())
                    .map_err(|_| gst::FlowError::Error)?;
                state.task = Some(DecodeTask { handle });
            }
            Ok(gst::FlowSuccess::Ok)
        }

        fn sink_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
            use gst::EventView;
            match event.view() {
                EventView::Caps(caps) => {
                    let format = caps
                        .caps()
                        .structure(0)
                        .and_then(|s| FormatHint::from_caps_name(s.name()));
                    let Some(format) = format else {
                        gst::error!(CAT, imp = self, "unsupported caps {:?}", caps.caps());
                        return false;
                    };
                    self.state.lock().format = Some(format);
                    // The src pad sends its own video caps once dimensions
                    // are known.
                    true
                }
                EventView::Segment(_) => {
                    // Upstream runs in BYTES; the decode task emits its own
                    // TIME segment.
                    true
                }
                EventView::Eos(_) => {
                    // Swallowed. Animations loop forever and stills hold their
                    // frame.
                    let state = self.state.lock();
                    if let Some(input) = state.input.as_ref() {
                        input.set_eos();
                    } else {
                        // EOS with no data at all means nothing will ever render.
                        drop(state);
                        gst::element_error!(
                            self.obj(),
                            gst::StreamError::Decode,
                            ["image stream ended without data"]
                        );
                    }
                    true
                }
                EventView::FlushStart(_) => {
                    // Forward downstream BEFORE joining the decode task. It may
                    // be blocked in srcpad.push() against a waiting sink, and
                    // only the flush reaching that sink unblocks it (the abort
                    // flag covers the input waits, not an in-flight push).
                    let ret = gst::Pad::event_default(pad, Some(&*self.obj()), event);
                    self.shutdown_task();
                    ret
                }
                EventView::FlushStop(_) => {
                    self.reset();
                    gst::Pad::event_default(pad, Some(&*self.obj()), event)
                }
                _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
            }
        }

        fn src_event(&self, pad: &gst::Pad, event: gst::Event) -> bool {
            use gst::EventView;
            match event.view() {
                // Not seekable. Loops are internal re-decodes.
                EventView::Seek(_) => false,
                // Drives the schedule rebase (see `qos_lateness`). Consumed
                // here, upstream has nothing to throttle.
                EventView::Qos(qos) => {
                    let (_, _, jitter, _) = qos.get();
                    if jitter > 0 {
                        self.qos_lateness.fetch_max(jitter, Ordering::Relaxed);
                    }
                    true
                }
                _ => gst::Pad::event_default(pad, Some(&*self.obj()), event),
            }
        }

        fn src_query(&self, pad: &gst::Pad, query: &mut gst::QueryRef) -> bool {
            use gst::QueryViewMut;
            match query.view_mut() {
                QueryViewMut::Seeking(q) => {
                    q.set(false, gst::ClockTime::ZERO, gst::ClockTime::NONE);
                    true
                }
                QueryViewMut::Duration(_) => {
                    // Unknowable while looping. Not forwarded because upstream
                    // would answer a TIME query with a BYTES duration.
                    false
                }
                _ => gst::Pad::query_default(pad, Some(&*self.obj()), query),
            }
        }

        /// Stop the decode task, unblocking every wait, then join.
        fn shutdown_task(&self) {
            let task = {
                let mut state = self.state.lock();
                if let Some(input) = state.input.as_ref() {
                    input.abort();
                }
                state.task.take()
            };
            if let Some(task) = task {
                if task.handle.join().is_err() {
                    warn!("fimagedec: decode task panicked");
                }
            }
        }

        fn reset(&self) {
            let mut state = self.state.lock();
            state.input = None;
            state.task = None;
            self.qos_lateness.store(0, Ordering::Relaxed);
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FImageDec {
        const NAME: &'static str = "FImageDec";
        type Type = super::FImageDec;
        type ParentType = gst::Element;

        fn with_class(klass: &Self::Class) -> Self {
            let templ = klass.pad_template("sink").unwrap();
            let sinkpad = gst::Pad::builder_from_template(&templ)
                .chain_function(|pad, parent, buffer| {
                    FImageDec::catch_panic_pad_function(
                        parent,
                        || Err(gst::FlowError::Error),
                        |imp| imp.sink_chain(pad, buffer),
                    )
                })
                .event_function(|pad, parent, event| {
                    FImageDec::catch_panic_pad_function(
                        parent,
                        || false,
                        |imp| imp.sink_event(pad, event),
                    )
                })
                .build();

            let templ = klass.pad_template("src").unwrap();
            let srcpad = gst::Pad::builder_from_template(&templ)
                .event_function(|pad, parent, event| {
                    FImageDec::catch_panic_pad_function(
                        parent,
                        || false,
                        |imp| imp.src_event(pad, event),
                    )
                })
                .query_function(|pad, parent, query| {
                    FImageDec::catch_panic_pad_function(
                        parent,
                        || false,
                        |imp| imp.src_query(pad, query),
                    )
                })
                .build();

            Self {
                sinkpad,
                srcpad,
                state: Mutex::new(State::default()),
                qos_lateness: Arc::new(AtomicI64::new(0)),
            }
        }
    }

    impl ObjectImpl for FImageDec {
        fn constructed(&self) {
            self.parent_constructed();
            let obj = self.obj();
            obj.add_pad(&self.sinkpad).unwrap();
            obj.add_pad(&self.srcpad).unwrap();
        }
    }

    impl GstObjectImpl for FImageDec {}

    impl ElementImpl for FImageDec {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "FCast image decoder",
                    "Codec/Decoder/Video",
                    "Decodes still images and animations to raw video, looping animations",
                    "FCast contributors",
                )
            });
            Some(&*METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let sink_caps = {
                    let mut caps = gst::Caps::new_empty();
                    {
                        let caps = caps.get_mut().unwrap();
                        for name in FormatHint::caps_names() {
                            caps.append(gst::Caps::new_empty_simple(name));
                        }
                    }
                    caps
                };
                let src_caps = gst_video::VideoCapsBuilder::new()
                    .format(VideoFormat::Rgba)
                    .build();
                vec![
                    gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &sink_caps,
                    )
                    .unwrap(),
                    gst::PadTemplate::new(
                        "src",
                        gst::PadDirection::Src,
                        gst::PadPresence::Always,
                        &src_caps,
                    )
                    .unwrap(),
                ]
            });
            PAD_TEMPLATES.as_ref()
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            if transition == gst::StateChange::PausedToReady {
                // Unblock and join before the parent deactivates the pads.
                self.shutdown_task();
            }
            let res = self.parent_change_state(transition)?;
            if transition == gst::StateChange::ReadyToPaused {
                self.reset();
            }
            Ok(res)
        }
    }
}

glib::wrapper! {
    pub struct FImageDec(ObjectSubclass<imp::FImageDec>)
        @extends gst::Element, gst::Object;
}

/// The mime types the application routes through the player pipeline (typefind
/// re-classifies from the actual bytes, so aliases are fine here).
pub fn player_mime_types() -> &'static [&'static str] {
    &[
        "image/jpeg",
        "image/gif",
        "image/png",
        "image/apng",
        "image/webp",
        "image/jxl",
        "image/jp2",
        "image/jpx",
        "image/jpm",
        "image/bmp",
        "image/x-ms-bmp",
        "image/tiff",
        "image/x-icon",
        "image/vnd.microsoft.icon",
        "image/avif",
        "image/qoi",
        "image/x-farbfeld",
        "image/x-dds",
        "image/x-portable-bitmap",
        "image/x-portable-graymap",
        "image/x-portable-pixmap",
        "image/x-portable-anymap",
        #[cfg(not(target_os = "android"))]
        "image/heic",
        #[cfg(not(target_os = "android"))]
        "image/heif",
    ]
}

pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "fimagedec",
        gst::Rank::PRIMARY,
        FImageDec::static_type(),
    )
}

#[cfg(test)]
mod tests {
    use std::sync::Once;

    use gst::prelude::*;

    fn init() {
        static INIT: Once = Once::new();
        INIT.call_once(|| {
            gst::init().unwrap();
            super::plugin_init().unwrap();
        });
    }

    /// A tiny in-memory animated GIF: 16x16 frames, 100ms delay, looping.
    fn make_gif(frames: u32) -> Vec<u8> {
        make_gif_with_delay(frames, 100)
    }

    /// Like `make_gif` but with a caller-chosen per-frame delay in ms.
    fn make_gif_with_delay(frames: u32, delay_ms: u32) -> Vec<u8> {
        use image::codecs::gif::{GifEncoder, Repeat};
        let mut out = Vec::new();
        {
            let mut enc = GifEncoder::new_with_speed(&mut out, 10);
            enc.set_repeat(Repeat::Infinite).unwrap();
            for i in 0..frames {
                let shade = (i * 40 % 256) as u8;
                let img = image::RgbaImage::from_pixel(
                    16,
                    16,
                    image::Rgba([shade, 255 - shade, 128, 255]),
                );
                let frame = image::Frame::from_parts(
                    img,
                    0,
                    0,
                    image::Delay::from_numer_denom_ms(delay_ms, 1),
                );
                enc.encode_frame(frame).unwrap();
            }
        }
        out
    }

    /// A tiny still PNG (8x8 solid) encoded in-memory.
    fn make_still_png() -> Vec<u8> {
        let img = image::RgbaImage::from_pixel(8, 8, image::Rgba([12, 200, 60, 255]));
        let mut out = std::io::Cursor::new(Vec::new());
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut out, image::ImageFormat::Png)
            .unwrap();
        out.into_inner()
    }

    /// Two-frame 8x8 animated fixtures with 100ms delays, generated once with
    /// ffmpeg because the image crate cannot encode them.
    const ANIM_WEBP: &[u8] = include_bytes!("../test-data/anim.webp");
    const ANIM_APNG: &[u8] = include_bytes!("../test-data/anim.apng");

    /// Build an `appsrc (caps) -> fimagedec -> appsink(sync=false)` pipeline.
    fn direct_pipeline(
        caps_name: &str,
    ) -> (
        gst::Pipeline,
        gst_app::AppSrc,
        gst::Element,
        gst_app::AppSink,
    ) {
        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder()
            .caps(&gst::Caps::new_empty_simple(caps_name))
            .format(gst::Format::Bytes)
            .build();
        let dec = gst::ElementFactory::make("fimagedec").build().unwrap();
        let appsink = gst_app::AppSink::builder().sync(false).build();
        pipeline
            .add_many([
                appsrc.upcast_ref::<gst::Element>(),
                &dec,
                appsink.upcast_ref(),
            ])
            .unwrap();
        gst::Element::link_many([appsrc.upcast_ref(), &dec, appsink.upcast_ref()]).unwrap();
        (pipeline, appsrc, dec, appsink)
    }

    /// Pull one sample, failing the test with `what` on timeout.
    fn pull(appsink: &gst_app::AppSink, what: &str) -> gst::Sample {
        appsink
            .try_pull_sample(gst::ClockTime::from_seconds(10))
            .unwrap_or_else(|| panic!("timed out waiting for {what}"))
    }

    /// Dimensions from a sample's caps.
    fn sample_dims(sample: &gst::Sample) -> (i32, i32) {
        let s = sample.caps().unwrap().structure(0).unwrap();
        (
            s.get::<i32>("width").unwrap(),
            s.get::<i32>("height").unwrap(),
        )
    }

    /// decodebin3 must autoplug fimagedec for a typefound GIF, and the
    /// animation must loop without EOS.
    #[test]
    fn decodebin3_autoplugs_and_loops_gif() {
        init();

        let gif = make_gif(4);
        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder()
            .caps(&gst::Caps::new_empty_simple("image/gif"))
            .format(gst::Format::Bytes)
            .build();
        let db3 = gst::ElementFactory::make("decodebin3").build().unwrap();
        let appsink = gst_app::AppSink::builder()
            .caps(
                &gst::Caps::builder("video/x-raw")
                    .field("format", "RGBA")
                    .build(),
            )
            .sync(false)
            .build();

        pipeline
            .add_many([
                appsrc.upcast_ref::<gst::Element>(),
                &db3,
                appsink.upcast_ref(),
            ])
            .unwrap();
        appsrc.link(&db3).unwrap();
        let appsink_clone = appsink.clone();
        db3.connect_pad_added(move |_, pad| {
            if pad.direction() != gst::PadDirection::Src {
                return;
            }
            let sink = appsink_clone.static_pad("sink").unwrap();
            pad.link(&sink).unwrap();
        });

        pipeline.set_state(gst::State::Playing).unwrap();
        appsrc.push_buffer(gst::Buffer::from_slice(gif)).unwrap();
        appsrc.end_of_stream().unwrap();

        // 4 frames in the file. 10 samples prove at least two loop passes.
        let mut dims = None;
        for _ in 0..10 {
            let sample = appsink
                .try_pull_sample(gst::ClockTime::from_seconds(10))
                .expect("sample from looping gif");
            let caps = sample.caps().unwrap();
            let s = caps.structure(0).unwrap();
            dims = Some((
                s.get::<i32>("width").unwrap(),
                s.get::<i32>("height").unwrap(),
            ));
        }
        assert_eq!(dims, Some((16, 16)));

        // Teardown must not hang on the parked/looping decode task.
        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// A single-frame GIF is a still. One frame, then park without EOS.
    #[test]
    fn single_frame_gif_parks_without_eos() {
        init();

        let gif = make_gif(1);
        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder()
            .caps(&gst::Caps::new_empty_simple("image/gif"))
            .format(gst::Format::Bytes)
            .build();
        let dec = gst::ElementFactory::make("fimagedec").build().unwrap();
        let appsink = gst_app::AppSink::builder().sync(false).build();

        pipeline
            .add_many([
                appsrc.upcast_ref::<gst::Element>(),
                &dec,
                appsink.upcast_ref(),
            ])
            .unwrap();
        gst::Element::link_many([appsrc.upcast_ref(), &dec, appsink.upcast_ref()]).unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();
        appsrc.push_buffer(gst::Buffer::from_slice(gif)).unwrap();
        appsrc.end_of_stream().unwrap();

        let sample = appsink
            .try_pull_sample(gst::ClockTime::from_seconds(10))
            .expect("the still frame");
        assert!(sample.buffer().is_some());

        assert!(
            appsink
                .try_pull_sample(gst::ClockTime::from_mseconds(500))
                .is_none()
        );
        assert!(!appsink.is_eos());

        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// An animated WebP fed directly to fimagedec must loop.
    #[test]
    fn animated_webp_loops() {
        init();
        let (pipeline, appsrc, _dec, appsink) = direct_pipeline("image/webp");

        pipeline.set_state(gst::State::Playing).unwrap();
        appsrc
            .push_buffer(gst::Buffer::from_slice(ANIM_WEBP))
            .unwrap();
        appsrc.end_of_stream().unwrap();

        // 2 source frames. 5 samples proves more than two full passes.
        let mut dims = None;
        for _ in 0..5 {
            let sample = pull(&appsink, "looping webp sample");
            dims = Some(sample_dims(&sample));
        }
        assert_eq!(dims, Some((8, 8)));
        assert!(!appsink.is_eos(), "animation must never send EOS");

        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// An APNG loops whether typed image/apng or image/png.
    #[test]
    fn apng_loops() {
        init();
        for caps_name in ["image/apng", "image/png"] {
            let (pipeline, appsrc, _dec, appsink) = direct_pipeline(caps_name);

            pipeline.set_state(gst::State::Playing).unwrap();
            appsrc
                .push_buffer(gst::Buffer::from_slice(ANIM_APNG))
                .unwrap();
            appsrc.end_of_stream().unwrap();

            let mut dims = None;
            for _ in 0..5 {
                let sample = pull(&appsink, "looping apng sample");
                dims = Some(sample_dims(&sample));
            }
            assert_eq!(dims, Some((8, 8)), "caps {caps_name}");
            assert!(!appsink.is_eos(), "caps {caps_name}: no EOS");

            pipeline.set_state(gst::State::Null).unwrap();
        }
    }

    /// A plain still PNG. One frame, then park (no second frame, no EOS).
    #[test]
    fn still_png_parks() {
        init();
        let (pipeline, appsrc, _dec, appsink) = direct_pipeline("image/png");

        pipeline.set_state(gst::State::Playing).unwrap();
        appsrc
            .push_buffer(gst::Buffer::from_slice(make_still_png()))
            .unwrap();
        appsrc.end_of_stream().unwrap();

        let sample = pull(&appsink, "still png frame");
        assert_eq!(sample_dims(&sample), (8, 8));

        assert!(
            appsink
                .try_pull_sample(gst::ClockTime::from_mseconds(500))
                .is_none(),
            "a still image must not produce a second frame"
        );
        assert!(!appsink.is_eos());

        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// PTS must strictly increase across a loop boundary, and a 0ms authored
    /// delay must clamp to 100ms.
    #[test]
    fn pts_monotonic_and_delay_clamped() {
        init();
        let gif = make_gif_with_delay(3, 0);
        let (pipeline, appsrc, _dec, appsink) = direct_pipeline("image/gif");

        pipeline.set_state(gst::State::Playing).unwrap();
        appsrc.push_buffer(gst::Buffer::from_slice(gif)).unwrap();
        appsrc.end_of_stream().unwrap();

        // 7 samples spans more than two full 3-frame passes.
        let hundred_ms = gst::ClockTime::from_mseconds(100);
        let mut last_pts: Option<gst::ClockTime> = None;
        for i in 0..7 {
            let sample = pull(&appsink, "clamped-delay gif sample");
            let buffer = sample.buffer().unwrap();
            let pts = buffer.pts().expect("buffer must carry a PTS");
            let dur = buffer.duration().expect("buffer must carry a duration");
            assert_eq!(
                dur, hundred_ms,
                "0ms authored delay must clamp to 100ms (sample {i})"
            );
            if let Some(prev) = last_pts {
                assert!(
                    pts > prev,
                    "PTS must strictly increase, sample {i}: {pts} !> {prev}"
                );
            }
            last_pts = Some(pts);
        }

        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// Tearing down mid-loop must not hang.
    #[test]
    fn teardown_mid_loop_does_not_hang() {
        init();
        let gif = make_gif(4);
        let (pipeline, appsrc, _dec, appsink) = direct_pipeline("image/gif");

        pipeline.set_state(gst::State::Playing).unwrap();
        appsrc.push_buffer(gst::Buffer::from_slice(gif)).unwrap();
        appsrc.end_of_stream().unwrap();

        pull(&appsink, "sample before teardown");
        pull(&appsink, "second sample before teardown");

        let start = std::time::Instant::now();
        pipeline.set_state(gst::State::Null).unwrap();
        let elapsed = start.elapsed();
        assert!(
            elapsed < std::time::Duration::from_secs(5),
            "teardown of a looping animation took {elapsed:?}, expected bounded"
        );
    }

    /// A flush pair on the sink pad mid-animation must not wedge the element.
    #[test]
    fn flush_stops_decode() {
        init();
        let gif = make_gif(4);
        let (pipeline, appsrc, dec, appsink) = direct_pipeline("image/gif");

        pipeline.set_state(gst::State::Playing).unwrap();
        appsrc.push_buffer(gst::Buffer::from_slice(gif)).unwrap();
        appsrc.end_of_stream().unwrap();

        pull(&appsink, "sample before flush");
        pull(&appsink, "second sample before flush");

        let sinkpad = dec.static_pad("sink").unwrap();
        assert!(
            sinkpad.send_event(gst::event::FlushStart::new()),
            "flush-start must be accepted"
        );
        assert!(
            sinkpad.send_event(gst::event::FlushStop::builder(true).build()),
            "flush-stop must be accepted"
        );

        let start = std::time::Instant::now();
        pipeline.set_state(gst::State::Null).unwrap();
        assert!(
            start.elapsed() < std::time::Duration::from_secs(5),
            "teardown after flush must be bounded"
        );
    }

    /// EOS with no buffers must post a StreamError rather than hang.
    #[test]
    fn eos_without_data_errors() {
        init();
        let (pipeline, appsrc, _dec, _appsink) = direct_pipeline("image/gif");
        let bus = pipeline.bus().unwrap();

        pipeline.set_state(gst::State::Playing).unwrap();
        // No push_buffer: straight to EOS.
        appsrc.end_of_stream().unwrap();

        let msg = bus
            .timed_pop_filtered(gst::ClockTime::from_seconds(10), &[gst::MessageType::Error])
            .expect("an error message must be posted for EOS without data");
        match msg.view() {
            gst::MessageView::Error(err) => {
                assert!(
                    err.error().matches(gst::StreamError::Decode),
                    "expected StreamError::Decode, got {:?}: {}",
                    err.error(),
                    err.error()
                );
            }
            other => panic!("expected Error, got {other:?}"),
        }

        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// The "fcast-image-stream" message must reach the bus with the right
    /// format, dimensions, and animated flag.
    #[test]
    fn image_stream_message_posted() {
        init();

        fn stream_info(caps_name: &str, data: Vec<u8>) -> (String, i32, i32, bool) {
            let (pipeline, appsrc, _dec, appsink) = direct_pipeline(caps_name);
            let bus = pipeline.bus().unwrap();

            pipeline.set_state(gst::State::Playing).unwrap();
            appsrc.push_buffer(gst::Buffer::from_slice(data)).unwrap();
            appsrc.end_of_stream().unwrap();

            // Pull one sample so decode makes progress before we wait for the
            // message. That also guarantees the first-pass post ran.
            let _ = appsink.try_pull_sample(gst::ClockTime::from_seconds(10));

            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
            let mut result = None;
            while std::time::Instant::now() < deadline {
                let Some(msg) = bus.timed_pop_filtered(
                    gst::ClockTime::from_seconds(1),
                    &[gst::MessageType::Element],
                ) else {
                    continue;
                };
                let gst::MessageView::Element(elem) = msg.view() else {
                    continue;
                };
                let Some(s) = elem.structure() else { continue };
                if s.name() != super::imp::IMAGE_STREAM_MESSAGE {
                    continue;
                }
                result = Some((
                    s.get::<String>("format").unwrap(),
                    s.get::<i32>("width").unwrap(),
                    s.get::<i32>("height").unwrap(),
                    s.get::<bool>("animated").unwrap(),
                ));
                break;
            }
            pipeline.set_state(gst::State::Null).unwrap();
            result.expect("fcast-image-stream message must be posted")
        }

        let (fmt, w, h, animated) = stream_info("image/gif", make_gif(4));
        assert_eq!((fmt.as_str(), w, h, animated), ("gif", 16, 16, true));

        let (fmt, w, h, animated) = stream_info("image/png", make_still_png());
        assert_eq!((fmt.as_str(), w, h, animated), ("png", 8, 8, false));
    }

    /// GIF decodes streaming, so the first frame must appear before EOS.
    #[test]
    fn gif_streaming_starts_before_eos() {
        init();
        let gif = make_gif(4);
        let mid = gif.len() / 2;
        let (first_half, second_half) = gif.split_at(mid);
        let first_half = first_half.to_vec();
        let second_half = second_half.to_vec();

        let (pipeline, appsrc, _dec, appsink) = direct_pipeline("image/gif");

        pipeline.set_state(gst::State::Playing).unwrap();
        appsrc
            .push_buffer(gst::Buffer::from_slice(first_half))
            .unwrap();

        let sample = pull(&appsink, "streaming first frame before EOS");
        assert_eq!(sample_dims(&sample), (16, 16));
        assert!(!appsink.is_eos());

        appsrc
            .push_buffer(gst::Buffer::from_slice(second_half))
            .unwrap();
        appsrc.end_of_stream().unwrap();

        // Well past the 4-frame file length, to prove the loop.
        for _ in 0..6 {
            pull(&appsink, "post-EOS looping sample");
        }
        assert!(!appsink.is_eos());

        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// An unmapped sink caps would let decodebin3 plug us for caps the decode
    /// task then rejects.
    #[test]
    fn caps_names_all_map_to_hints() {
        for name in super::imp::FormatHint::caps_names() {
            assert!(
                super::imp::FormatHint::from_caps_name(name).is_some(),
                "sink caps {name} has no format hint"
            );
        }
    }

    /// Every typefinder-produced media type must be decodable here, or a
    /// typefound file dead-ends in decodebin3. HEIC/HEIF are the sanctioned
    /// exception when the libheif hooks are compiled out.
    #[test]
    fn typefinder_caps_all_decodable() {
        let heif_gated = cfg!(target_os = "android");
        for name in crate::imagetypefind::produced_caps() {
            if heif_gated && matches!(*name, "image/heic" | "image/heif") {
                continue;
            }
            assert!(
                super::imp::FormatHint::from_caps_name(name).is_some(),
                "typefinder emits {name} but fimagedec cannot decode it"
            );
        }
    }

    /// A hand-built QOI still exercises the generic ImageReader still path.
    #[test]
    fn qoi_still_parks() {
        init();

        // QOI: 14-byte header (magic, w, h, channels, colorspace), pixel ops,
        // 8-byte end marker.
        let mut qoi: Vec<u8> = Vec::new();
        qoi.extend_from_slice(b"qoif");
        qoi.extend_from_slice(&2u32.to_be_bytes());
        qoi.extend_from_slice(&2u32.to_be_bytes());
        qoi.push(4); // channels
        qoi.push(0); // colorspace
        for _ in 0..4 {
            qoi.push(0xFF); // QOI_OP_RGBA
            qoi.extend_from_slice(&[10, 20, 30, 255]);
        }
        qoi.extend_from_slice(&[0, 0, 0, 0, 0, 0, 0, 1]); // end marker

        let (pipeline, appsrc, _dec, appsink) = direct_pipeline("image/qoi");
        pipeline.set_state(gst::State::Playing).unwrap();
        appsrc.push_buffer(gst::Buffer::from_slice(qoi)).unwrap();
        appsrc.end_of_stream().unwrap();

        let sample = pull(&appsink, "the qoi still frame");
        assert_eq!(sample_dims(&sample), (2, 2));
        assert!(
            appsink
                .try_pull_sample(gst::ClockTime::from_mseconds(500))
                .is_none()
        );
        assert!(!appsink.is_eos());

        pipeline.set_state(gst::State::Null).unwrap();
    }

    /// A FlushStart while the decode task is blocked pushing into a prerolled
    /// sync=true sink must not deadlock. The handler has to forward the flush
    /// downstream before joining the task, because only the flush reaching the
    /// sink can unblock a thread parked in `push`.
    #[test]
    fn flush_while_preroll_blocked_does_not_deadlock() {
        init();

        let pipeline = gst::Pipeline::new();
        let appsrc = gst_app::AppSrc::builder()
            .caps(&gst::Caps::new_empty_simple("image/gif"))
            .format(gst::Format::Bytes)
            .build();
        let dec = gst::ElementFactory::make("fimagedec").build().unwrap();
        // With sync=true, in PAUSED the sink prerolls frame 1 and blocks frame 2
        // until it goes PLAYING, which never happens here.
        let fakesink = gst::ElementFactory::make("fakesink")
            .property("sync", true)
            .build()
            .unwrap();
        pipeline
            .add_many([appsrc.upcast_ref::<gst::Element>(), &dec, &fakesink])
            .unwrap();
        gst::Element::link_many([appsrc.upcast_ref(), &dec, &fakesink]).unwrap();

        pipeline.set_state(gst::State::Paused).unwrap();
        appsrc
            .push_buffer(gst::Buffer::from_slice(make_gif(4)))
            .unwrap();
        appsrc.end_of_stream().unwrap();

        // Wait for preroll to complete.
        let bus = pipeline.bus().unwrap();
        let async_done = bus.timed_pop_filtered(
            gst::ClockTime::from_seconds(10),
            &[gst::MessageType::AsyncDone, gst::MessageType::Error],
        );
        match async_done.as_ref().map(|m| m.view()) {
            Some(gst::MessageView::AsyncDone(_)) => {}
            Some(gst::MessageView::Error(err)) => {
                pipeline.set_state(gst::State::Null).unwrap();
                panic!("pipeline errored before preroll: {}", err.error());
            }
            _ => {
                pipeline.set_state(gst::State::Null).unwrap();
                panic!("preroll (AsyncDone) never arrived within 10s");
            }
        }

        // Let the decode task block pushing the second frame.
        std::thread::sleep(std::time::Duration::from_millis(300));

        // Helper thread so the test thread can time out a send_event that
        // never returns.
        let sinkpad = dec.static_pad("sink").unwrap();
        let (tx, rx) = std::sync::mpsc::channel::<bool>();
        let flush_pad = sinkpad.clone();
        std::thread::spawn(move || {
            let accepted = flush_pad.send_event(gst::event::FlushStart::new());
            let _ = tx.send(accepted);
        });

        let returned = rx.recv_timeout(std::time::Duration::from_secs(5));

        // Detached cleanup. A still-wedged element blocks FlushStop and the
        // state change, and must not swallow the verdict below.
        let cleanup_pad = sinkpad.clone();
        let cleanup_pipeline = pipeline.clone();
        let cleanup = std::thread::spawn(move || {
            let _ = cleanup_pad.send_event(gst::event::FlushStop::builder(true).build());
            let _ = cleanup_pipeline.set_state(gst::State::Null);
        });

        // A deadlock parks non-daemon GStreamer threads forever, so the process
        // never exits and the harness cannot report this thread's panic. The
        // watchdog force-exits instead; it is defused once cleanup completes so
        // a passing run cannot kill other tests in this process.
        let defused = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        std::thread::spawn({
            let defused = defused.clone();
            move || {
                std::thread::sleep(std::time::Duration::from_secs(20));
                if defused.load(std::sync::atomic::Ordering::SeqCst) {
                    return;
                }
                eprintln!("fimagedec flush deadlock watchdog: process wedged, forcing exit");
                std::process::exit(101);
            }
        });

        let deadlocked = returned.is_err();
        assert!(
            !deadlocked,
            "FlushStart on the sink pad did not return within 5s: the handler \
             deadlocked joining the decode task, which is blocked in \
             srcpad.push() against a prerolled sync=true sink that only the \
             not-yet-forwarded FlushStart could unblock"
        );

        // Passing path only. Wait out cleanup, then stand down the watchdog.
        let _ = cleanup.join();
        defused.store(true, std::sync::atomic::Ordering::SeqCst);
    }
}
