use fcast_protocol::companion;
use gst::{glib, prelude::*};
use url::Url;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FCompUrl {
    pub provider_id: companion::ProviderId,
    pub resource_id: companion::ResourceId,
}

impl FCompUrl {
    pub fn new(url: &Url) -> Option<Self> {
        let url::Host::Domain(host) = url.host()? else {
            return None;
        };
        if url.scheme() != "fcomp" {
            return None;
        }
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
    use fcast_protocol::{companion, v4};
    use futures::prelude::*;
    use gst::{glib, prelude::*, subclass::prelude::*};
    use gst_base::subclass::{base_src::CreateSuccess, prelude::*};
    use parking_lot::Mutex;
    use std::{sync::LazyLock, time::Duration};
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

    #[derive(Clone, Debug, glib::Boxed)]
    #[boxed_type(name = "FCompContext")]
    pub struct CompContext(pub crate::fcast::CompanionContext);

    #[derive(Default)]
    enum State {
        #[default]
        Stopped,
        Started {
            size: Option<u64>,
            current_pos: u64,
            stop: Option<u64>,
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
    enum Canceller {
        #[default]
        None,
        Handle(futures::future::AbortHandle),
        Cancelled,
    }

    impl Canceller {
        fn abort(&mut self) {
            if let Canceller::Handle(ref canceller) = *self {
                canceller.abort();
            }

            *self = Canceller::Cancelled;
        }
    }

    #[derive(Default)]
    pub struct FCompSrc {
        state: Mutex<State>,
        settings: Mutex<Settings>,
        context: Mutex<Option<CompContext>>,
        canceller: Mutex<Canceller>,
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

            let mut rx = provider.get_resource_info(url.resource_id).map_err(|err| {
                gst::error_msg!(
                    gst::ResourceError::NotFound,
                    [
                        "Failed to get resource info id={} err={err}",
                        url.resource_id
                    ]
                )
            })?;

            let res = self.wait(async move {
                match rx.recv().await {
                    Some(r) => Ok(r),
                    None => Err(gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to get resource info"]
                    )),
                }
            });

            match res {
                Ok(info) => {
                    let info = info.borrow_dependent();
                    let size = match info.resource_size_type() {
                        fcast_protocol::v4::flat::CompanionResourceSize::Known => {
                            info.resource_size_as_known().map(|s| s.size())
                        }
                        fcast_protocol::v4::flat::CompanionResourceSize::Unknown | _ => None,
                    };

                    *self.state.lock() = State::Started {
                        size,
                        current_pos: 0,
                        stop: None,
                    };

                    Ok(())
                }
                Err(_) => Err(gst::error_msg!(
                    gst::ResourceError::OpenRead,
                    ["Failed to get resource info"]
                )),
            }
        }

        fn wait<F, T>(&self, future: F) -> Result<T, Option<gst::ErrorMessage>>
        where
            F: Send + Future<Output = Result<T, gst::ErrorMessage>>,
            T: Send + 'static,
        {
            let mut canceller = self.canceller.lock();
            if matches!(*canceller, Canceller::Cancelled) {
                return Err(None);
            }
            let (abort_handle, abort_registration) = future::AbortHandle::new_pair();
            *canceller = Canceller::Handle(abort_handle);
            drop(canceller);

            // Wrap in a timeout
            let future = async {
                let res = tokio::time::timeout(Duration::from_millis(2500), future).await;

                match res {
                    Ok(res) => res,
                    Err(_) => Err(gst::error_msg!(
                        gst::ResourceError::Read,
                        ["Request timeout"]
                    )),
                }
            };

            // And make abortable
            let future = async {
                match future::Abortable::new(future, abort_registration).await {
                    Ok(res) => res.map_err(Some),
                    Err(_) => Err(None),
                }
            };

            let res = crate::RUNTIME.block_on(future);

            // Clear out the canceller
            let mut canceller = self.canceller.lock();
            if matches!(*canceller, Canceller::Cancelled) {
                return Err(None);
            }
            *canceller = Canceller::None;

            res
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
                State::Started { size, .. } => size,
                _ => None,
            }
        }

        fn unlock(&self) -> Result<(), gst::ErrorMessage> {
            let mut canceller = self.canceller.lock();
            canceller.abort();
            Ok(())
        }

        fn unlock_stop(&self) -> Result<(), gst::ErrorMessage> {
            let mut canceller = self.canceller.lock();
            *canceller = Canceller::None;
            Ok(())
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

        fn query(&self, query: &mut gst::QueryRef) -> bool {
            use gst::QueryViewMut;

            match query.view_mut() {
                QueryViewMut::Scheduling(q) => {
                    q.set(
                        gst::SchedulingFlags::SEQUENTIAL | gst::SchedulingFlags::BANDWIDTH_LIMITED,
                        1,
                        -1,
                        0,
                    );
                    q.add_scheduling_modes([gst::PadMode::Push]);
                    true
                }
                _ => BaseSrcImplExt::parent_query(self, query),
            }
        }

        fn do_seek(&self, segment: &mut gst::Segment) -> bool {
            let segment = segment.downcast_mut::<gst::format::Bytes>().unwrap();

            let mut state = self.state.lock();

            match &mut (*state) {
                State::Stopped => {
                    gst::element_imp_error!(self, gst::LibraryError::Failed, ["Not started yet"]);
                    return false;
                }
                State::Started {
                    current_pos, stop, ..
                } => {
                    *current_pos = *segment.start().expect("No start position given");
                    *stop = segment.stop().map(|stop| *stop);
                }
            }

            true
        }
    }

    impl PushSrcImpl for FCompSrc {
        fn create(
            &self,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<CreateSuccess, gst::FlowError> {
            let mut state = self.state.lock();
            let State::Started { current_pos, .. } = &mut *state else {
                gst::element_imp_error!(self, gst::CoreError::Failed, ["Not started yet"]);
                return Err(gst::FlowError::Error);
            };

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
            let read_length = companion::MAX_RESOURCE_READ_SIZE as u64;
            let read_head =
                v4::flat::ResourceReadHead::new(*current_pos, *current_pos + read_length - 1);

            gst::debug!(
                CAT,
                imp = self,
                "Reading new chunk requested_length={read_length} read_head={read_head:?}"
            );

            let mut rx = provider
                .get_resource(url.resource_id, Some(read_head))
                .map_err(|err| {
                    gst::element_imp_error!(
                        self,
                        gst::ResourceError::Failed,
                        ["Failed to get resource: {err}"]
                    );
                    gst::FlowError::Error
                })?;

            let res = self.wait(async move {
                match rx.recv().await {
                    Some(c) => Ok(c),
                    None => Err(gst::error_msg!(
                        gst::ResourceError::Read,
                        ["Failed to read"]
                    )),
                }
            });

            match res {
                Ok(res) => match res.result {
                    companion::GetResourceResult::NotFound => {
                        gst::element_imp_error!(
                            self,
                            gst::ResourceError::NotFound,
                            ["Companion resource was not found"]
                        );
                        Err(gst::FlowError::Error)
                    }
                    companion::GetResourceResult::Success(data) => {
                        let size = data.len() as u64;
                        let mut buffer = gst::Buffer::from_slice(data);
                        {
                            let buffer = buffer.get_mut().unwrap();
                            buffer.set_offset(*current_pos);
                            buffer.set_offset_end(*current_pos + size);
                        }

                        *current_pos += size;

                        Ok(CreateSuccess::NewBuffer(buffer))
                    }
                },
                Err(Some(err)) => {
                    gst::debug!(CAT, imp = self, "Error {:?}", err);
                    self.post_error_message(err);
                    Err(gst::FlowError::Error)
                }
                Err(None) => {
                    gst::debug!(CAT, imp = self, "Flushing");
                    Err(gst::FlowError::Flushing)
                }
            }
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
        type ParentType = gst_base::PushSrc;
        type Interfaces = (gst::URIHandler,);
    }
}

glib::wrapper! {
    pub struct FCompSrc(ObjectSubclass<imp::FCompSrc>)
        @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fcast::{
        CompanionContext, CompanionMessage, FeedbackSender, ResourceInfoResponseCell,
    };
    use fcast_protocol::{companion, v4};
    use std::sync::mpsc;
    use url::Url;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
            super::plugin_init().unwrap();
        });
    }

