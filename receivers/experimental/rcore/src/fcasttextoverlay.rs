use gst::glib::{self, types::StaticType};

mod imp {
    use parking_lot::Mutex;
    use std::sync::LazyLock;

    use gst::{glib, prelude::*, subclass::prelude::*};

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
        text_segment_position: Option<gst::ClockTime>,
    }

    pub struct FCastTextOverlay {
        state: Mutex<State>,
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
            // tracing::debug!(?event, "video sink event");

            // overlay.src_pad.push_event(event)

            use gst::EventView;

            tracing::debug!(?event, "video sink event");

            // return gst::Pad::event_default(pad, parent, event);

            let execute_default = true;

            let mut state = self.state.lock();
            match event.view() {
                EventView::StreamStart(_) => {
                    state.video_flushing = false;
                    state.video_eos = false;
                    state.segment = gst::Segment::new();
                }
                // EventView::Caps(_) => {
                //     ret = gst_base_text_overlay_setcaps
                // }
                EventView::Segment(seg) => {
                    let seg = seg.segment();
                    if seg.format() == gst::Format::Time {
                        state.segment = seg.clone();
                    } else {
                        // TODO: log
                    }
                }
                EventView::Eos(_) => {
                    state.video_eos = true;
                }
                EventView::FlushStart(_) => {
                    state.video_flushing = true;
                }
                EventView::FlushStop(_) => {
                    state.video_flushing = false;
                    state.video_eos = false;
                    state.segment = gst::Segment::new();
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

        fn video_sink_chain(
            &self,
            buffer: gst::Buffer,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            self.src_pad.push(buffer)
        }

        fn video_sink_query(
            &self,
            pad: &gst::Pad,
            parent: Option<&gst::Object>,
            query: &mut gst::QueryRef,
        ) -> bool {
            if let gst::QueryViewMut::Caps(q) = query.view_mut() {
                let overlay_filter = if let Some(filter) = q.filter() {
                    let sw_caps = gst_video::VideoCapsBuilder::new()
                        .features([gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION])
                        .build();

                    Some(sw_caps.intersect(filter))
                } else {
                    None
                };

                tracing::debug!(?overlay_filter);
                // let peer_caps = elem.video_sink_pad.peer_query_caps(overlay_filter.as_ref());
                let peer_caps = pad.peer_query_caps(overlay_filter.as_ref());
                if let Some(peer) = pad.peer() {
                    gst::debug!(
                        CAT,
                        obj = pad,
                        "peer_name {} peer_parent_name {}",
                        peer.name(),
                        peer.parent().unwrap().name()
                    );
                }
                gst::debug!(CAT, obj = pad, "peer caps {peer_caps:?}");

                let result_caps;
                if peer_caps.is_any() {
                    result_caps = Some(pad.pad_template_caps());
                } else {
                    /* duplicate caps which contains the composition into one version with
                     * the meta and one without. Filter the other caps by the software caps */
                    // GstCaps *sw_caps = gst_static_caps_get (&sw_template_caps);
                    // caps = gst_base_text_overlay_intersect_by_feature (peer_caps,
                    //     GST_CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION, sw_caps);
                    // gst_caps_unref (sw_caps);

                    let sw_caps = gst_video::VideoCapsBuilder::new().any_features().build();
                    let mut new_caps = gst::Caps::new_empty();
                    let new_caps_mut = new_caps.get_mut().unwrap();
                    for (idx, caps) in sw_caps.iter().enumerate() {
                        let Some(features) = sw_caps.features(idx) else {
                            continue;
                        };
                        let mut features = features.to_owned();
                        tracing::debug!(?caps, ?features, "asdf");
                        let simple_caps = gst::Caps::builder_full()
                            .structure_with_features(caps.to_owned(), features.to_owned())
                            .build();
                        let filtered_caps;
                        if features
                            .contains(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION)
                        {
                            new_caps_mut.append(simple_caps.clone());
                            tracing::debug!("adding simple caps");
                            features
                                .remove(gst_video::CAPS_FEATURE_META_GST_VIDEO_OVERLAY_COMPOSITION);
                            filtered_caps = simple_caps;
                        } else {
                            // filtered_caps = simple_caps.intersect(filtered_caps);
                            // filtered_caps = simple_caps.intersect(simple_caps.to_o());
                            filtered_caps = simple_caps.to_owned()
                        }

                        new_caps_mut.append(filtered_caps);
                    }

                    new_caps_mut.append(
                        gst_video::VideoCapsBuilder::new()
                            .features(["memory:DMABuf"])
                            .format(gst_video::VideoFormat::DmaDrm)
                            .build(),
                    );

                    // result_caps = Some(pad.pad_template_caps());
                    result_caps = Some(new_caps);
                    // result_caps = Some(pad.peer_query_caps(None));
                }

                gst::debug!(CAT, obj = pad, "returning {result_caps:?}");
                q.set_result(&result_caps);

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
            use gst::EventView;

            // return gst::Pad::event_default(pad, parent, event);

            let mut execute_default = true;
            let mut state = self.state.lock();
            match event.view() {
                EventView::StreamStart(_) => {
                    state.text_flushing = false;
                    state.text_eos = false;
                    state.segment = gst::Segment::new();
                }
                EventView::Caps(_) => {
                    // TODO: have_pango_markup
                }
                EventView::Segment(seg) => {
                    let seg = seg.segment();
                    if seg.format() == gst::Format::Time {
                        state.text_segment = seg.clone();
                    } else {
                        // TODO: log
                    }

                    // TODO:
                    /* wake up the video chain, it might be waiting for a text buffer or
                     * a text segment update */
                    // GST_BASE_TEXT_OVERLAY_LOCK (overlay);
                    // GST_BASE_TEXT_OVERLAY_BROADCAST (overlay);
                    // GST_BASE_TEXT_OVERLAY_UNLOCK (overlay);
                }
                EventView::Gap(gap) => {
                    let (mut start, duration) = gap.get();
                    if let Some(duration) = duration {
                        start += duration;
                    }
                    state.text_segment.set_position(start);

                    // /* wake up the video chain, it might be waiting for a text buffer or
                    // * a text segment update */
                    // GST_BASE_TEXT_OVERLAY_LOCK (overlay);
                    // GST_BASE_TEXT_OVERLAY_BROADCAST (overlay);
                    // GST_BASE_TEXT_OVERLAY_UNLOCK (overlay);

                    execute_default = false;
                }
                EventView::Eos(_) => {
                    state.text_eos = false;
                    execute_default = false;
                }
                EventView::FlushStart(_) => {
                    state.text_flushing = true;
                    execute_default = false;
                    // TODO: broadcast cond
                }
                EventView::FlushStop(_) => {
                    state.text_flushing = false;
                    state.text_eos = false;
                    state.segment = gst::Segment::new();
                    execute_default = false;
                    // TODO: pop text
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

        fn text_sink_chain(&self, buffer: gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            tracing::debug!(?buffer, "text sink chain");
            let Some(start) = buffer.pts() else {
                // TODO: log
                return Ok(gst::FlowSuccess::Ok);
            };

            let Some(stop) = buffer.duration().map(|duration| start + duration) else {
                // TODO: log
                return Ok(gst::FlowSuccess::Ok);
            };

            Ok(gst::FlowSuccess::Ok)
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
            tracing::debug!(?query, "video sink query (before)");
            let res = self.video_sink_pad.peer_query(query);
            tracing::debug!(?query, "video sink query (after)");
            res
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
                    tracing::debug!(?buffer, "video sink chain");
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
                    tracing::debug!(?event, "text sink event");
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
                    tracing::debug!(?query, "src query");
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
                    // &gst_video::VideoCapsBuilder::new()
                    //     .any_features()
                    //     .format_list(gst_video::VIDEO_FORMATS_ALL.as_ref().iter().cloned())
                    //     .build(),
                )
                .unwrap();
                let video_sink = gst::PadTemplate::new(
                    "video_sink",
                    gst::PadDirection::Sink,
                    gst::PadPresence::Always,
                    &caps,
                    // &gst_video::VideoCapsBuilder::new()
                    //     .any_features()
                    //     .format_list(gst_video::VIDEO_FORMATS_ALL.as_ref().iter().cloned())
                    //     .build(),
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
