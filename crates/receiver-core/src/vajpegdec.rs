use gst::{glib, prelude::*};

glib::wrapper! {
    pub struct FVaJpegDec(ObjectSubclass<imp::FVaJpegDec>)
        @extends gst::Bin, gst::Element, gst::Object;
}

pub mod imp {
    use std::sync::LazyLock;

    use gst::{glib, prelude::*, subclass::prelude::*};

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "fvajpegdec",
            gst::DebugColorFlags::empty(),
            Some("FCast hardware JPEG still decoder"),
        )
    });

    #[derive(Default)]
    pub struct FVaJpegDec;

    #[glib::object_subclass]
    impl ObjectSubclass for FVaJpegDec {
        const NAME: &'static str = "FVaJpegDec";
        type Type = super::FVaJpegDec;
        type ParentType = gst::Bin;
    }

    impl ObjectImpl for FVaJpegDec {
        fn constructed(&self) {
            self.parent_constructed();
            // Never panic on a missing element or a failed link. Registration is
            // gated on the required elements existing (see plugin_init), so this
            // is belt-and-suspenders: on failure the bin is left without pads and
            // decodebin3 autoplug falls back to fimagedec.
            if self.try_build().is_none() {
                gst::error!(
                    CAT,
                    "fvajpegdec: could not assemble the decode chain; leaving it to fimagedec"
                );
            }
        }
    }

    impl FVaJpegDec {
        /// Assemble `jpegparse ! vajpegdec` plus the ghost pads and probes.
        /// Returns `None` (without panicking) if any element or pad is
        /// unavailable.
        fn try_build(&self) -> Option<()> {
            let obj = self.obj();

            let parse = gst::ElementFactory::make("jpegparse").build().ok()?;
            let dec = gst::ElementFactory::make("vajpegdec").build().ok()?;
            obj.add_many([&parse, &dec]).ok()?;
            gst::Element::link_many([&parse, &dec]).ok()?;

            // Relabel the incoming CAPS event image/x-fcast-jpeg -> image/jpeg
            // so jpegparse accepts it. The probe fires before jpegparse's own
            // event handler, so the rewritten caps is what it validates.
            let parse_sink = parse.static_pad("sink")?;
            parse_sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && let gst::EventView::Caps(caps_ev) = event.view()
                    && let Some(structure) = caps_ev.caps().structure(0)
                {
                    let mut structure = structure.to_owned();
                    structure.set_name("image/jpeg");
                    let new_caps = gst::Caps::builder_full().structure(structure).build();
                    info.data = Some(gst::PadProbeData::Event(gst::event::Caps::new(&new_caps)));
                }
                gst::PadProbeReturn::Ok
            });

            // On the decoder src pad: announce the image stream the way fimagedec
            // does (from the decoded caps), and swallow EOS so the single decoded
            // frame is held on screen with the pipeline in PLAYING (fimagedec's
            // park behaviour, without imagefreeze).
            let dec_src = dec.static_pad("src")?;
            let weak = obj.downgrade();
            dec_src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                let Some(gst::PadProbeData::Event(event)) = &info.data else {
                    return gst::PadProbeReturn::Ok;
                };
                match event.view() {
                    gst::EventView::Caps(caps_ev) => {
                        if let Some(obj) = weak.upgrade()
                            && let Ok(vinfo) = gst_video::VideoInfo::from_caps(caps_ev.caps())
                        {
                            let s =
                                gst::Structure::builder(crate::imagedec::imp::IMAGE_STREAM_MESSAGE)
                                    .field("format", "jpeg")
                                    .field("width", vinfo.width() as i32)
                                    .field("height", vinfo.height() as i32)
                                    .field("animated", false)
                                    .build();
                            let msg = gst::message::Element::builder(s).src(&obj).build();
                            let _ = obj.post_message(msg);
                        }
                        gst::PadProbeReturn::Ok
                    }
                    // A still is held forever: swallow EOS so the pipeline stays
                    // PLAYING showing the frame (mirrors fimagedec).
                    gst::EventView::Eos(_) => gst::PadProbeReturn::Drop,
                    _ => gst::PadProbeReturn::Ok,
                }
            });

            // Ghost pads: sink -> jpegparse.sink, src -> vajpegdec.src.
            //
            // The sink advertises the private image/x-fcast-jpeg but jpegparse
            // only accepts image/jpeg, so caps interactions must be translated
            // in BOTH directions:
            //   - QUERIES (decodebin3 ACCEPT_CAPS/CAPS this sink before and
            //     during linking): answered here from the template, so nothing
            //     proxies image/x-fcast-jpeg down to jpegparse (which would
            //     reject it, and decodebin3 would fall back to fimagedec / the
            //     link would fail not-negotiated).
            //   - the downstream CAPS EVENT: rewritten to image/jpeg by the
            //     probe on jpegparse.sink above.
            // There is deliberately no `identity` in front: a passthrough would
            // proxy the ACCEPT_CAPS query straight to jpegparse and reject it.
            let private_caps = gst::Caps::new_empty_simple("image/x-fcast-jpeg");
            let sink_ghost = gst::GhostPad::builder_from_template(&obj.pad_template("sink")?)
                .query_function(move |pad, parent, query| match query.view_mut() {
                    gst::QueryViewMut::AcceptCaps(q) => {
                        let accepted = q.caps().can_intersect(&private_caps);
                        q.set_result(accepted);
                        true
                    }
                    gst::QueryViewMut::Caps(q) => {
                        let result = match q.filter() {
                            Some(filter) => filter.intersect(&private_caps),
                            None => private_caps.clone(),
                        };
                        q.set_result(&result);
                        true
                    }
                    _ => gst::Pad::query_default(pad, parent, query),
                })
                .build();
            sink_ghost.set_target(Some(&parse_sink)).ok()?;
            obj.add_pad(&sink_ghost).ok()?;

            let src_ghost = gst::GhostPad::builder_from_template(&obj.pad_template("src")?).build();
            src_ghost.set_target(Some(&dec_src)).ok()?;
            obj.add_pad(&src_ghost).ok()?;

            gst::debug!(CAT, "fvajpegdec bin constructed");
            Some(())
        }
    }

    impl GstObjectImpl for FVaJpegDec {}
    impl BinImpl for FVaJpegDec {}

    impl ElementImpl for FVaJpegDec {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "FCast hardware JPEG still decoder",
                    // Decoder/Video so decodebin3 autoplugs it like any decoder.
                    "Codec/Decoder/Video/Hardware",
                    "Decodes baseline JPEG stills on the GPU (vajpegdec) and holds the frame",
                    "FCast contributors",
                )
            });
            Some(&*METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let sink_caps = gst::Caps::new_empty_simple("image/x-fcast-jpeg");
                // ANY so the VA / dmabuf output caps negotiate freely to the sink.
                let src_caps = gst::Caps::new_any();
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
    }
}