    #[test]
    fn parse_comp_url() {
        let cases = [
            (
                "fcomp://0.fcast/0",
                Some(FCompUrl {
                    provider_id: 0,
                    resource_id: 0,
                }),
            ),
            (
                "fcomp://100.fcast/1337",
                Some(FCompUrl {
                    provider_id: 100,
                    resource_id: 1337,
                }),
            ),
            ("fcomp://fcast/0", None),
            ("fcomp://fcast/", None),
            ("fcomp://0.fcast/", None),
            ("fcomp://10000000000.fcast/0", None),
            ("fcomp://0x123.fcast/0", None),
            ("fcomp://12a3.fcast/0", None),
            ("fcomp://12a3.gcast/0", None),
            ("fcomp://0.gcast/0", None),
            ("fomp://0.fcast/0", None),
            ("fcomp://0.fcast/0/0", None),
        ];

        for (url, result) in cases {
            assert_eq!(FCompUrl::new(&Url::parse(url).unwrap()), result, "{url}",);
        }
    }

    // ---------------------------------------------------------------
    // --- The following harness and tests was generated by claude ---
    // ---------------------------------------------------------------

    #[derive(Debug, Clone)]
    enum Message {
        Buffer(gst::Buffer),
        Event(gst::Event),
        Message(gst::Message),
    }

