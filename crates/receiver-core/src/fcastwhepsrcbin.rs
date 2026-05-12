// URI handler for `fcastwhep://` webrtc source

use gst::glib::{self, types::StaticType};

mod imp {
    use std::sync::{LazyLock, Mutex};

    use gst::{glib, prelude::*, subclass::prelude::*};

    #[derive(Default)]
    struct State {
        pub uri: Option<String>,
    }

    #[derive(Default)]
    pub struct FCastWhepSrcBin {
        state: Mutex<State>,
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FCastWhepSrcBin {
        const NAME: &str = "FCastWhepSrcBin";
        type Type = super::FCastWhepSrcBin;
        type ParentType = gst::Bin;
        type Interfaces = (gst::URIHandler,);
    }

    impl ObjectImpl for FCastWhepSrcBin {}

    impl GstObjectImpl for FCastWhepSrcBin {}

    impl ElementImpl for FCastWhepSrcBin {
        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let src_pad_template = gst::PadTemplate::new(
                    "src_%u",
                    gst::PadDirection::Src,
                    gst::PadPresence::Sometimes,
                    &gst::Caps::new_empty_simple("application/x-rtp"),
                )
                .unwrap();

                vec![src_pad_template]
            });

            PAD_TEMPLATES.as_ref()
        }
    }

    impl BinImpl for FCastWhepSrcBin {}

    impl URIHandlerImpl for FCastWhepSrcBin {
        const URI_TYPE: gst::URIType = gst::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["fcastwhep"]
        }

        fn uri(&self) -> Option<String> {
            self.state
                .lock()
                .unwrap()
                .uri
                .clone()
                .map(|uri| uri.replace("http://", "fcastwhep://"))
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            let uri = uri.replace("fcastwhep://", "http://");

            let mut state = self.state.lock().unwrap();

            let element = self.obj();
            let bin: &gst::Bin = element.upcast_ref();

            let whepsrc = gst::ElementFactory::make("whepsrc")
                .property("whep-endpoint", &uri)
                .build()
                .map_err(|err| {
                    glib::Error::new(gst::PluginError::Dependencies, &err.to_string())
                })?;

            bin.add(&whepsrc).unwrap();

            whepsrc.connect_pad_added({
                let bin_weak = bin.downgrade();
                move |_, pad| {
                    let Some(bin) = bin_weak.upgrade() else {
                        return;
                    };
                    if let Ok(ghost) = gst::GhostPad::with_target(pad) {
                        let _ = bin.add_pad(&ghost);
                    }
                }
            });

            state.uri = Some(uri);

            Ok(())
        }
    }
}

glib::wrapper! {
    pub struct FCastWhepSrcBin(ObjectSubclass<imp::FCastWhepSrcBin>)
        @extends gst::Bin, gst::Element, gst::Object,
        @implements gst::URIHandler;
}

pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "fcastwhepsrcbin",
        gst::Rank::NONE,
        FCastWhepSrcBin::static_type(),
    )
}