/// Register `fvajpegdec` above `fimagedec` for baseline JPEG, but only when VA
/// JPEG decode is actually usable. See the module docs for the gate rationale.
#[cfg(target_os = "linux")]
pub fn plugin_init() -> Result<(), glib::BoolError> {
    // Respect the soak-harness escape hatch: if VA is force-disabled, images
    // stay on the software path like every other VA element.
    if std::env::var_os("FCAST_DISABLE_VA").is_some() {
        return Ok(());
    }
    // Only register when the whole chain can actually be built: the `jpegparse`
    // factory must exist (from gst-plugins-bad `jpegformat`, re-enabled on Linux
    // in xtask ENABLE_LINUX) and VA JPEG decode must work on this GPU. Otherwise
    // baseline JPEG stays on fimagedec and we never instantiate (and thus never
    // crash) this element.
    let have_parser = gst::ElementFactory::find("jpegparse").is_some();
    let have_va = va_jpeg_decode_available();
    if have_va && have_parser {
        tracing::info!("fvajpegdec: registering VA-API JPEG still decoder (rank PRIMARY+32)");
        return gst::Element::register(
            None,
            "fvajpegdec",
            // Above fimagedec (PRIMARY) so baseline JPEG prefers the GPU.
            gst::Rank::PRIMARY + 32,
            FVaJpegDec::static_type(),
        );
    }
    if have_va && !have_parser {
        // VA JPEG decode works but the parser is absent: the static GStreamer
        // was built without gst-plugins-bad `jpegformat`. Rebuild it (with a
        // meson reconfigure, not just a relink) so jpegparse is included,
        // otherwise JPEG stays on the CPU (fimagedec) instead of the GPU.
        tracing::warn!(
            "fvajpegdec: vajpegdec is present but jpegparse is missing, so hardware \
             JPEG decode is DISABLED (JPEG will use the CPU fimagedec path). Rebuild \
             the static GStreamer with gst-plugins-bad 'jpegformat' enabled and \
             reconfigure meson."
        );
    } else {
        tracing::debug!(
            have_va,
            have_parser,
            "fvajpegdec: not registering; JPEG stays on fimagedec"
        );
    }
    Ok(())
}

