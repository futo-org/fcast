/// Port of [textoverlay] to forward text as metadata instead of rendering to a bitmap.
///
/// [textoverlay]: https://gstreamer.freedesktop.org/documentation/pango/textoverlay.html
use std::mem;

use gst::glib::{self, types::StaticType};

// Unused
// pub const CAPS_FEATURE_FCAST_TEXT_OVERLAY: &str = "meta:FCastTextOverlay";

pub(crate) mod meta_imp {
    use gst::glib::{self, translate::*};
    use std::{ptr, sync::LazyLock};

    #[derive(Debug, Copy, Clone)]
    pub enum TextFormat {
        Utf8,
        PangoMarkup,
    }

    pub(super) struct FCastVideoTextOverlayMetaParams {
        pub format: TextFormat,
        pub text: String,
    }

    #[repr(C)]
    #[derive(Debug)]
    pub struct FCastVideoTextOverlayMeta {
        parent: gst::ffi::GstMeta,
        pub format: TextFormat,
        pub text: String,
    }

    pub fn get_type() -> glib::Type {
        static TYPE: LazyLock<glib::Type> = LazyLock::new(|| unsafe {
            let t = from_glib(gst::ffi::gst_meta_api_type_register(
                c"FCastVideoTextOverlayMetaAPI".as_ptr() as *const _,
                [ptr::null::<std::os::raw::c_char>()].as_ptr() as *mut *const _,
            ));

            assert_ne!(t, glib::Type::INVALID);

            t
        });

        *TYPE
    }

    unsafe extern "C" fn meta_init(
        meta: *mut gst::ffi::GstMeta,
        params: glib::ffi::gpointer,
        _buffer: *mut gst::ffi::GstBuffer,
    ) -> glib::ffi::gboolean {
        unsafe {
            assert!(!params.is_null());

            let meta = &mut *(meta as *mut FCastVideoTextOverlayMeta);
            let params = ptr::read(params as *const FCastVideoTextOverlayMetaParams);

            ptr::write(&mut meta.format, params.format);
            ptr::write(&mut meta.text, params.text);

            true.into_glib()
        }
    }

    unsafe extern "C" fn meta_free(
        meta: *mut gst::ffi::GstMeta,
        _buffer: *mut gst::ffi::GstBuffer,
    ) {
        unsafe {
            let meta = &mut *(meta as *mut FCastVideoTextOverlayMeta);

            ptr::drop_in_place(&mut meta.format);
            ptr::drop_in_place(&mut meta.text);
        }
    }

    unsafe extern "C" fn meta_transform(
        dest: *mut gst::ffi::GstBuffer,
        meta: *mut gst::ffi::GstMeta,
        _buffer: *mut gst::ffi::GstBuffer,
        _type_: glib::ffi::GQuark,
        _data: glib::ffi::gpointer,
    ) -> glib::ffi::gboolean {
        unsafe {
            let meta = &*(meta as *mut FCastVideoTextOverlayMeta);

            super::FCastVideoTextOverlayMeta::add(
                gst::BufferRef::from_mut_ptr(dest),
                meta.format,
                meta.text.clone(),
            );

            true.into_glib()
        }
    }

    pub fn meta_get_info() -> *const gst::ffi::GstMetaInfo {
        struct MetaInfo(ptr::NonNull<gst::ffi::GstMetaInfo>);
        unsafe impl Send for MetaInfo {}
        unsafe impl Sync for MetaInfo {}

        static META_INFO: LazyLock<MetaInfo> = LazyLock::new(|| unsafe {
            MetaInfo(
                ptr::NonNull::new(gst::ffi::gst_meta_register(
                    get_type().into_glib(),
                    c"FCastVideoTextOverlayMeta".as_ptr() as *const _,
                    std::mem::size_of::<FCastVideoTextOverlayMeta>(),
                    Some(meta_init),
                    Some(meta_free),
                    Some(meta_transform),
                ) as *mut gst::ffi::GstMetaInfo)
                .expect("Failed to register meta API"),
            )
        });

        META_INFO.0.as_ptr()
    }
}