    fn resource_info_cell(size: Option<u64>) -> ResourceInfoResponseCell {
        let body: Vec<u8> = v4::MessageBuilder::new()
            .companion_resource_info_response(0, "application/octet-stream", size)
            .to_vec();

        ResourceInfoResponseCell::new(body, |buf| {
            v4::flat::root_as_packet(buf)
                .unwrap()
                .payload_as_companion_resource_info_response()
                .unwrap()
        })
    }

    /// Controls how the fake companion provider responds to resource requests.
    enum ProviderBehavior {
        /// Serve the given bytes, chunked according to the read head.
        Data(Vec<u8>),
        /// Report `reported_size` for the resource info, but answer every
        /// `GetResource` with `GetResourceResult::None`.
        AlwaysNone { reported_size: u64 },
    }

    struct Harness {
        src: gst::Element,
        pad: gst::Pad,
        receiver: Option<mpsc::Receiver<Message>>,
        _context: CompanionContext,
    }

    impl Harness {
        fn new(provider_data: Option<Vec<u8>>) -> Harness {
            Self::new_with(provider_data.map(ProviderBehavior::Data))
        }

        fn new_with(behavior: Option<ProviderBehavior>) -> Harness {
            init();

            let src = gst::ElementFactory::make("fcompsrc").build().unwrap();

            let context = CompanionContext::new();
            if let Some(behavior) = behavior {
                let (provider_tx, mut provider_rx) =
                    tokio::sync::mpsc::unbounded_channel::<CompanionMessage>();
                let provider_id = context.register_provider(provider_tx);
                assert_eq!(provider_id, 0);

                std::thread::Builder::new()
                    .name("fake-companion-provider".into())
                    .spawn(move || {
                        while let Some(msg) = provider_rx.blocking_recv() {
                            match msg {
                                CompanionMessage::GetResourceInfo { feedback, .. } => {
                                    let FeedbackSender::Channel(tx) = feedback;
                                    let size = match &behavior {
                                        ProviderBehavior::Data(data) => data.len() as u64,
                                        ProviderBehavior::AlwaysNone { reported_size } => {
                                            *reported_size
                                        }
                                    };
                                    let _ = tx.send(resource_info_cell(Some(size)));
                                }
                                CompanionMessage::GetResource {
                                    read_head,
                                    feedback,
                                    ..
                                } => {
                                    let FeedbackSender::Channel(tx) = feedback;
                                    let result = match &behavior {
                                        ProviderBehavior::Data(data) => {
                                            let start =
                                                read_head.map(|r| r.start()).unwrap_or(0) as usize;
                                            let stop_inclusive = read_head
                                                .map(|r| r.stop_inclusive() as usize)
                                                .unwrap_or(data.len().saturating_sub(1))
                                                .min(data.len().saturating_sub(1));
                                            let chunk = if data.is_empty() || start > stop_inclusive
                                            {
                                                Vec::new()
                                            } else {
                                                data[start..=stop_inclusive].to_vec()
                                            };
                                            companion::GetResourceResult::Success(chunk)
                                        }
                                        ProviderBehavior::AlwaysNone { .. } => {
                                            companion::GetResourceResult::NotFound
                                        }
                                    };

                                    let _ = tx.send(companion::ResourceResponse {
                                        request_id: 0,
                                        part: 0,
                                        total_parts: 1,
                                        result,
                                    });
                                }
                            }
                        }
                    })
                    .unwrap();
            }

            let mut gst_context = gst::Context::new(imp::FCOMP_CONTEXT, true);
            {
                let gst_context = gst_context.get_mut().unwrap();
                gst_context
                    .structure_mut()
                    .set("context", imp::CompContext(context.clone()));
            }
            src.set_context(&gst_context);

            let uri_handler = src.dynamic_cast_ref::<gst::URIHandler>().unwrap();
            uri_handler.set_uri(&companion::create_url(0, 0)).unwrap();

            let (sender, receiver) = mpsc::sync_channel(0);

            let pad = gst::Pad::builder(gst::PadDirection::Sink)
                .name("sink")
                .chain_function({
                    let sender = sender.clone();
                    move |_pad, _parent, buffer| {
                        let _ = sender.send(Message::Buffer(buffer));
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .event_function({
                    let sender = sender.clone();
                    move |_pad, _parent, event| {
                        let _ = sender.send(Message::Event(event));
                        true
                    }
                })
                .build();

            let srcpad = src.static_pad("src").unwrap();
            srcpad.link(&pad).unwrap();

            let bus = gst::Bus::new();
            bus.set_flushing(false);
            src.set_bus(Some(&bus));
            bus.set_sync_handler(move |_bus, msg| {
                let _ = sender.send(Message::Message(msg.clone()));
                gst::BusSyncReply::Drop
            });

            pad.set_active(true).unwrap();

            Harness {
                src,
                pad,
                receiver: Some(receiver),
                _context: context,
            }
        }

        fn run<F: FnOnce(&gst::Element) + Send + 'static>(&self, func: F) {
            self.src.call_async(move |src| func(src));
        }

        fn wait_buffer_or_eos(&mut self) -> Option<gst::Buffer> {
            loop {
                match self.receiver.as_mut().unwrap().recv().unwrap() {
                    Message::Event(ev) => {
                        if let gst::EventView::Eos(_) = ev.view() {
                            return None;
                        }
                    }
                    Message::Message(msg) => {
                        if let gst::MessageView::Error(err) = msg.view() {
                            panic!(
                                "Got error: {} ({})",
                                err.error(),
                                err.debug()
                                    .unwrap_or_else(|| glib::GString::from("UNKNOWN"))
                            );
                        }
                    }
                    Message::Buffer(buffer) => return Some(buffer),
                }
            }
        }

        fn wait_for_error(&mut self) -> glib::Error {
            loop {
                match self.receiver.as_mut().unwrap().recv().unwrap() {
                    Message::Event(ev) => {
                        if let gst::EventView::Eos(_) = ev.view() {
                            panic!("Got EOS but expected error");
                        }
                    }
                    Message::Message(msg) => {
                        if let gst::MessageView::Error(err) = msg.view() {
                            return err.error();
                        }
                    }
                    Message::Buffer(_) => panic!("Got buffer but expected error"),
                }
            }
        }

        fn wait_for_state_change(&mut self) -> gst::State {
            loop {
                match self.receiver.as_mut().unwrap().recv().unwrap() {
                    Message::Event(ev) => {
                        if let gst::EventView::Eos(_) = ev.view() {
                            panic!("Got EOS but expected state change");
                        }
                    }
                    Message::Message(msg) => match msg.view() {
                        gst::MessageView::StateChanged(state) => return state.current(),
                        gst::MessageView::Error(err) => panic!(
                            "Got error: {} ({})",
                            err.error(),
                            err.debug()
                                .unwrap_or_else(|| glib::GString::from("UNKNOWN"))
                        ),
                        _ => (),
                    },
                    Message::Buffer(_) => panic!("Got buffer but expected state change"),
                }
            }
        }

        fn wait_for_segment(
            &mut self,
            allow_buffer: bool,
        ) -> gst::FormattedSegment<gst::format::Bytes> {
            loop {
                match self.receiver.as_mut().unwrap().recv().unwrap() {
                    Message::Event(ev) => {
                        if let gst::EventView::Segment(seg) = ev.view() {
                            return seg
                                .segment()
                                .clone()
                                .downcast::<gst::format::Bytes>()
                                .unwrap();
                        }
                    }
                    Message::Message(msg) => {
                        if let gst::MessageView::Error(err) = msg.view() {
                            panic!(
                                "Got error: {} ({})",
                                err.error(),
                                err.debug()
                                    .unwrap_or_else(|| glib::GString::from("UNKNOWN"))
                            );
                        }
                    }
                    Message::Buffer(_) => {
                        if !allow_buffer {
                            panic!("Got buffer but expected segment");
                        }
                    }
                }
            }
        }

        fn collect_contiguous_from(&mut self, start_offset: u64) -> Vec<u8> {
            let mut out = Vec::new();
            let mut expected_offset = start_offset;
            let mut iterations = 0;
            while let Some(buffer) = self.wait_buffer_or_eos() {
                iterations += 1;
                assert!(
                    iterations < 10_000,
                    "Too many buffers received, EOS likely never arrived"
                );

                assert_eq!(buffer.offset(), expected_offset);
                let map = buffer.map_readable().unwrap();
                expected_offset += map.size() as u64;
                out.extend_from_slice(&map);
            }
            out
        }

        fn collect_until_eos(&mut self) -> Vec<u8> {
            let mut out = Vec::new();
            let mut iterations = 0;
            while let Some(buffer) = self.wait_buffer_or_eos() {
                iterations += 1;
                assert!(
                    iterations < 10_000,
                    "Too many buffers received, EOS likely never arrived"
                );

                let map = buffer.map_readable().unwrap();
                out.extend_from_slice(&map);
            }
            out
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            let bus = self.src.bus().unwrap();
            bus.set_flushing(true);

            self.receiver.take();

            self.pad.set_active(false).unwrap();
            self.src.set_state(gst::State::Null).unwrap();
        }
    }

    #[test]
    fn test_basic_read() {
        let data = b"Hello, companion world!".to_vec();
        let mut h = Harness::new(Some(data.clone()));

        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        assert_eq!(h.collect_until_eos(), data);
    }

    #[test]
    fn test_reports_size_as_duration() {
        let data = vec![0xab; 4096];
        let mut h = Harness::new(Some(data.clone()));

        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        let _first = h
            .wait_buffer_or_eos()
            .expect("expected at least one buffer");
        assert_eq!(
            h.src.query_duration::<gst::format::Bytes>(),
            Some(gst::format::Bytes::from_u64(data.len() as u64))
        );

        while h.wait_buffer_or_eos().is_some() {}
    }

    #[test]
    fn test_large_resource_is_chunked() {
        let len = companion::MAX_RESOURCE_READ_SIZE * 2 + 1234;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();

        let mut h = Harness::new(Some(data.clone()));

        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        assert_eq!(h.collect_until_eos(), data);
    }

    #[test]
    fn test_resource_ending_on_chunk_boundary() {
        let len = companion::MAX_RESOURCE_READ_SIZE * 3;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();

        let mut h = Harness::new(Some(data.clone()));

        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        assert_eq!(h.collect_until_eos(), data);
    }

    #[test]
    fn test_single_chunk_resource() {
        let len = companion::MAX_RESOURCE_READ_SIZE;
        let data: Vec<u8> = (0..len).map(|i| (i % 251) as u8).collect();

        let mut h = Harness::new(Some(data.clone()));

        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        assert_eq!(h.collect_until_eos(), data);
    }

    #[test]
    fn test_missing_provider_errors() {
        let mut h = Harness::new(None);

        h.run(|src| {
            let _ = src.set_state(gst::State::Playing);
        });

        let err = h.wait_for_error();
        assert!(
            err.is::<gst::ResourceError>(),
            "expected a resource error, got {err:?}"
        );
    }

    #[test]
    fn test_provider_returns_none_errors_without_panic() {
        let mut h = Harness::new_with(Some(ProviderBehavior::AlwaysNone {
            reported_size: 4096,
        }));

        h.run(|src| {
            let _ = src.set_state(gst::State::Playing);
        });

        let err = h.wait_for_error();
        assert!(
            err.is::<gst::ResourceError>(),
            "expected a resource error, got {err:?}"
        );
    }

    fn patterned(len: usize) -> Vec<u8> {
        (0..len).map(|i| (i % 256) as u8).collect()
    }

    #[test]
    fn test_seek_after_buffer_received() {
        let data = patterned(8192);
        let mut h = Harness::new(Some(data.clone()));

        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        let buffer = h.wait_buffer_or_eos().unwrap();
        assert_eq!(buffer.offset(), 0);

        let seek_to = 123u64;
        h.run(move |src| {
            src.seek_simple(gst::SeekFlags::FLUSH, gst::format::Bytes::from_u64(seek_to))
                .unwrap();
        });

        let segment = h.wait_for_segment(true);
        assert_eq!(segment.start(), Some(gst::format::Bytes::from_u64(seek_to)));

        let received = h.collect_contiguous_from(seek_to);
        assert_eq!(received, &data[seek_to as usize..]);
    }

    #[test]
    fn test_seek_in_ready_state() {
        let data = patterned(8192);
        let mut h = Harness::new(Some(data.clone()));

        h.run(|src| {
            src.set_state(gst::State::Ready).unwrap();
        });
        assert_eq!(h.wait_for_state_change(), gst::State::Ready);

        let seek_to = 4096u64;
        h.run(move |src| {
            src.seek_simple(gst::SeekFlags::FLUSH, gst::format::Bytes::from_u64(seek_to))
                .unwrap();
            src.set_state(gst::State::Playing).unwrap();
        });

        let segment = h.wait_for_segment(true);
        assert_eq!(segment.start(), Some(gst::format::Bytes::from_u64(seek_to)));

        let received = h.collect_contiguous_from(seek_to);
        assert_eq!(received, &data[seek_to as usize..]);
    }

    #[test]
    fn test_seek_into_later_chunk() {
        let len = companion::MAX_RESOURCE_READ_SIZE * 2 + 1234;
        let data = patterned(len);
        let mut h = Harness::new(Some(data.clone()));

        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        let buffer = h.wait_buffer_or_eos().unwrap();
        assert_eq!(buffer.offset(), 0);

        let seek_to = (companion::MAX_RESOURCE_READ_SIZE + 100) as u64;
        h.run(move |src| {
            src.seek_simple(gst::SeekFlags::FLUSH, gst::format::Bytes::from_u64(seek_to))
                .unwrap();
        });

        let segment = h.wait_for_segment(true);
        assert_eq!(segment.start(), Some(gst::format::Bytes::from_u64(seek_to)));

        let received = h.collect_contiguous_from(seek_to);
        assert_eq!(received, &data[seek_to as usize..]);
    }
}
