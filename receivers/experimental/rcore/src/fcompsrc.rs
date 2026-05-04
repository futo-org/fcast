use fcast_protocol::companion;
use gst::{glib, prelude::*};
use url::Url;

#[derive(Debug, Clone, Copy)]
pub struct FCompUrl {
    pub provider_id: companion::ProviderId,
    pub resource_id: companion::ResourceId,
}

impl FCompUrl {
    pub fn new(url: &Url) -> Option<Self> {
        let url::Host::Domain(host) = url.host()? else {
            return None;
        };
        let mut host_parts = host.split('.');
        let provider_id = host_parts.next()?.parse::<u16>().ok()?;
        if host_parts.next()? != "fcast" {
            return None;
        }
        let resource_id = url.path().strip_prefix('/')?.parse::<u32>().ok()?;

        Some(Self {
            provider_id,
            resource_id,
        })
    }
}

pub mod imp {
    use fcast_protocol::companion;
    use gst::{glib, prelude::*, subclass::prelude::*};
    use gst_base::subclass::prelude::*;
    use parking_lot::Mutex;
    use std::sync::LazyLock;
    use url::Url;

    use crate::fcompsrc::FCompUrl;

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "fcompsrc",
            gst::DebugColorFlags::empty(),
            Some("FCompanion source"),
        )
    });

    pub const FCOMP_CONTEXT: &str = "fcast.fcomp.context";
    const DEFAULT_CACHE_SIZE: u64 = 1024 * 1024 * 2; // 2MiB

    #[derive(Clone, Debug, glib::Boxed)]
    #[boxed_type(name = "FCompContext")]
    pub struct CompContext(pub crate::fcast::CompanionContext);

    struct DataCache {
        data: Vec<u8>,
        /// The offset into the file this cache starts at
        start: u64,
    }

    impl DataCache {
        fn new() -> Self {
            Self {
                data: Vec::new(),
                start: 0,
            }
        }

        #[inline]
        fn update(&mut self, data: Vec<u8>, offset: u64) {
            self.data = data;
            self.start = offset;
        }

        #[inline]
        fn can_read(&self, offset: u64, length: usize) -> bool {
            let cache_end = self.start + (self.data.len() as u64);
            let read_end = offset + length as u64;

            let offset_in_cache = offset >= self.start && offset < cache_end;
            let length_in_cache = read_end <= cache_end;

            offset_in_cache && length_in_cache
        }

        #[inline]
        fn read(&self, offset: u64, length: usize) -> &[u8] {
            let read_end = offset + length as u64;
            let read_start = offset - self.start;

            let read_end = self.data.len().min((read_end - self.start) as usize);

            &self.data[read_start as usize..read_end as usize]
        }
    }

    #[derive(Default)]
    enum State {
        #[default]
        Stopped,
        Started {
            size: companion::ResourceSize,
            head_cache: DataCache,
            cache: DataCache,
            tail_cache: DataCache,
        },
    }

    #[derive(Default)]
    struct Settings {
        url: Option<Url>,
        comp_url: Option<super::FCompUrl>,
    }

    impl Settings {
        fn comp_url(&self) -> Result<super::FCompUrl, gst::ErrorMessage> {
            let Some(url) = self.comp_url.as_ref().cloned() else {
                return Err(gst::error_msg!(
                    gst::ResourceError::Failed,
                    ["Missing parsed companion URL"]
                ));
            };
            Ok(url)
        }
    }

    #[derive(Default)]
    pub struct FCompSrc {
        state: Mutex<State>,
        settings: Mutex<Settings>,
        context: Mutex<Option<CompContext>>,
    }

    impl FCompSrc {
        fn current_context(&self) -> Option<CompContext> {
            if let Some(ref ctx) = *self.context.lock() {
                Some(ctx.clone())
            } else {
                None
            }
        }

        fn ensure_context(&self) -> Result<CompContext, gst::ErrorMessage> {
            if let Some(ctx) = self.current_context() {
                return Ok(ctx);
            }

            let _ = self.obj().post_message(
                gst::message::NeedContext::builder(FCOMP_CONTEXT)
                    .src(&*self.obj())
                    .build(),
            );

            if let Some(ctx) = self.current_context() {
                return Ok(ctx);
            }

            Err(gst::error_msg!(
                gst::ResourceError::Failed,
                ["Failed to get companion context"]
            ))
        }

        fn start(&self, url: FCompUrl) -> Result<(), gst::ErrorMessage> {
            let context = self.ensure_context()?;
            let provider = context
                .0
                .get_provider(url.provider_id)
                .ok_or(gst::error_msg!(
                    gst::ResourceError::NotFound,
                    ["Provider not found id={}", url.provider_id]
                ))?;

            let info_rx = provider.get_resource_info_blocking(url.resource_id);
            let info = info_rx.recv().unwrap();

            *self.state.lock() = State::Started {
                size: info.resource_size,
                head_cache: DataCache::new(),
                cache: DataCache::new(),
                tail_cache: DataCache::new(),
            };

            Ok(())
        }
    }

    impl ObjectImpl for FCompSrc {}

    impl GstObjectImpl for FCompSrc {}

    impl ElementImpl for FCompSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "FCompanion Source",
                        "Source/Network/FCOMP",
                        "Read from an FComp source",
                        "Marcus Hanestad <marcus@futo.org>",
                    )
                });

            Some(&*ELEMENT_METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static PAD_TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                let caps = gst::Caps::new_any();
                let src_pad_template = gst::PadTemplate::new(
                    "src",
                    gst::PadDirection::Src,
                    gst::PadPresence::Always,
                    &caps,
                )
                .unwrap();

                vec![src_pad_template]
            });

            PAD_TEMPLATES.as_ref()
        }

        fn set_context(&self, context: &gst::Context) {
            if context.context_type() == FCOMP_CONTEXT {
                let mut comp = self.context.lock();
                let s = context.structure();
                *comp = s
                    .get::<&CompContext>("context")
                    .map(|c| Some(c.clone()))
                    .unwrap_or(None);
            }

            self.parent_set_context(context);
        }
    }

    impl BaseSrcImpl for FCompSrc {
        fn is_seekable(&self) -> bool {
            true
        }

        fn size(&self) -> Option<u64> {
            let state = self.state.lock();
            match *state {
                State::Started { ref size, .. } => match size {
                    companion::ResourceSize::Unknown => None,
                    companion::ResourceSize::Known(len) => Some(len.0),
                },
                _ => None,
            }
        }

        fn start(&self) -> Result<(), gst::ErrorMessage> {
            let url = self.settings.lock().comp_url()?;
            gst::debug!(CAT, imp = self, "Starting for URL {url:?}");

            self.start(url)
        }

        fn stop(&self) -> Result<(), gst::ErrorMessage> {
            gst::debug!(CAT, imp = self, "Stopping");
            *self.state.lock() = State::Stopped;

            Ok(())
        }

        fn fill(
            &self,
            offset: u64,
            length: u32,
            buffer: &mut gst::BufferRef,
        ) -> Result<gst::FlowSuccess, gst::FlowError> {
            let mut state = self.state.lock();
            let State::Started {
                size,
                head_cache,
                cache,
                tail_cache,
            } = &mut *state
            else {
                gst::element_imp_error!(self, gst::CoreError::Failed, ["Not started yet"]);
                return Err(gst::FlowError::Error);
            };

            let active_cache = {
                let full_len = match size {
                    companion::ResourceSize::Unknown => 0u64,
                    companion::ResourceSize::Known(size) => (**size).into(),
                };

                // Optimize random reads that often happen when initally probing the file
                if offset < DEFAULT_CACHE_SIZE || head_cache.can_read(offset, length as usize) {
                    &mut *head_cache
                } else if offset > full_len.saturating_sub(DEFAULT_CACHE_SIZE) {
                    &mut *tail_cache
                } else {
                    &mut *cache
                }
            };

            // TODO: write test for the whole elment, needs to test shutdowns too

            if !active_cache.can_read(offset, length as usize) {
                let context = self.ensure_context().map_err(|err| {
                    gst::element_imp_error!(self, gst::ResourceError::Failed, ["{err}"]);
                    gst::FlowError::Error
                })?;
                let url = self.settings.lock().comp_url().map_err(|err| {
                    gst::element_imp_error!(self, gst::ResourceError::Failed, ["{err}"]);
                    gst::FlowError::Error
                })?;
                let Some(provider) = context.0.get_provider(url.provider_id) else {
                    gst::element_imp_error!(
                        self,
                        gst::ResourceError::NotFound,
                        ["Could not find provider id={}", url.provider_id]
                    );
                    return Err(gst::FlowError::Error);
                };
                let cache_length = (length as u64).max(DEFAULT_CACHE_SIZE);
                let read_head = companion::ReadHead::Range {
                    start: offset.into(),
                    stop_inclusive: (offset + cache_length).into(),
                };
                gst::debug!(
                    CAT,
                    imp = self,
                    "Reading new chunk requested_length={length} read_head={read_head:?}"
                );
                // TODO: timeout
                let data = provider
                    .get_resource_blocking(url.resource_id, read_head)
                    .recv()
                    .unwrap();

                match data.result {
                    companion::GetResourceResult::None => {
                        gst::element_imp_error!(
                            self,
                            gst::LibraryError::Failed,
                            ["Failed to read resource"]
                        );
                        return Err(gst::FlowError::Error);
                    }
                    companion::GetResourceResult::Success(data) => {
                        active_cache.update(data, offset);
                        if !active_cache.can_read(offset, length as usize) {
                            gst::element_imp_error!(
                                self,
                                gst::ResourceError::Read,
                                ["Read returned less data than requested"]
                            );
                            return Err(gst::FlowError::Error);
                        }
                    }
                }
            }

            let size = {
                let mut map = buffer.map_writable().map_err(|_| {
                    gst::element_imp_error!(
                        self,
                        gst::LibraryError::Failed,
                        ["Failed to map buffer"]
                    );
                    gst::FlowError::Error
                })?;

                let data = active_cache.read(offset, length as usize);
                map[0..data.len()].copy_from_slice(data);

                data.len()
            };

            buffer.set_size(size);

            Ok(gst::FlowSuccess::Ok)
        }
    }

    impl URIHandlerImpl for FCompSrc {
        const URI_TYPE: gst::URIType = gst::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["fcomp"]
        }

        fn uri(&self) -> Option<String> {
            let settings = self.settings.lock();

            settings.url.as_ref().map(Url::to_string)
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            let url = Url::parse(uri).map_err(|err| {
                glib::Error::new(
                    gst::URIError::BadUri,
                    format!("Failed to parse URI({uri}): {err:?}").as_str(),
                )
            })?;
            let comp_url = super::FCompUrl::new(&url).ok_or(glib::Error::new(
                gst::URIError::BadUri,
                "Invalid FCompanion URI",
            ))?;
            let mut settings = self.settings.lock();
            settings.url = Some(url);
            settings.comp_url = Some(comp_url);
            Ok(())
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FCompSrc {
        const NAME: &'static str = "FCompSrc";
        type Type = super::FCompSrc;
        type ParentType = gst_base::BaseSrc;
        type Interfaces = (gst::URIHandler,);
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn test_data_cache_can_read() {
            let cache = DataCache {
                data: vec![0; 1024],
                start: 100,
            };
            assert!(!cache.can_read(1, 1));
            assert!(!cache.can_read(0, 1024 + 512));
            assert!(!cache.can_read(1024, 1100));
            assert!(!cache.can_read(1024, 1024 + 1));
            assert!(cache.can_read(101, 1));
            assert!(cache.can_read(100, 512));
            assert!(cache.can_read(100, 1024));
            assert!(cache.can_read(512, 512));
        }

        #[test]
        fn test_data_cache_read() {
            let cache = DataCache {
                data: vec![0; 1024],
                start: 100,
            };
            assert_eq!(cache.read(100, 1), &[0]);
            assert_eq!(cache.read(100, 10), &[0; 10]);
            assert_eq!(cache.read(100, 1024), &[0; 1024]);
        }
    }
}

glib::wrapper! {
    pub struct FCompSrc(ObjectSubclass<imp::FCompSrc>)
        @extends gst_base::BaseSrc, gst::Element, gst::Object,
        @implements gst::URIHandler;
}

pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "fcompsrc",
        gst::Rank::PRIMARY,
        FCompSrc::static_type(),
    )
}