#[repr(transparent)]
pub struct FCastVideoTextOverlayMeta(meta_imp::FCastVideoTextOverlayMeta);

unsafe impl Send for FCastVideoTextOverlayMeta {}
unsafe impl Sync for FCastVideoTextOverlayMeta {}

impl FCastVideoTextOverlayMeta {
    pub fn add(buffer: &mut gst::BufferRef, format: meta_imp::TextFormat, text: String) {
        unsafe {
            let mut params =
                mem::ManuallyDrop::new(meta_imp::FCastVideoTextOverlayMetaParams { format, text });

            gst::ffi::gst_buffer_add_meta(
                buffer.as_mut_ptr(),
                meta_imp::meta_get_info(),
                &mut *params as *mut meta_imp::FCastVideoTextOverlayMetaParams
                    as glib::ffi::gpointer,
            );
        }
    }

    pub fn get(&self) -> (meta_imp::TextFormat, &str) {
        (self.0.format, &self.0.text)
    }
}

unsafe impl gst::meta::MetaAPI for FCastVideoTextOverlayMeta {
    type GstType = meta_imp::FCastVideoTextOverlayMeta;

    fn meta_api() -> glib::Type {
        meta_imp::get_type()
    }
}

impl std::fmt::Debug for FCastVideoTextOverlayMeta {
    fn fmt(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
        f.debug_struct("FCastVideoTextOverlayMeta")
            .field("format", &self.0.format)
            .field("text", &self.0.text)
            .finish()
    }
}

mod imp {
    use parking_lot::{Condvar, Mutex};
    use std::sync::LazyLock;

    use gst::{EventView, glib, prelude::*, subclass::prelude::*};

