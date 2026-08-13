//! `ftestparse`: near-identity parsers for the fcasttest caps.
//!
//! Exists so `urisourcebin parse-streams=true` builds the same parsebin
//! chain it builds in production. The src caps add `parsed=true`, which lets
//! parsebin stop parsing and expose the stream once it sees a decoder next.
//!
//! Registered above the decoders' PRIMARY, as real parsers outrank their
//! decoders. On a rank tie parsebin would expose the unparsed pad instead of
//! plugging the parser.

use gst::glib;

/// Sink to src adds `parsed=true`, src to sink drops the field.
fn transform_caps(
    direction: gst::PadDirection,
    caps: &gst::Caps,
    filter: Option<&gst::Caps>,
) -> gst::Caps {
    let mut transformed = caps.copy();
    {
        let transformed = transformed.get_mut().unwrap();
        for structure in transformed.iter_mut() {
            if direction == gst::PadDirection::Src {
                structure.remove_field("parsed");
            } else {
                structure.set("parsed", true);
            }
        }
    }

    match filter {
        Some(filter) => filter.intersect_with_mode(&transformed, gst::CapsIntersectMode::First),
        None => transformed,
    }
}

/// Both parsers are the same passthrough element with different caps and klass,
/// which is all parsebin matches on.
macro_rules! ftest_parser {
    (
        $module:ident,
        $imp_name:ident,
        $wrapper:ident,
        $type_name:literal,
        $factory:literal,
        $media:expr,
        $klass:literal,
        $long_name:literal
    ) => {
        pub mod $module {
            mod imp {
                use std::sync::LazyLock;

                use gst::{glib, subclass::prelude::*};
                use gst_base::subclass::{BaseTransformMode, prelude::*};

                #[derive(Default)]
                pub struct $imp_name;

                #[glib::object_subclass]
                impl ObjectSubclass for $imp_name {
                    const NAME: &'static str = $type_name;
                    type Type = super::$wrapper;
                    type ParentType = gst_base::BaseTransform;
                }

                impl ObjectImpl for $imp_name {}
                impl GstObjectImpl for $imp_name {}

                impl ElementImpl for $imp_name {
                    fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
                        static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> =
                            LazyLock::new(|| {
                                gst::subclass::ElementMetadata::new(
                                    $long_name,
                                    $klass,
                                    "Marks fcasttest streams as parsed, passing data through",
                                    "Marcus Hanestad <marcus@futo.org>",
                                )
                            });

                        Some(&*ELEMENT_METADATA)
                    }

                    fn pad_templates() -> &'static [gst::PadTemplate] {
                        static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> =
                            LazyLock::new(|| {
                                let sink_caps = gst::Caps::new_empty_simple($media);
                                let src_caps =
                                    gst::Caps::builder($media).field("parsed", true).build();
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

                impl BaseTransformImpl for $imp_name {
                    const MODE: BaseTransformMode = BaseTransformMode::AlwaysInPlace;
                    const PASSTHROUGH_ON_SAME_CAPS: bool = true;
                    const TRANSFORM_IP_ON_PASSTHROUGH: bool = false;

                    fn transform_caps(
                        &self,
                        direction: gst::PadDirection,
                        caps: &gst::Caps,
                        filter: Option<&gst::Caps>,
                    ) -> Option<gst::Caps> {
                        Some(crate::parse::transform_caps(direction, caps, filter))
                    }

                    fn transform_ip(
                        &self,
                        _buffer: &mut gst::BufferRef,
                    ) -> Result<gst::FlowSuccess, gst::FlowError> {
                        Ok(gst::FlowSuccess::Ok)
                    }
                }
            }

            use gst::{glib, prelude::*};

            glib::wrapper! {
                pub struct $wrapper(ObjectSubclass<imp::$imp_name>)
                    @extends gst_base::BaseTransform, gst::Element, gst::Object;
            }

            pub fn register() -> Result<(), glib::BoolError> {
                gst::Element::register(
                    None,
                    $factory,
                    gst::Rank::PRIMARY + 1,
                    $wrapper::static_type(),
                )
            }
        }
    };
}

ftest_parser!(
    video,
    FTestVideoParse,
    FTestVideoParse,
    "FTestVideoParse",
    "ftestvparse",
    crate::caps::VIDEO_MEDIA_TYPE,
    "Codec/Parser/Video",
    "FCast Test Video Parser"
);

ftest_parser!(
    audio,
    FTestAudioParse,
    FTestAudioParse,
    "FTestAudioParse",
    "ftestaparse",
    crate::caps::AUDIO_MEDIA_TYPE,
    "Codec/Parser/Audio",
    "FCast Test Audio Parser"
);

pub fn register() -> Result<(), glib::BoolError> {
    video::register()?;
    audio::register()
}