/// Whether `vajpegdec` exists and can reach READY (VA display present and a JPEG
/// decode entrypoint on this GPU). Creating the element and changing state is
/// the cheapest reliable check short of a full decode.
#[cfg(target_os = "linux")]
fn va_jpeg_decode_available() -> bool {
    let Ok(dec) = gst::ElementFactory::make("vajpegdec").build() else {
        return false;
    };
    let usable = dec.set_state(gst::State::Ready).is_ok();
    let _ = dec.set_state(gst::State::Null);
    usable
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    /// When the parser and a working VA JPEG decoder are both present, the gate
    /// must register `fvajpegdec`. Guarded so it is a no-op on a box without VA
    /// or jpegparse (CI without a GPU), where it only prints the state.
    #[test]
    fn registers_when_dependencies_present() {
        gst::init().unwrap();
        let have_parser = gst::ElementFactory::find("jpegparse").is_some();
        let have_va = super::va_jpeg_decode_available();
        // let _: another test in this module may already have registered it.
        let _ = super::plugin_init();
        let registered = gst::ElementFactory::find("fvajpegdec").is_some();
        eprintln!("jpegparse={have_parser} va_jpeg={have_va} fvajpegdec_registered={registered}");
        if have_parser && have_va {
            assert!(
                registered,
                "fvajpegdec must register when jpegparse and VA JPEG decode are present"
            );
        }
    }

    /// Regression for the caps-translation traps: a baseline JPEG must decode
    /// through `fvajpegdec` (the GPU path), not fall back to `fimagedec`. This
    /// exercises decodebin3's ACCEPT_CAPS query + the downstream caps event
    /// across the private image/x-fcast-jpeg -> image/jpeg boundary (the
    /// not-negotiated bug). No-op on a box without VA JPEG decode.
    #[test]
    fn baseline_jpeg_decodes_through_fvajpegdec() {
        use gst::prelude::*;
        use std::sync::{
            Arc, Mutex,
            atomic::{AtomicUsize, Ordering},
        };

        static BASELINE_JPEG: &[u8] = include_bytes!("testdata/baseline.jpg");

        gst::init().unwrap();
        let _ = crate::imagetypefind::plugin_init();
        let _ = crate::imagedec::plugin_init();
        let _ = super::plugin_init();
        if gst::ElementFactory::find("fvajpegdec").is_none() {
            eprintln!("skipping: no VA JPEG decode on this box");
            return;
        }

        let pipeline = gst::Pipeline::new();
        let src = gst_app::AppSrc::builder().build();
        let dec = gst::ElementFactory::make("decodebin3").build().unwrap();
        let sink = gst::ElementFactory::make("fakesink").build().unwrap();
        pipeline
            .add_many([src.upcast_ref::<gst::Element>(), &dec, &sink])
            .unwrap();
        src.upcast_ref::<gst::Element>().link(&dec).unwrap();

        let frames = Arc::new(AtomicUsize::new(0));
        let frames_probe = frames.clone();
        sink.static_pad("sink")
            .unwrap()
            .add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                frames_probe.fetch_add(1, Ordering::Relaxed);
                gst::PadProbeReturn::Ok
            });
        let sink_weak = sink.downgrade();
        dec.connect_pad_added(move |_, pad| {
            if let Some(sink) = sink_weak.upgrade() {
                let sp = sink.static_pad("sink").unwrap();
                if !sp.is_linked() {
                    let _ = pad.link(&sp);
                }
            }
        });
        let elems = Arc::new(Mutex::new(Vec::<String>::new()));
        let elems_cb = elems.clone();
        pipeline.connect_deep_element_added(move |_, _, el| {
            if let Some(fac) = el.factory() {
                elems_cb.lock().unwrap().push(fac.name().to_string());
            }
        });

        pipeline.set_state(gst::State::Playing).unwrap();
        let _ = src.push_buffer(gst::Buffer::from_slice(BASELINE_JPEG));
        let _ = src.end_of_stream();

        let bus = pipeline.bus().unwrap();
        let mut error = None;
        let start = std::time::Instant::now();
        while start.elapsed() < std::time::Duration::from_secs(10) {
            if let Some(msg) = bus.timed_pop(gst::ClockTime::from_mseconds(100)) {
                if let gst::MessageView::Error(e) = msg.view() {
                    error = Some(e.error().to_string());
                    break;
                }
            }
            if frames.load(Ordering::Relaxed) > 0 {
                break;
            }
        }
        let _ = pipeline.set_state(gst::State::Null);

        let elems = elems.lock().unwrap().clone();
        assert!(
            error.is_none(),
            "pipeline error: {error:?} (elems: {elems:?})"
        );
        assert!(
            frames.load(Ordering::Relaxed) > 0,
            "no frame decoded (elems: {elems:?})"
        );
        assert!(
            elems.iter().any(|n| n == "fvajpegdec"),
            "baseline JPEG did not go through fvajpegdec (elems: {elems:?})"
        );
        assert!(
            !elems.iter().any(|n| n == "fimagedec"),
            "baseline JPEG fell back to fimagedec instead of the GPU path (elems: {elems:?})"
        );
    }
}