    use crate::fcasttextoverlay::FCastVideoTextOverlayMeta;

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new("fcasttextoverlay", gst::DebugColorFlags::empty(), None)
    });

    #[derive(Default)]
    struct State {
        video_flushing: bool,
        video_eos: bool,
        text_flushing: bool,
        text_eos: bool,
        segment: gst::Segment,
        text_segment: gst::Segment,
        text_buffer: Option<gst::Buffer>,
        text_buffer_running_time: Option<gst::ClockTime>,
        text_buffer_running_time_end: Option<gst::ClockTime>,
        text_linked: bool,
        have_pango_markup: bool,
        info: Option<gst_video::VideoInfo>,
    }

    enum WaitForTextResult {
        // TODO: can it be a ref?
        Have(Option<gst::Buffer>),
        Waiting,
        Flushing,
        Eos,
    }

    fn generic_to_time(generic: gst::GenericFormattedValue) -> Option<gst::ClockTime> {
        match generic {
            gst::GenericFormattedValue::Time(t) => t,
            _ => None,
        }
    }

    pub struct FCastTextOverlay {
        state: Mutex<State>,
        state_cvar: parking_lot::Condvar,
        video_sink_pad: gst::Pad,
        text_sink_pad: gst::Pad,
        src_pad: gst::Pad,
    }

    impl FCastTextOverlay {
        fn video_sink_event(
            &self,
            pad: &gst::Pad,
            parent: Option<&gst::Object>,
            event: gst::Event,
        ) -> bool {
            gst::debug!(CAT, imp = self, "video sink event {event:?}");

            let execute_default = true;

            let mut state = self.state.lock();
            match event.view() {
                EventView::StreamStart(_) => {
                    state.video_flushing = false;
                    state.video_eos = false;
                    state.segment.reset_with_format(gst::Format::Time);
                }
                EventView::Caps(caps_event) => {
                    self.src_pad.check_reconfigure();

                    let caps = caps_event.caps();

                    gst::debug!(CAT, imp = self, "performing negotiation caps={caps:?}");

                    let overlay_caps = caps.to_owned();

                    self.src_pad
                        .push_event(gst::event::Caps::new(&overlay_caps));

                    gst::debug!(CAT, imp = self, "Using caps {:?}", overlay_caps);

                    match gst_video::VideoInfo::from_caps(&overlay_caps) {
                        Ok(info) => state.info = Some(info),
                        Err(err) => {
                            gst::warning!(CAT, imp = self, "caps have no video info {err:?}")
                        }
                    }

                    return true;
                }
                EventView::Segment(seg) => {
                    let seg = seg.segment();
                    if seg.format() == gst::Format::Time {
                        state.segment = seg.clone();
                    } else {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "received non-time newsegment event on video input"
                        );
                    }
                }
                EventView::Eos(_) => {
                    state.video_eos = true;
                }
                EventView::FlushStart(_) => {
                    state.video_flushing = true;
                    self.state_cvar.notify_all();
                }
                EventView::FlushStop(_) => {
                    state.video_flushing = false;
                    state.video_eos = false;
                    state.segment.reset_with_format(gst::Format::Time);
                }
                _ => (),
            }

            if execute_default {
                drop(state);
                gst::Pad::event_default(pad, parent, event)
            } else {
                true
            }
        }

        fn wait_for_text_buf(
            &self,
            buffer: &gst::Buffer,
            start: gst::ClockTime,
            end: Option<gst::ClockTime>,
        ) -> WaitForTextResult {
            let mut state = self.state.lock();

            if state.video_flushing {
                return WaitForTextResult::Flushing;
            }

            if state.video_eos {
                return WaitForTextResult::Eos;
            }

            if !state.text_linked {
                return WaitForTextResult::Have(None);
            }

            match state.text_buffer.as_ref() {
                Some(text_buf) => {
                    let (Some(text_running_time), Some(text_running_time_end)) = (
                        state.text_buffer_running_time,
                        state.text_buffer_running_time_end,
                    ) else {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "Got text buffer with invalid timestamp or duration"
                        );
                        state.text_buffer = None;
                        self.state_cvar.notify_all();
                        return WaitForTextResult::Have(None);
                    };

                    let vid_running_time = generic_to_time(state.segment.to_running_time(start));
                    let vid_running_time_end = generic_to_time(state.segment.to_running_time(end));

                    if vid_running_time
                        .map(|vid_end| text_running_time_end < vid_end)
                        .unwrap_or(false)
                    {
                        gst::debug!(CAT, imp = self, "text buffer too old, popping");
                        state.text_buffer = None;
                        drop(state);
                        self.state_cvar.notify_all();
                        WaitForTextResult::Waiting
                    } else if vid_running_time_end <= Some(text_running_time) {
                        gst::debug!(CAT, imp = self, "text in future",);
                        WaitForTextResult::Have(None)
                    } else {
                        WaitForTextResult::Have(Some(text_buf.clone()))
                    }
                }
                None => {
                    let mut wait_for_text_buf = !state.text_eos;

                    if state.text_segment.format() == gst::Format::Time {
                        let vid_running_time =
                            generic_to_time(state.segment.to_running_time(buffer.pts()));
                        let text_start_running_time = generic_to_time(
                            state
                                .text_segment
                                .to_running_time(state.text_segment.start()),
                        );
                        let text_position_running_time = generic_to_time(
                            state
                                .text_segment
                                .to_running_time(state.text_segment.position()),
                        );

                        if let Some(vid_running_time) = vid_running_time {
                            if let Some(text_start_running_time) = text_start_running_time
                                && vid_running_time < text_start_running_time
                            {
                                wait_for_text_buf = false;
                            }

                            if let Some(text_position_running_time) = text_position_running_time
                                // Deadlocks when text position is 0?
                                && (text_position_running_time == gst::ClockTime::from_seconds(0)
                                    || vid_running_time < text_position_running_time)
                            {
                                wait_for_text_buf = false;
                            }
                        }
                    }

                    if wait_for_text_buf {
                        gst::debug!(CAT, imp = self, "Waiting for text buffer");
                        self.state_cvar.wait(&mut state);
                        WaitForTextResult::Waiting
                    } else {
                        WaitForTextResult::Have(None)
                    }
                }
            }
        }

        fn video_sink_chain(
            &self,
            mut buffer: gst::Buffer,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let start = buffer.pts();
            let mut end = match (start, buffer.duration()) {
                (Some(start), Some(duration)) => Some(start + duration),
                _ => gst::ClockTime::NONE,
            };

            let state = self.state.lock();

            fn out_of_segment(
                overlay: &FCastTextOverlay,
            ) -> Result<gst::FlowSuccess, gst::FlowError> {
                gst::debug!(CAT, imp = overlay, "buffer out of segment, discarding");
                Ok(gst::FlowSuccess::Ok)
            }

            if end.is_none()
                && generic_to_time(state.segment.start())
                    .map(|seg_start| start < Some(seg_start))
                    .unwrap_or(false)
            {
                return out_of_segment(self);
            }

            if state.segment.format() == gst::Format::Undefined {
                gst::debug!(CAT, imp = self, "Segment has undefined format");
                return Ok(gst::FlowSuccess::Ok);
            }

            let Some(start) = start else {
                return Err(gst::FlowError::Error);
            };

            if end.is_none() && Some(start) < generic_to_time(state.segment.start()) {
                return out_of_segment(self);
            }

            let Some((clip_start, clip_end)) = state.segment.clip(start, end) else {
                return out_of_segment(self);
            };

            let (Some(clip_start), Some(clip_end)) =
                (generic_to_time(clip_start), generic_to_time(clip_end))
            else {
                gst::error!(
                    CAT,
                    imp = self,
                    "clip_start or clip_end are not in clock format"
                );
                return Err(gst::FlowError::Error);
            };

            if clip_start != start || end.map(|end| clip_end != end).unwrap_or(false) {
                gst::debug!(
                    CAT,
                    imp = self,
                    "clipping buffer timestamp/duration to segment"
                );
                let buffer_mut = buffer.get_mut().unwrap();
                buffer_mut.set_pts(clip_start);
                if end.is_none() {
                    buffer_mut.set_duration(clip_end - clip_start);
                }
            }

            if end.is_none() {
                if let Some(info) = state.info.as_ref() {
                    end = Some(
                        start
                            + gst::ClockTime::from_seconds_f64(
                                info.fps().numer() as f64 / info.fps().denom() as f64,
                            ),
                    );
                } else {
                    end = Some(start + gst::ClockTime::from_seconds(1));
                }
            }

            let _ = self.obj().sync_values(start);

            drop(state);

            loop {
                match self.wait_for_text_buf(&buffer, start, end) {
                    WaitForTextResult::Have(text_buf) => {
                        if let Some(text_buf) = text_buf {
                            let Some(buffer_mut) = buffer.get_mut() else {
                                gst::debug!(CAT, imp = self, "received invalid video frame buffer");
                                break;
                            };

                            let Ok(text_read) = text_buf.map_readable() else {
                                gst::warning!(CAT, imp = self, "text buffer is invalid");
                                break;
                            };
                            let format = if self.state.lock().have_pango_markup {
                                super::meta_imp::TextFormat::PangoMarkup
                            } else {
                                super::meta_imp::TextFormat::Utf8
                            };

                            match str::from_utf8(text_read.as_slice()) {
                                Ok(text) => FCastVideoTextOverlayMeta::add(
                                    buffer_mut,
                                    format,
                                    text.to_owned(),
                                ),
                                Err(err) => {
                                    gst::warning!(CAT, imp = self, "Invalid text input: {err}")
                                }
                            }
                        }
                        break;
                    }
                    WaitForTextResult::Waiting => {
                        gst::debug!(CAT, imp = self, "waiting for text buffer...");
                    }
                    WaitForTextResult::Flushing => {
                        gst::debug!(CAT, imp = self, "flushing, discarding buffer");
                        return Err(gst::FlowError::Flushing);
                    }
                    WaitForTextResult::Eos => {
                        gst::debug!(CAT, imp = self, "eos, discarding buffer");
                        return Err(gst::FlowError::Eos);
                    }
                }
            }

            self.state.lock().segment.set_position(clip_start);

            self.state_cvar.notify_all();

            self.src_pad.push(buffer)
        }

        fn video_sink_query(
            &self,
            pad: &gst::Pad,
            parent: Option<&gst::Object>,
            query: &mut gst::QueryRef,
        ) -> bool {
            if let gst::QueryViewMut::Caps(q) = query.view_mut() {
                let peer_caps = pad.peer_query_caps(None);
                let result_caps = if peer_caps.is_any() {
                    pad.pad_template_caps()
                } else {
                    peer_caps
                };

                gst::debug!(CAT, obj = pad, "returning {result_caps:?}");
                q.set_result(&Some(result_caps));

                true
            } else {
                gst::Pad::query_default(pad, parent, query)
            }
        }

        fn text_sink_event(
            &self,
            pad: &gst::Pad,
            parent: Option<&gst::Object>,
            event: gst::Event,
        ) -> bool {
            gst::debug!(CAT, imp = self, "text sink event: {event:?}");

            let mut execute_default = true;
            let mut state = self.state.lock();
            match event.view() {
                EventView::StreamStart(_) => {
                    state.text_flushing = false;
                    state.text_eos = false;
                    state.text_segment.reset_with_format(gst::Format::Time);
                    state.text_buffer = None;
                }
                EventView::Caps(caps_event) => {
                    let caps = caps_event.caps();
                    if let Some(structure) = caps.structure(0)
                        && let Ok(format) = structure.get::<&str>("format")
                    {
                        state.have_pango_markup = format == "pango-markup";
                    }
                }
                EventView::Segment(seg) => {
                    let seg = seg.segment();
                    if seg.format() == gst::Format::Time {
                        gst::info!(CAT, imp = self, "new text segment {seg:?}");
                        state.text_segment = seg.clone();
                    } else {
                        gst::warning!(
                            CAT,
                            imp = self,
                            "received non-time newsegment event on text input"
                        );
                    }

                    self.state_cvar.notify_all();

                    execute_default = false;
                }
                EventView::Gap(gap) => {
                    let (mut start, duration) = gap.get();
                    if let Some(duration) = duration {
                        start += duration;
                    }
                    state.text_segment.set_position(start);

                    self.state_cvar.notify_all();

                    execute_default = false;
                }
                EventView::Eos(_) => {
                    state.text_eos = true;
                    execute_default = false;
                    self.state_cvar.notify_all();
                }
                EventView::FlushStart(_) => {
                    state.text_flushing = true;
                    execute_default = false;
                    self.state_cvar.notify_all();
                }
                EventView::FlushStop(_) => {
                    state.text_flushing = false;
                    state.text_eos = false;
                    state.text_segment.reset_with_format(gst::Format::Time);
                    execute_default = false;
                    state.text_buffer = None;
                }
                _ => (),
            }

            if execute_default {
                drop(state);
                gst::Pad::event_default(pad, parent, event)
            } else {
                true
            }
        }

        fn text_sink_chain(
            &self,
            mut buffer: gst::Buffer,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            gst::log!(CAT, imp = self, "text sink chain {buffer:?}");

            let mut state = self.state.lock();

            if state.text_flushing {
                gst::debug!(CAT, imp = self, "text flushing");
                return Err(gst::FlowError::Flushing);
            }

            if state.text_eos {
                gst::debug!(CAT, imp = self, "text EOS");
                return Err(gst::FlowError::Eos);
            }

            let Some(text_start) = buffer.pts() else {
                gst::warning!(CAT, imp = self, "buffer is missing timestamp");
                return Ok(gst::FlowSuccess::Ok);
            };

            let Some(stop) = buffer
                .duration()
                .map(|duration| text_start.saturating_add(duration))
            else {
                gst::warning!(CAT, imp = self, "buffer is missing duration");
                return Ok(gst::FlowSuccess::Ok);
            };

            if let Some((clip_start, clip_end)) = state.text_segment.clip(text_start, stop) {
                let buffer_mut = buffer.make_mut();
                match clip_start {
                    gst::GenericFormattedValue::Time(clip_start) => {
                        buffer_mut.set_pts(clip_start);
                        match clip_end {
                            gst::GenericFormattedValue::Time(clip_end) => {
                                if let Some(clip_start) = clip_start
                                    && let Some(clip_end) = clip_end
                                {
                                    buffer_mut.set_duration(clip_end - clip_start);
                                }
                            }
                            _ => {
                                gst::error!(
                                    CAT,
                                    imp = self,
                                    "clip_end is not in valid time format"
                                );
                            }
                        }
                    }
                    _ => {
                        gst::error!(CAT, imp = self, "clip_start is not in valid time format");
                    }
                }

                while state.text_buffer.is_some() {
                    gst::log!(
                        CAT,
                        imp = self,
                        "Pad has buffer queued, waiting {:?}",
                        state.text_buffer
                    );
                    self.state_cvar.wait(&mut state);
                    gst::log!(CAT, imp = self, "Pad resuming");
                    if state.text_flushing {
                        gst::log!(CAT, imp = self, "text flushing");
                        return Err(gst::FlowError::Flushing);
                    }
                }

                state.text_buffer_running_time_end = gst::ClockTime::NONE;
                state.text_segment.set_position(clip_start);
                state.text_buffer_running_time =
                    generic_to_time(state.text_segment.to_running_time(text_start));

                if let Some(text_duration) = buffer.duration() {
                    let text_end = text_start + text_duration;
                    state.text_buffer_running_time_end =
                        generic_to_time(state.text_segment.to_running_time(text_end));
                }

                state.text_buffer = Some(buffer);

                self.state_cvar.notify_all();
            }

            Ok(gst::FlowSuccess::Ok)
        }

        fn text_sink_linked(&self) -> Result<gst::PadLinkSuccess, gst::PadLinkError> {
            gst::debug!(CAT, imp = self, "Text pad linked");

            self.state.lock().text_linked = true;

            Ok(gst::PadLinkSuccess)
        }

        fn text_sink_unlinked(&self) {
            gst::debug!(CAT, imp = self, "Text pad unlinked");

            let mut state = self.state.lock();
            state.text_linked = false;
            state.text_segment = gst::Segment::new();
        }

        fn src_event(
            &self,
            _pad: &gst::Pad,
            _parent: Option<&gst::Object>,
            event: gst::Event,
        ) -> bool {
            let res = self.video_sink_pad.push_event(event.clone());
            self.text_sink_pad.push_event(event);
            res
        }

        fn src_query(
            &self,
            _pad: &gst::Pad,
            _parent: Option<&gst::Object>,
            query: &mut gst::QueryRef,
        ) -> bool {
            gst::debug!(CAT, imp = self, "src query {query:?}");
            self.video_sink_pad.peer_query(query)
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FCastTextOverlay {
        const NAME: &str = "FCastTextOverlay";
        type Type = super::FCastTextOverlay;
        type ParentType = gst::Element;

        fn with_class(klass: &Self::Class) -> Self {
            let video_sink_templ = klass.pad_template("video_sink").unwrap();
            let video_sink_pad = gst::Pad::builder_from_template(&video_sink_templ)
                .flags(gst::PadFlags::PROXY_ALLOCATION)
                .event_function(|pad, parent, event| {
                    FCastTextOverlay::catch_panic_pad_function(
                        parent,
                        || false,
                        |elem| elem.video_sink_event(pad, parent, event),
                    )
                })
                .chain_function(|_pad, parent, buffer| {
                    FCastTextOverlay::catch_panic_pad_function(
                        parent,
                        || Err(gst::FlowError::Error),
                        |elem| elem.video_sink_chain(buffer),
                    )
                })
                .query_function(|pad, parent, query| {
                    FCastTextOverlay::catch_panic_pad_function(
                        parent,
                        || false,
                        |elem| elem.video_sink_query(pad, parent, query),
                    )
                })
                .build();

            let text_sink_templ = klass.pad_template("text_sink").unwrap();
            let text_sink_pad = gst::Pad::builder_from_template(&text_sink_templ)
                .event_function(|pad, parent, event| {
                    FCastTextOverlay::catch_panic_pad_function(
                        parent,
                        || false,
                        |elem| elem.text_sink_event(pad, parent, event),
                    )
                })
                .chain_function(|_pad, parent, buffer| {
                    FCastTextOverlay::catch_panic_pad_function(
                        parent,
                        || Err(gst::FlowError::Error),
                        |elem| elem.text_sink_chain(buffer),
                    )
                })
                .link_function(|_, parent, _| {
                    FCastTextOverlay::catch_panic_pad_function(
                        parent,
                        || Err(gst::PadLinkError::Refused),
                        |elem| elem.text_sink_linked(),
                    )
                })
                .unlink_function(|_, parent| {
                    FCastTextOverlay::catch_panic_pad_function(
                        parent,
                        || (),
                        |elem| elem.text_sink_unlinked(),
                    );
                })
                .build();

            let src_templ = klass.pad_template("src").unwrap();
            let src_pad = gst::Pad::builder_from_template(&src_templ)
                .event_function(|pad, parent, event| {
                    FCastTextOverlay::catch_panic_pad_function(
                        parent,
                        || false,
                        |elem| elem.src_event(pad, parent, event),
                    )
                })
                .query_function(|pad, parent, query| {
                    FCastTextOverlay::catch_panic_pad_function(
                        parent,
                        || false,
                        |elem| elem.src_query(pad, parent, query),
                    )
                })
                .build();

            Self {
                state: Mutex::new(State::default()),
                video_sink_pad,
                text_sink_pad,
                src_pad,
                state_cvar: Condvar::new(),
            }
        }
    }

    impl ObjectImpl for FCastTextOverlay {
        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();
            obj.add_pad(&self.video_sink_pad).unwrap();
            obj.add_pad(&self.text_sink_pad).unwrap();
            obj.add_pad(&self.src_pad).unwrap();
        }
    }

    impl GstObjectImpl for FCastTextOverlay {}

    fn video_pad_template() -> gst::Caps {
        let mut caps = gst::Caps::new_empty();
        {
            let caps = caps.get_mut().unwrap();
            caps.append(gst_video::VideoCapsBuilder::new().any_features().build());
            caps.append(gst_video::VideoCapsBuilder::new().build());
        }

        caps
    }

    impl ElementImpl for FCastTextOverlay {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "...",
                        "Video/Overlay/Subtitle",
                        "...",
                        "...",
                    )
                });

            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let caps = video_pad_template();
                let src = gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Sometimes,
                    &caps,
                )
                .unwrap();
                let video_sink = gst::PadTemplate::new(
                    "video_sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap();
                let text_sink = gst::PadTemplate::new(
                    "text_sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &gst::Caps::builder("text/x-raw")
                        .field("format", gst::List::new(["utf8", "pango-markup"]))
                        .build(),
                )
                .unwrap();

                vec![video_sink, text_sink, src]
            });

            PAD_TEMPLATES.as_ref()
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            if transition == gst::StateChange::PausedToReady {
                let mut state = self.state.lock();
                state.text_flushing = true;
                state.video_flushing = true;
                state.text_buffer = None;
                self.state_cvar.notify_all();
            }

            let ret = self.parent_change_state(transition)?;

            if transition == gst::StateChange::ReadyToPaused {
                let mut state = self.state.lock();
                state.text_flushing = false;
                state.video_flushing = false;
                state.video_eos = false;
                state.text_eos = false;
                state.segment = gst::Segment::new();
                state.text_segment = gst::Segment::new();
            }

            Ok(ret)
        }
    }
}

glib::wrapper! {
    pub struct FCastTextOverlay(ObjectSubclass<imp::FCastTextOverlay>)
        @extends gst::Element, gst::Object;
}

pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "fcasttextoverlay",
        gst::Rank::PRIMARY,
        FCastTextOverlay::static_type(),
    )
}
