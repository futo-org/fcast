// Copyright (C) 2016-2018 Sebastian Dröge <sebastian@centricular.com>
// Copyright (C) 2026      Marcus Hanestad <marcus@futo.org>
//
// Licensed under the Apache License, Version 2.0 <LICENSE-APACHE or
// http://www.apache.org/licenses/LICENSE-2.0> or the MIT license
// <LICENSE-MIT or http://opensource.org/licenses/MIT>, at your
// option. This file may not be copied, modified, or distributed
// except according to those terms.
//
// SPDX-License-Identifier: MIT OR Apache-2.0

mod imp {
    use std::{sync::Arc, time::Duration};

    use futures::{future, prelude::*};
    use parking_lot::Mutex;
    use reqwest::{Client, Response, StatusCode};
    use url::Url;

    use std::sync::LazyLock;

    use gst::{glib, prelude::*, subclass::prelude::*};
    use gst_base::{
        prelude::*,
        subclass::{base_src::CreateSuccess, prelude::*},
    };

    const DEFAULT_LOCATION: Option<Url> = None;
    const DEFAULT_USER_AGENT: &str = "FCast Receiver";
    const DEFAULT_IS_LIVE: bool = false;
    const DEFAULT_TIMEOUT: u32 = 15;
    const DEFAULT_COMPRESS: bool = false;
    const DEFAULT_IRADIO_MODE: bool = true;
    const DEFAULT_KEEP_ALIVE: bool = true;

    #[derive(Debug, Clone)]
    struct Settings {
        location: Option<Url>,
        user_agent: String,
        user_id: Option<String>,
        user_pw: Option<String>,
        timeout: u32,
        compress: bool,
        extra_headers: Option<gst::Structure>,
        cookies: Vec<String>,
        iradio_mode: bool,
        keep_alive: bool,
    }

    impl Default for Settings {
        fn default() -> Self {
            Settings {
                location: DEFAULT_LOCATION,
                user_agent: DEFAULT_USER_AGENT.into(),
                user_id: None,
                user_pw: None,
                timeout: DEFAULT_TIMEOUT,
                compress: DEFAULT_COMPRESS,
                extra_headers: None,
                cookies: Vec::new(),
                iradio_mode: DEFAULT_IRADIO_MODE,
                keep_alive: DEFAULT_KEEP_ALIVE,
            }
        }
    }

    const REQWEST_CLIENT_CONTEXT: &str = "fcast.reqwest.client";

    #[derive(Clone, Debug, glib::Boxed)]
    #[boxed_type(name = "FCastReqwestClientContext")]
    struct ClientContext(Arc<ClientContextInner>);

    #[derive(Debug)]
    struct ClientContextInner {
        client: Client,
    }

    #[allow(clippy::large_enum_variant)]
    #[derive(Debug, Default)]
    enum State {
        #[default]
        Stopped,
        Started {
            uri: Url,
            response: Option<Response>,
            seekable: bool,
            position: u64,
            size: Option<u64>,
            caps: Option<gst::Caps>,
            tags: Option<gst::TagList>,
        },
    }

    #[derive(Default)]
    enum Canceller {
        #[default]
        None,
        Handle(future::AbortHandle),
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
    pub struct FCastHttpSrc {
        client: Mutex<Option<ClientContext>>,
        external_client: Mutex<Option<ClientContext>>,
        settings: Mutex<Settings>,
        state: Mutex<State>,
        canceller: Mutex<Canceller>,
    }

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "fcasthttpsrc",
            gst::DebugColorFlags::empty(),
            Some("FCast HTTP source"),
        )
    });

    impl FCastHttpSrc {
        fn set_location(&self, uri: Option<&str>) -> Result<(), glib::Error> {
            let state = self.state.lock();
            if let State::Started { .. } = *state {
                return Err(glib::Error::new(
                    gst::URIError::BadState,
                    "Changing the `location` property on a started `fcasthttpsrc` is not supported",
                ));
            }

            let mut settings = self.settings.lock();

            if uri.is_none() {
                settings.location = DEFAULT_LOCATION;
                return Ok(());
            }

            let uri = uri.unwrap();
            let uri = Url::parse(uri).map_err(|err| {
                glib::Error::new(
                    gst::URIError::BadUri,
                    format!("Failed to parse URI '{uri}': {err:?}").as_str(),
                )
            })?;

            if uri.scheme() != "http" && uri.scheme() != "https" {
                return Err(glib::Error::new(
                    gst::URIError::UnsupportedProtocol,
                    format!("Unsupported URI scheme '{}'", uri.scheme()).as_str(),
                ));
            }

            settings.location = Some(uri);

            Ok(())
        }

        fn ensure_client(&self) -> Result<ClientContext, gst::ErrorMessage> {
            let mut client_guard = self.client.lock();
            if let Some(ref client) = *client_guard {
                gst::debug!(CAT, imp = self, "Using already configured client");
                return Ok(client.clone());
            }

            // Attempt to acquire an existing client context from another element instance
            let mut q = gst::query::Context::new(REQWEST_CLIENT_CONTEXT);
            if self.obj().src_pad().peer_query(&mut q) {
                if let Some(context) = q.context_owned() {
                    self.obj().set_context(&context);
                }
            } else {
                let _ = self.obj().post_message(
                    gst::message::NeedContext::builder(REQWEST_CLIENT_CONTEXT)
                        .src(&*self.obj())
                        .build(),
                );
            }

            // Hopefully now, self.set_context will have been synchronously called
            if let Some(client) = self.external_client.lock().clone() {
                gst::debug!(CAT, imp = self, "Using shared client");
                *client_guard = Some(client.clone());

                return Ok(client);
            }

            let builder = Client::builder().cookie_store(true).gzip(true);
            gst::debug!(CAT, imp = self, "Creating new client");
            let client = ClientContext(Arc::new(ClientContextInner {
                client: builder.build().map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to create Client: {}", err]
                    )
                })?,
            }));

            // Share created client with other elements.
            gst::debug!(CAT, imp = self, "Sharing new client with other elements");
            let mut context = gst::Context::new(REQWEST_CLIENT_CONTEXT, true);
            {
                let context = context.get_mut().unwrap();
                let s = context.structure_mut();
                s.set("client", &client);
            }
            self.obj().set_context(&context);
            let _ = self.obj().post_message(
                gst::message::HaveContext::builder(context)
                    .src(&*self.obj())
                    .build(),
            );

            *client_guard = Some(client.clone());

            Ok(client)
        }

        fn do_request(
            &self,
            uri: Url,
            start: u64,
            stop: Option<u64>,
        ) -> Result<State, Option<gst::ErrorMessage>> {
            use headers::{
                Connection, ContentLength, ContentRange, HeaderMapExt, Range, UserAgent,
            };
            use reqwest::header::{self, HeaderMap, HeaderName, HeaderValue};

            gst::debug!(CAT, imp = self, "Creating new request for {}", uri);

            let settings = self.settings.lock();

            let req = self.ensure_client()?.0.client.get(uri.clone());

            let mut headers = HeaderMap::new();

            if settings.keep_alive {
                headers.typed_insert(Connection::keep_alive());
            } else {
                headers.typed_insert(Connection::close());
            }

            match (start != 0, stop) {
                (false, None) => (),
                (true, None) => {
                    headers.typed_insert(Range::bytes(start..).unwrap());
                }
                (_, Some(stop)) => {
                    headers.typed_insert(Range::bytes(start..stop).unwrap());
                }
            }

            headers.typed_insert(settings.user_agent.parse::<UserAgent>().unwrap());

            if !settings.compress {
                // Compression is the default
                headers.insert(
                    header::ACCEPT_ENCODING,
                    "identity".parse::<HeaderValue>().unwrap(),
                );
            };

            if let Some(ref extra_headers) = settings.extra_headers {
                for (field, value) in extra_headers.iter() {
                    let field = match HeaderName::try_from(field.as_str()) {
                        Ok(field) => field,
                        Err(err) => {
                            gst::warning!(
                                CAT,
                                imp = self,
                                "Failed to transform extra-header field name '{}' to header name: {}",
                                field,
                                err,
                            );

                            continue;
                        }
                    };

                    let mut append_header = |field: &HeaderName, value: &glib::Value| {
                        let value = match value.transform::<String>() {
                            Ok(value) => value,
                            Err(_) => {
                                gst::warning!(
                                    CAT,
                                    imp = self,
                                    "Failed to transform extra-header '{}' value to string",
                                    field
                                );
                                return;
                            }
                        };

                        let value = value.get::<Option<&str>>().unwrap().unwrap_or("");

                        let value = match value.parse::<HeaderValue>() {
                            Ok(value) => value,
                            Err(_) => {
                                gst::warning!(
                                    CAT,
                                    imp = self,
                                    "Failed to transform extra-header '{}' value to header value",
                                    field
                                );
                                return;
                            }
                        };

                        headers.append(field.clone(), value);
                    };

                    if let Ok(values) = value.get::<gst::ArrayRef>() {
                        for value in values.as_slice() {
                            append_header(&field, value);
                        }
                    } else if let Ok(values) = value.get::<gst::ListRef>() {
                        for value in values.as_slice() {
                            append_header(&field, value);
                        }
                    } else {
                        append_header(&field, value);
                    }
                }
            }

            if !settings.cookies.is_empty() {
                headers.insert(
                    header::COOKIE,
                    settings.cookies.join("; ").parse::<HeaderValue>().unwrap(),
                );
            }

            if settings.iradio_mode {
                headers.insert("icy-metadata", "1".parse().unwrap());
            }

            // Add all headers for the request here
            let req = req.headers(headers);

            let req = if let Some(ref user_id) = settings.user_id {
                // HTTP auth available
                req.basic_auth(user_id, settings.user_pw.clone())
            } else {
                req
            };

            gst::debug!(CAT, imp = self, "Sending new request: {:?}", req);

            drop(settings);

            let future = async {
                req.send().await.map_err(|err| {
                    gst::error_msg!(
                        gst::ResourceError::OpenRead,
                        ["Failed to fetch {}: {:?}", uri, err]
                    )
                })
            };
            let res = self.wait(future);

            let res = match res {
                Ok(res) => res,
                Err(Some(err)) => {
                    gst::debug!(CAT, imp = self, "Error {:?}", err);
                    return Err(Some(err));
                }
                Err(None) => {
                    gst::debug!(CAT, imp = self, "Flushing");
                    return Err(None);
                }
            };

            gst::debug!(CAT, imp = self, "Received response: {:?}", res);

            if !res.status().is_success() {
                match res.status() {
                    StatusCode::NOT_FOUND => {
                        gst::error!(CAT, imp = self, "Resource not found");
                        return Err(Some(gst::error_msg!(
                            gst::ResourceError::NotFound,
                            ["Resource '{}' not found", uri]
                        )));
                    }
                    StatusCode::UNAUTHORIZED
                    | StatusCode::PAYMENT_REQUIRED
                    | StatusCode::FORBIDDEN
                    | StatusCode::PROXY_AUTHENTICATION_REQUIRED => {
                        gst::error!(CAT, imp = self, "Not authorized: {}", res.status());
                        return Err(Some(gst::error_msg!(
                            gst::ResourceError::NotAuthorized,
                            ["Not Authorized for resource '{}': {}", uri, res.status()]
                        )));
                    }
                    _ => {
                        gst::error!(CAT, imp = self, "Request failed: {}", res.status());
                        return Err(Some(gst::error_msg!(
                            gst::ResourceError::OpenRead,
                            ["Request for '{}' failed: {}", uri, res.status()]
                        )));
                    }
                }
            }

            let headers = res.headers();

            let size = headers
                .typed_get::<ContentLength>()
                .map(|ContentLength(cl)| cl + start);

            let accept_byte_ranges = headers
                .get(header::ACCEPT_RANGES)
                .map(|ranges| ranges == "bytes")
                .unwrap_or(false);
            let seekable = size.is_some() && accept_byte_ranges;

            #[allow(clippy::manual_unwrap_or_default)]
            // https://github.com/rust-lang/rust-clippy/issues/12928
            let position = if let Some((range_start, _)) = headers
                .typed_get::<ContentRange>()
                .and_then(|range| range.bytes_range())
            {
                range_start
            } else {
                0
            };

            if position != start {
                return Err(Some(gst::error_msg!(
                    gst::ResourceError::Seek,
                    ["Failed to seek to {}: Got {}", start, position]
                )));
            }

            let mut caps = headers
                .get("icy-metaint")
                .and_then(|s| s.to_str().ok())
                .and_then(|s| s.parse::<i32>().ok())
                .map(|icy_metaint| {
                    gst::Caps::builder("application/x-icy")
                        .field("metadata-interval", icy_metaint)
                        .build()
                });

            if let Some(content_type) = headers
                .get(header::CONTENT_TYPE)
                .and_then(|content_type| content_type.to_str().ok())
                .and_then(|content_type| content_type.parse::<mime::Mime>().ok())
            {
                gst::debug!(CAT, imp = self, "Got content type {}", content_type);
                if let Some(ref mut caps) = caps {
                    let caps = caps.get_mut().unwrap();
                    let s = caps.structure_mut(0).unwrap();
                    s.set("content-type", content_type.as_ref());
                } else if content_type.type_() == "audio" && content_type.subtype() == "L16" {
                    let channels = content_type
                        .get_param("channels")
                        .and_then(|s| s.as_ref().parse::<i32>().ok())
                        .unwrap_or(2);
                    let rate = content_type
                        .get_param("rate")
                        .and_then(|s| s.as_ref().parse::<i32>().ok())
                        .unwrap_or(44_100);

                    caps = Some(
                        gst::Caps::builder("audio/x-unaligned-raw")
                            .field("format", "S16BE")
                            .field("layout", "interleaved")
                            .field("channels", channels)
                            .field("rate", rate)
                            .build(),
                    );
                }
            }

            let mut tags = gst::TagList::new();
            {
                let tags = tags.get_mut().unwrap();

                if let Some(ref icy_name) = headers.get("icy-name").and_then(|s| s.to_str().ok()) {
                    tags.add::<gst::tags::Organization>(icy_name, gst::TagMergeMode::Replace);
                }

                if let Some(ref icy_genre) = headers.get("icy-genre").and_then(|s| s.to_str().ok())
                {
                    tags.add::<gst::tags::Genre>(icy_genre, gst::TagMergeMode::Replace);
                }

                if let Some(ref icy_url) = headers.get("icy-url").and_then(|s| s.to_str().ok()) {
                    tags.add::<gst::tags::Location>(icy_url, gst::TagMergeMode::Replace);
                }
            }

            gst::debug!(CAT, imp = self, "Request successful");

            Ok(State::Started {
                uri,
                response: Some(res),
                seekable,
                position,
                size,
                caps,
                tags: if tags.n_tags() > 0 { Some(tags) } else { None },
            })
        }

        fn wait<F, T>(&self, future: F) -> Result<T, Option<gst::ErrorMessage>>
        where
            F: Send + Future<Output = Result<T, gst::ErrorMessage>>,
            T: Send + 'static,
        {
            let timeout = self.settings.lock().timeout;

            let mut canceller = self.canceller.lock();
            if matches!(*canceller, Canceller::Cancelled) {
                return Err(None);
            }
            let (abort_handle, abort_registration) = future::AbortHandle::new_pair();
            *canceller = Canceller::Handle(abort_handle);
            drop(canceller);

            // Wrap in a timeout
            let future = async {
                if timeout == 0 {
                    future.await
                } else {
                    let res =
                        tokio::time::timeout(Duration::from_secs(timeout.into()), future).await;

                    match res {
                        Ok(res) => res,
                        Err(_) => Err(gst::error_msg!(
                            gst::ResourceError::Read,
                            ["Request timeout"]
                        )),
                    }
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

            /* Clear out the canceller */
            let mut canceller = self.canceller.lock();
            if matches!(*canceller, Canceller::Cancelled) {
                return Err(None);
            }
            *canceller = Canceller::None;

            res
        }
    }

    impl ObjectImpl for FCastHttpSrc {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecString::builder("location")
                        .nick("Location")
                        .blurb("URL to read from")
                        .readwrite()
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecString::builder("user-agent")
                        .nick("User-Agent")
                        .blurb("Value of the User-Agent HTTP request header field")
                        .default_value(DEFAULT_USER_AGENT)
                        .readwrite()
                        .readwrite()
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecBoolean::builder("is-live")
                        .nick("Is Live")
                        .blurb("Act like a live source")
                        .default_value(DEFAULT_IS_LIVE)
                        .readwrite()
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecString::builder("user-id")
                        .nick("User-id")
                        .blurb("HTTP location URI user id for authentication")
                        .readwrite()
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecString::builder("user-pw")
                        .nick("User-pw")
                        .blurb("HTTP location URI user password for authentication")
                        .readwrite()
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecUInt::builder("timeout")
                        .nick("Timeout")
                        .blurb("Value in seconds to timeout a blocking I/O (0 = No timeout).")
                        .maximum(3600)
                        .default_value(DEFAULT_TIMEOUT)
                        .readwrite()
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecBoolean::builder("compress")
                        .nick("Compress")
                        .blurb("Allow compressed content encodings")
                        .default_value(DEFAULT_COMPRESS)
                        .readwrite()
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecBoxed::builder::<gst::Structure>("extra-headers")
                        .nick("Extra Headers")
                        .blurb("Extra headers to append to the HTTP request")
                        .readwrite()
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecBoxed::builder::<Vec<String>>("cookies")
                        .nick("Cookies")
                        .nick("HTTP request cookies")
                        .readwrite()
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecBoolean::builder("iradio-mode")
                        .nick("I-Radio Mode")
                        .blurb("Enable internet radio mode (ask server to send shoutcast/icecast metadata interleaved with the actual stream data")
                        .default_value(DEFAULT_IRADIO_MODE)
                        .readwrite()
                        .mutable_ready()
                        .build(),
                    glib::ParamSpecBoolean::builder("keep-alive")
                        .nick("Keep Alive")
                        .blurb("Use HTTP persistent connections")
                        .default_value(DEFAULT_KEEP_ALIVE)
                        .readwrite()
                        .mutable_ready()
                        .build(),
                ]
            });

            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            let res = match pspec.name() {
                "location" => {
                    let location = value.get::<Option<&str>>().expect("type checked upstream");
                    self.set_location(location)
                }
                "user-agent" => {
                    let mut settings = self.settings.lock();
                    let user_agent = value
                        .get::<Option<String>>()
                        .expect("type checked upstream")
                        .unwrap_or_else(|| DEFAULT_USER_AGENT.into());
                    settings.user_agent = user_agent;
                    Ok(())
                }
                "is-live" => {
                    let is_live = value.get().expect("type checked upstream");
                    self.obj().set_live(is_live);
                    Ok(())
                }
                "user-id" => {
                    let mut settings = self.settings.lock();
                    let user_id = value.get().expect("type checked upstream");
                    settings.user_id = user_id;
                    Ok(())
                }
                "user-pw" => {
                    let mut settings = self.settings.lock();
                    let user_pw = value.get().expect("type checked upstream");
                    settings.user_pw = user_pw;
                    Ok(())
                }
                "timeout" => {
                    let mut settings = self.settings.lock();
                    let timeout = value.get().expect("type checked upstream");
                    settings.timeout = timeout;
                    Ok(())
                }
                "compress" => {
                    let mut settings = self.settings.lock();
                    let compress = value.get().expect("type checked upstream");
                    settings.compress = compress;
                    Ok(())
                }
                "extra-headers" => {
                    let mut settings = self.settings.lock();
                    let extra_headers = value.get().expect("type checked upstream");
                    settings.extra_headers = extra_headers;
                    Ok(())
                }
                "cookies" => {
                    let mut settings = self.settings.lock();
                    settings.cookies = value.get::<Vec<String>>().expect("type checked upstream");
                    Ok(())
                }
                "iradio-mode" => {
                    let mut settings = self.settings.lock();
                    let iradio_mode = value.get().expect("type checked upstream");
                    settings.iradio_mode = iradio_mode;
                    Ok(())
                }
                "keep-alive" => {
                    let mut settings = self.settings.lock();
                    let keep_alive = value.get().expect("type checked upstream");
                    settings.keep_alive = keep_alive;
                    Ok(())
                }
                _ => unimplemented!(),
            };

            if let Err(err) = res {
                gst::error!(
                    CAT,
                    imp = self,
                    "Failed to set property `{}`: {:?}",
                    pspec.name(),
                    err
                );
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "location" => {
                    let settings = self.settings.lock();
                    let location = settings.location.as_ref().map(Url::to_string);

                    location.to_value()
                }
                "user-agent" => {
                    let settings = self.settings.lock();
                    settings.user_agent.to_value()
                }
                "is-live" => self.obj().is_live().to_value(),
                "user-id" => {
                    let settings = self.settings.lock();
                    settings.user_id.to_value()
                }
                "user-pw" => {
                    let settings = self.settings.lock();
                    settings.user_pw.to_value()
                }
                "timeout" => {
                    let settings = self.settings.lock();
                    settings.timeout.to_value()
                }
                "compress" => {
                    let settings = self.settings.lock();
                    settings.compress.to_value()
                }
                "extra-headers" => {
                    let settings = self.settings.lock();
                    settings.extra_headers.to_value()
                }
                "cookies" => {
                    let settings = self.settings.lock();
                    settings.cookies.to_value()
                }
                "iradio-mode" => {
                    let settings = self.settings.lock();
                    settings.iradio_mode.to_value()
                }
                "keep-alive" => {
                    let settings = self.settings.lock();
                    settings.keep_alive.to_value()
                }
                _ => unimplemented!(),
            }
        }

        fn constructed(&self) {
            self.parent_constructed();

            let obj = self.obj();
            obj.set_automatic_eos(false);
            obj.set_format(gst::Format::Bytes);
        }
    }

    impl GstObjectImpl for FCastHttpSrc {}

    impl ElementImpl for FCastHttpSrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static ELEMENT_METADATA: LazyLock<gst::subclass::ElementMetadata> =
                LazyLock::new(|| {
                    gst::subclass::ElementMetadata::new(
                        "HTTP Source",
                        "Source/Network/HTTP",
                        "Read stream from an HTTP/HTTPS location",
                        "Sebastian Dröge <sebastian@centricular.com>",
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
            if context.context_type() == REQWEST_CLIENT_CONTEXT {
                let mut external_client = self.external_client.lock();
                let s = context.structure();
                *external_client = s
                    .get::<&ClientContext>("client")
                    .map(|c| Some(c.clone()))
                    .unwrap_or(None);
            }

            self.parent_set_context(context);
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            if let gst::StateChange::ReadyToNull = transition {
                *self.client.lock() = None;
            }

            self.parent_change_state(transition)
        }
    }

    impl BaseSrcImpl for FCastHttpSrc {
        fn is_seekable(&self) -> bool {
            let state = self.state.lock();
            match *state {
                State::Started { seekable, .. } => seekable,
                _ => false,
            }
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
            let mut state = self.state.lock();

            *state = State::Stopped;

            let uri = self
                .settings
                .lock()
                .location
                .as_ref()
                .ok_or_else(|| {
                    gst::error_msg!(gst::CoreError::StateChange, ["Can't start without an URI"])
                })
                .cloned()?;

            gst::debug!(CAT, imp = self, "Starting for URI {}", uri);

            *state = self.do_request(uri, 0, None).map_err(|err| {
                err.unwrap_or_else(|| {
                    gst::error_msg!(gst::LibraryError::Failed, ["Interrupted during start"])
                })
            })?;

            Ok(())
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

            let uri = match *state {
                State::Started { ref uri, .. } => uri.clone(),
                State::Stopped => {
                    gst::element_imp_error!(self, gst::LibraryError::Failed, ["Not started yet"]);

                    return false;
                }
            };

            let start = *segment.start().expect("No start position given");
            let stop = segment.stop().map(|stop| *stop);

            gst::debug!(CAT, imp = self, "Seeking to {}-{:?}", start, stop);

            *state = State::Stopped;
            match self.do_request(uri, start, stop) {
                Ok(s) => {
                    *state = s;
                    true
                }
                Err(Some(err)) => {
                    self.post_error_message(err);
                    false
                }
                Err(None) => false,
            }
        }
    }

    impl PushSrcImpl for FCastHttpSrc {
        fn create(
            &self,
            _buffer: Option<&mut gst::BufferRef>,
        ) -> Result<CreateSuccess, gst::FlowError> {
            let mut state = self.state.lock();

            let (response, position, caps, tags) = match *state {
                State::Started {
                    ref mut response,
                    ref mut position,
                    ref mut tags,
                    ref mut caps,
                    ..
                } => (response, position, caps, tags),
                State::Stopped => {
                    gst::element_imp_error!(self, gst::LibraryError::Failed, ["Not started yet"]);

                    return Err(gst::FlowError::Error);
                }
            };

            let offset = *position;

            let mut current_response = match response.take() {
                Some(response) => response,
                None => {
                    gst::error!(CAT, imp = self, "Don't have a response");
                    gst::element_imp_error!(
                        self,
                        gst::ResourceError::Read,
                        ["Don't have a response"]
                    );

                    return Err(gst::FlowError::Error);
                }
            };

            let tags = tags.take();
            let caps = caps.take();
            drop(state);

            if let Some(caps) = caps {
                gst::debug!(CAT, imp = self, "Setting caps {:?}", caps);
                self.obj()
                    .set_caps(&caps)
                    .map_err(|_| gst::FlowError::NotNegotiated)?;
            }

            if let Some(tags) = tags {
                gst::debug!(CAT, imp = self, "Sending iradio tags {:?}", tags);
                self.obj().src_pad().push_event(gst::event::Tag::new(tags));
            }

            let future = async {
                current_response.chunk().await.map_err(move |err| {
                    gst::error_msg!(
                        gst::ResourceError::Read,
                        ["Failed to read chunk at offset {}: {:?}", offset, err]
                    )
                })
            };
            let res = self.wait(future);

            let res = match res {
                Ok(res) => res,
                Err(Some(err)) => {
                    gst::debug!(CAT, imp = self, "Error {:?}", err);
                    self.post_error_message(err);
                    return Err(gst::FlowError::Error);
                }
                Err(None) => {
                    gst::debug!(CAT, imp = self, "Flushing");
                    return Err(gst::FlowError::Flushing);
                }
            };

            let mut state = self.state.lock();
            let (response, position) = match *state {
                State::Started {
                    ref mut response,
                    ref mut position,
                    ..
                } => (response, position),
                State::Stopped => {
                    gst::element_imp_error!(self, gst::LibraryError::Failed, ["Not started yet"]);

                    return Err(gst::FlowError::Error);
                }
            };

            if let Some(chunk) = res {
                if !chunk.is_empty() {
                    gst::trace!(
                        CAT,
                        imp = self,
                        "Chunk of {} bytes received at offset {}",
                        chunk.len(),
                        offset
                    );
                    let size = chunk.len();

                    *position += size as u64;

                    let mut buffer = gst::Buffer::from_slice(chunk);

                    *response = Some(current_response);

                    {
                        let buffer = buffer.get_mut().unwrap();
                        buffer.set_offset(offset);
                        buffer.set_offset_end(offset + size as u64);
                    }

                    return Ok(CreateSuccess::NewBuffer(buffer));
                }
            }

            /* No further data, end of stream */
            gst::debug!(CAT, imp = self, "End of stream");
            *response = Some(current_response);
            Err(gst::FlowError::Eos)
        }
    }

    impl URIHandlerImpl for FCastHttpSrc {
        const URI_TYPE: gst::URIType = gst::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["http", "https"]
        }

        fn uri(&self) -> Option<String> {
            let settings = self.settings.lock();

            settings.location.as_ref().map(Url::to_string)
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            self.set_location(Some(uri))
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FCastHttpSrc {
        const NAME: &'static str = "FCastHttpSrc";
        type Type = super::FCastHttpSrc;
        type ParentType = gst_base::PushSrc;
        type Interfaces = (gst::URIHandler,);
    }
}

use gst::{glib, prelude::*};

glib::wrapper! {
    pub struct FCastHttpSrc(ObjectSubclass<imp::FCastHttpSrc>)
        @extends gst_base::PushSrc, gst_base::BaseSrc, gst::Element, gst::Object,
        @implements gst::URIHandler;
}

pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "fcasthttpsrc",
        gst::Rank::PRIMARY + 1,
        FCastHttpSrc::static_type(),
    )
}

#[cfg(test)]
mod tests {
    #![allow(clippy::single_match)]

    use gst::{glib, prelude::*};
    use http_body_util::combinators::BoxBody;

    use std::sync::mpsc;

    fn init() {
        use std::sync::Once;
        static INIT: Once = Once::new();

        INIT.call_once(|| {
            gst::init().unwrap();
            super::plugin_init().unwrap();
        });
    }

    /// Our custom test harness around the HTTP source
    #[derive(Debug)]
    struct Harness {
        src: gst::Element,
        pad: gst::Pad,
        receiver: Option<mpsc::Receiver<Message>>,
        _rt: tokio::runtime::Runtime,
    }

    /// Messages sent from our test harness
    #[allow(clippy::enum_variant_names)]
    #[derive(Debug, Clone)]
    enum Message {
        Buffer(gst::Buffer),
        Event(gst::Event),
        Message(gst::Message),
        ServerError(String),
    }

    fn full_body(s: impl Into<bytes::Bytes>) -> BoxBody<bytes::Bytes, hyper::Error> {
        use http_body_util::{BodyExt, Full};
        Full::new(s.into()).map_err(|never| match never {}).boxed()
    }

    fn empty_body() -> BoxBody<bytes::Bytes, hyper::Error> {
        use http_body_util::{BodyExt, Empty};
        Empty::new().map_err(|never| match never {}).boxed()
    }

    impl Harness {
        /// Creates a new HTTP source and test harness around it
        ///
        /// `http_func`: Function to generate HTTP responses based on a request
        /// `setup_func`: Setup function for the HTTP source, should only set properties and similar
        fn new<
            F: FnMut(
                    hyper::Request<hyper::body::Incoming>,
                ) -> hyper::Response<BoxBody<bytes::Bytes, hyper::Error>>
                + Send
                + 'static,
            G: FnOnce(&gst::Element),
        >(
            http_func: F,
            setup_func: G,
        ) -> Harness {
            use hyper::{server::conn::http1, service::service_fn};
            use std::sync::{Arc, Mutex};

            // Create the HTTP source
            let src = gst::ElementFactory::make("fcasthttpsrc").build().unwrap();

            // Sender/receiver for the messages we generate from various places for the tests
            //
            // Sending to this sender will block until the corresponding item was received from the
            // receiver, which allows us to handle everything as if it is running in a single thread
            let (sender, receiver) = mpsc::sync_channel(0);

            // Sink pad that receives everything the source is generating
            let pad = gst::Pad::builder(gst::PadDirection::Sink)
                .name("sink")
                .chain_function({
                    let sender_clone = sender.clone();
                    move |_pad, _parent, buffer| {
                        let _ = sender_clone.send(Message::Buffer(buffer));
                        Ok(gst::FlowSuccess::Ok)
                    }
                })
                .event_function({
                    let sender_clone = sender.clone();
                    move |_pad, _parent, event| {
                        let _ = sender_clone.send(Message::Event(event));
                        true
                    }
                })
                .build();

            let srcpad = src.static_pad("src").unwrap();
            srcpad.link(&pad).unwrap();

            let bus = gst::Bus::new();
            bus.set_flushing(false);
            src.set_bus(Some(&bus));
            let sender_clone = sender.clone();
            bus.set_sync_handler(move |_bus, msg| {
                let _ = sender_clone.send(Message::Message(msg.clone()));
                gst::BusSyncReply::Drop
            });

            // Activate the pad so that it can be used now
            pad.set_active(true).unwrap();

            // Create the tokio runtime used for the HTTP server in this test
            let rt = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()
                .unwrap();

            // Create an HTTP sever that listens on localhost on some random, free port
            let addr = std::net::SocketAddr::from(([127, 0, 0, 1], 0));

            // Whenever a new client is connecting, a new service function is requested. For each
            // client we use the same service function, which simply calls the function used by the
            // test
            let http_func = Arc::new(Mutex::new(http_func));
            let service = service_fn(move |req: hyper::Request<hyper::body::Incoming>| {
                let http_func = http_func.clone();
                async move { Ok::<_, hyper::Error>((*http_func.lock().unwrap())(req)) }
            });

            let (local_addr_sender, local_addr_receiver) = tokio::sync::oneshot::channel();

            // Spawn the server in the background so that it can handle requests
            rt.spawn(async move {
                // Bind the server, retrieve the local port that was selected in the end and set this as
                // the location property on the source
                let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
                let local_addr = listener.local_addr().unwrap();

                local_addr_sender.send(local_addr).unwrap();

                loop {
                    let (stream, _) = listener.accept().await.unwrap();
                    let io = tokio_io::TokioIo::new(stream);
                    let service = service.clone();
                    let sender = sender.clone();
                    tokio::task::spawn(async move {
                        let http = http1::Builder::new().serve_connection(io, service);
                        if let Err(e) = http.await {
                            let _ = sender.send(Message::ServerError(format!("{e}")));
                        }
                    });
                }
            });

            let local_addr = rt.block_on(local_addr_receiver).unwrap();
            src.set_property("location", format!("http://{local_addr}/"));

            // Let the test setup anything needed on the HTTP source now
            setup_func(&src);

            Harness {
                src,
                pad,
                receiver: Some(receiver),
                _rt: rt,
            }
        }

        fn wait_for_error(&mut self) -> glib::Error {
            loop {
                match self.receiver.as_mut().unwrap().recv().unwrap() {
                    Message::ServerError(err) => {
                        panic!("Got server error: {err}");
                    }
                    Message::Event(ev) => {
                        use gst::EventView;

                        match ev.view() {
                            EventView::Eos(_) => {
                                panic!("Got EOS but expected error");
                            }
                            _ => (),
                        }
                    }
                    Message::Message(msg) => {
                        use gst::MessageView;

                        match msg.view() {
                            MessageView::Error(err) => {
                                return err.error();
                            }
                            _ => (),
                        }
                    }
                    Message::Buffer(_buffer) => {
                        panic!("Got buffer but expected error");
                    }
                }
            }
        }

        fn wait_for_state_change(&mut self) -> gst::State {
            loop {
                match self.receiver.as_mut().unwrap().recv().unwrap() {
                    Message::ServerError(err) => {
                        panic!("Got server error: {err}");
                    }
                    Message::Event(ev) => {
                        use gst::EventView;

                        match ev.view() {
                            EventView::Eos(_) => {
                                panic!("Got EOS but expected state change");
                            }
                            _ => (),
                        }
                    }
                    Message::Message(msg) => {
                        use gst::MessageView;

                        match msg.view() {
                            MessageView::StateChanged(state) => {
                                return state.current();
                            }
                            MessageView::Error(err) => {
                                panic!(
                                    "Got error: {} ({})",
                                    err.error(),
                                    err.debug()
                                        .unwrap_or_else(|| glib::GString::from("UNKNOWN"))
                                );
                            }
                            _ => (),
                        }
                    }
                    Message::Buffer(_buffer) => {
                        panic!("Got buffer but expected state change");
                    }
                }
            }
        }

        fn wait_for_segment(
            &mut self,
            allow_buffer: bool,
            mut allow_server_error: bool,
        ) -> gst::FormattedSegment<gst::format::Bytes> {
            loop {
                match self.receiver.as_mut().unwrap().recv().unwrap() {
                    Message::ServerError(err) => {
                        if allow_server_error {
                            allow_server_error = false;
                        } else {
                            panic!("Got server error: {err}");
                        }
                    }
                    Message::Event(ev) => {
                        use gst::EventView;

                        match ev.view() {
                            EventView::Segment(seg) => {
                                return seg
                                    .segment()
                                    .clone()
                                    .downcast::<gst::format::Bytes>()
                                    .unwrap();
                            }
                            _ => (),
                        }
                    }
                    Message::Message(msg) => {
                        use gst::MessageView;

                        match msg.view() {
                            MessageView::Error(err) => {
                                panic!(
                                    "Got error: {} ({})",
                                    err.error(),
                                    err.debug()
                                        .unwrap_or_else(|| glib::GString::from("UNKNOWN"))
                                );
                            }
                            _ => (),
                        }
                    }
                    Message::Buffer(_buffer) => {
                        if !allow_buffer {
                            panic!("Got buffer but expected segment");
                        }
                    }
                }
            }
        }

        /// Wait until a buffer is available or EOS was reached
        ///
        /// This function will panic on errors.
        fn wait_buffer_or_eos(&mut self) -> Option<gst::Buffer> {
            loop {
                match self.receiver.as_mut().unwrap().recv().unwrap() {
                    Message::ServerError(err) => {
                        panic!("Got server error: {err}");
                    }
                    Message::Event(ev) => {
                        use gst::EventView;

                        match ev.view() {
                            EventView::Eos(_) => return None,
                            _ => (),
                        }
                    }
                    Message::Message(msg) => {
                        use gst::MessageView;

                        match msg.view() {
                            MessageView::Error(err) => {
                                panic!(
                                    "Got error: {} ({})",
                                    err.error(),
                                    err.debug()
                                        .unwrap_or_else(|| glib::GString::from("UNKNOWN"))
                                );
                            }
                            _ => (),
                        }
                    }
                    Message::Buffer(buffer) => return Some(buffer),
                }
            }
        }

        /// Run some code asynchronously on another thread with the HTTP source
        fn run<F: FnOnce(&gst::Element) + Send + 'static>(&self, func: F) {
            self.src.call_async(move |src| func(src));
        }
    }

    impl Drop for Harness {
        fn drop(&mut self) {
            // Shut down everything that was set up for this test harness
            // and wait until the tokio runtime exited
            let bus = self.src.bus().unwrap();
            bus.set_flushing(true);

            // Drop the receiver first before setting the state so that
            // any threads that might still be blocked on the sender
            // are immediately unblocked
            self.receiver.take().unwrap();

            self.pad.set_active(false).unwrap();
            self.src.set_state(gst::State::Null).unwrap();
        }
    }

    #[test]
    fn test_basic_request() {
        use std::io::{Cursor, Read};

        init();

        // Set up a harness that returns "Hello World" for any HTTP request and checks if the
        // default headers are all sent
        let mut h = Harness::new(
            |req| {
                let headers = req.headers();
                assert_eq!(headers.get("connection").unwrap(), "keep-alive");
                assert_eq!(headers.get("accept-encoding").unwrap(), "identity");
                assert_eq!(headers.get("icy-metadata").unwrap(), "1");

                hyper::Response::new(full_body("Hello World"))
            },
            |_src| {
                // No additional setup needed here
            },
        );

        // Set the HTTP source to Playing so that everything can start
        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        // And now check if the data we receive is exactly what we expect it to be
        let expected_output = "Hello World";
        let mut cursor = Cursor::new(expected_output);

        while let Some(buffer) = h.wait_buffer_or_eos() {
            // On the first buffer also check if the duration reported by the HTTP source is what we
            // would expect it to be
            if cursor.position() == 0 {
                assert_eq!(
                    h.src.query_duration::<gst::format::Bytes>(),
                    Some(gst::format::Bytes::from_usize(expected_output.len()))
                );
            }

            // Map the buffer readable and check if it contains exactly the data we would expect at
            // this point after reading everything else we read in previous runs
            let map = buffer.map_readable().unwrap();
            let mut read_buf = vec![0; map.size()];
            assert_eq!(cursor.read(&mut read_buf).unwrap(), map.size());
            assert_eq!(&*map, &*read_buf);
        }

        // Check if everything was read
        assert_eq!(cursor.position(), 11);
    }

    #[test]
    fn test_basic_request_inverted_defaults() {
        use std::io::{Cursor, Read};

        init();

        // Set up a harness that returns "Hello World" for any HTTP request and override various
        // default properties to check if the corresponding headers are set correctly
        let mut h = Harness::new(
            |req| {
                let headers = req.headers();
                assert_eq!(headers.get("connection").unwrap(), "close");
                assert_eq!(headers.get("accept-encoding").unwrap(), "gzip");
                assert_eq!(headers.get("icy-metadata"), None);
                assert_eq!(headers.get("user-agent").unwrap(), "test user-agent");

                hyper::Response::new(full_body("Hello World"))
            },
            |src| {
                src.set_property("keep-alive", false);
                src.set_property("compress", true);
                src.set_property("iradio-mode", false);
                src.set_property("user-agent", "test user-agent");
            },
        );

        // Set the HTTP source to Playing so that everything can start
        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        // And now check if the data we receive is exactly what we expect it to be
        let expected_output = "Hello World";
        let mut cursor = Cursor::new(expected_output);

        while let Some(buffer) = h.wait_buffer_or_eos() {
            // On the first buffer also check if the duration reported by the HTTP source is what we
            // would expect it to be
            if cursor.position() == 0 {
                assert_eq!(
                    h.src.query_duration::<gst::format::Bytes>(),
                    Some(gst::format::Bytes::from_usize(expected_output.len()))
                );
            }

            // Map the buffer readable and check if it contains exactly the data we would expect at
            // this point after reading everything else we read in previous runs
            let map = buffer.map_readable().unwrap();
            let mut read_buf = vec![0; map.size()];
            assert_eq!(cursor.read(&mut read_buf).unwrap(), map.size());
            assert_eq!(&*map, &*read_buf);
        }

        // Check if everything was read
        assert_eq!(cursor.position(), 11);
    }

    #[test]
    fn test_extra_headers() {
        use std::io::{Cursor, Read};

        init();

        // Set up a harness that returns "Hello World" for any HTTP request and check if the
        // extra-headers property works correctly for setting additional headers
        let mut h = Harness::new(
            |req| {
                let headers = req.headers();
                assert_eq!(headers.get("foo").unwrap(), "bar");
                assert_eq!(headers.get("baz").unwrap(), "1");
                assert_eq!(
                    headers
                        .get_all("list")
                        .iter()
                        .map(|v| v.to_str().unwrap())
                        .collect::<Vec<&str>>(),
                    vec!["1", "2"]
                );
                assert_eq!(
                    headers
                        .get_all("array")
                        .iter()
                        .map(|v| v.to_str().unwrap())
                        .collect::<Vec<&str>>(),
                    vec!["1", "2"]
                );

                hyper::Response::new(full_body("Hello World"))
            },
            |src| {
                src.set_property(
                    "extra-headers",
                    gst::Structure::builder("headers")
                        .field("foo", "bar")
                        .field("baz", 1i32)
                        .field("list", gst::List::new([1i32, 2i32]))
                        .field("array", gst::Array::new([1i32, 2i32]))
                        .build(),
                );
            },
        );

        // Set the HTTP source to Playing so that everything can start
        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        // And now check if the data we receive is exactly what we expect it to be
        let expected_output = "Hello World";
        let mut cursor = Cursor::new(expected_output);

        while let Some(buffer) = h.wait_buffer_or_eos() {
            // On the first buffer also check if the duration reported by the HTTP source is what we
            // would expect it to be
            if cursor.position() == 0 {
                assert_eq!(
                    h.src.query_duration::<gst::format::Bytes>(),
                    Some(gst::format::Bytes::from_usize(expected_output.len()))
                );
            }

            // Map the buffer readable and check if it contains exactly the data we would expect at
            // this point after reading everything else we read in previous runs
            let map = buffer.map_readable().unwrap();
            let mut read_buf = vec![0; map.size()];
            assert_eq!(cursor.read(&mut read_buf).unwrap(), map.size());
            assert_eq!(&*map, &*read_buf);
        }

        // Check if everything was read
        assert_eq!(cursor.position(), 11);
    }

    #[test]
    fn test_cookies_property() {
        use std::io::{Cursor, Read};

        init();

        // Set up a harness that returns "Hello World" for any HTTP request and check if the
        // cookies property can be used to set cookies correctly
        let mut h = Harness::new(
            |req| {
                let headers = req.headers();
                assert_eq!(headers.get("cookie").unwrap(), "foo=1; bar=2; baz=3");

                hyper::Response::new(full_body("Hello World"))
            },
            |src| {
                src.set_property(
                    "cookies",
                    vec![
                        String::from("foo=1"),
                        String::from("bar=2"),
                        String::from("baz=3"),
                    ],
                );
            },
        );

        // Set the HTTP source to Playing so that everything can start
        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        // And now check if the data we receive is exactly what we expect it to be
        let expected_output = "Hello World";
        let mut cursor = Cursor::new(expected_output);

        while let Some(buffer) = h.wait_buffer_or_eos() {
            // On the first buffer also check if the duration reported by the HTTP source is what we
            // would expect it to be
            if cursor.position() == 0 {
                assert_eq!(
                    h.src.query_duration::<gst::format::Bytes>(),
                    Some(gst::format::Bytes::from_usize(expected_output.len()))
                );
            }

            // Map the buffer readable and check if it contains exactly the data we would expect at
            // this point after reading everything else we read in previous runs
            let map = buffer.map_readable().unwrap();
            let mut read_buf = vec![0; map.size()];
            assert_eq!(cursor.read(&mut read_buf).unwrap(), map.size());
            assert_eq!(&*map, &*read_buf);
        }

        // Check if everything was read
        assert_eq!(cursor.position(), 11);
    }

    #[test]
    fn test_iradio_mode() {
        use std::io::{Cursor, Read};

        init();

        // Set up a harness that returns "Hello World" for any HTTP request and check if the
        // iradio-mode property works correctly, and especially the icy- headers are parsed correctly
        // and put into caps/tags
        let mut h = Harness::new(
            |req| {
                let headers = req.headers();
                assert_eq!(headers.get("icy-metadata").unwrap(), "1");

                hyper::Response::builder()
                    .header("icy-metaint", "8192")
                    .header("icy-name", "Name")
                    .header("icy-genre", "Genre")
                    .header("icy-url", "http://www.example.com")
                    .header("Content-Type", "audio/mpeg; rate=44100")
                    .body(full_body("Hello World"))
                    .unwrap()
            },
            |_src| {
                // No additional setup needed here
            },
        );

        // Set the HTTP source to Playing so that everything can start
        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        // And now check if the data we receive is exactly what we expect it to be
        let expected_output = "Hello World";
        let mut cursor = Cursor::new(expected_output);

        while let Some(buffer) = h.wait_buffer_or_eos() {
            // On the first buffer also check if the duration reported by the HTTP source is what we
            // would expect it to be
            if cursor.position() == 0 {
                assert_eq!(
                    h.src.query_duration::<gst::format::Bytes>(),
                    Some(gst::format::Bytes::from_usize(expected_output.len()))
                );
            }

            // Map the buffer readable and check if it contains exactly the data we would expect at
            // this point after reading everything else we read in previous runs
            let map = buffer.map_readable().unwrap();
            let mut read_buf = vec![0; map.size()];
            assert_eq!(cursor.read(&mut read_buf).unwrap(), map.size());
            assert_eq!(&*map, &*read_buf);
        }

        // Check if everything was read
        assert_eq!(cursor.position(), 11);

        let srcpad = h.src.static_pad("src").unwrap();
        let caps = srcpad.current_caps().unwrap();
        assert_eq!(
            caps,
            gst::Caps::builder("application/x-icy")
                .field("metadata-interval", 8192i32)
                .field("content-type", "audio/mpeg; rate=44100")
                .build()
        );

        {
            match srcpad.sticky_event::<gst::event::Tag>(0) {
                Some(tag_event) => {
                    let tags = tag_event.tag();
                    assert_eq!(tags.get::<gst::tags::Organization>().unwrap().get(), "Name");
                    assert_eq!(tags.get::<gst::tags::Genre>().unwrap().get(), "Genre");
                    assert_eq!(
                        tags.get::<gst::tags::Location>().unwrap().get(),
                        "http://www.example.com",
                    );
                }
                _ => {
                    unreachable!();
                }
            }
        }
    }

    #[test]
    fn test_audio_l16() {
        use std::io::{Cursor, Read};

        init();

        // Set up a harness that returns "Hello World" for any HTTP request and check if the
        // audio/L16 content type is parsed correctly and put into the caps
        let mut h = Harness::new(
            |_req| {
                hyper::Response::builder()
                    .header("Content-Type", "audio/L16; rate=48000; channels=2")
                    .body(full_body("Hello World"))
                    .unwrap()
            },
            |_src| {
                // No additional setup needed here
            },
        );

        // Set the HTTP source to Playing so that everything can start
        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        // And now check if the data we receive is exactly what we expect it to be
        let expected_output = "Hello World";
        let mut cursor = Cursor::new(expected_output);

        while let Some(buffer) = h.wait_buffer_or_eos() {
            // On the first buffer also check if the duration reported by the HTTP source is what we
            // would expect it to be
            if cursor.position() == 0 {
                assert_eq!(
                    h.src.query_duration::<gst::format::Bytes>(),
                    Some(gst::format::Bytes::from_usize(expected_output.len()))
                );
            }

            // Map the buffer readable and check if it contains exactly the data we would expect at
            // this point after reading everything else we read in previous runs
            let map = buffer.map_readable().unwrap();
            let mut read_buf = vec![0; map.size()];
            assert_eq!(cursor.read(&mut read_buf).unwrap(), map.size());
            assert_eq!(&*map, &*read_buf);
        }

        // Check if everything was read
        assert_eq!(cursor.position(), 11);

        let srcpad = h.src.static_pad("src").unwrap();
        let caps = srcpad.current_caps().unwrap();
        assert_eq!(
            caps,
            gst::Caps::builder("audio/x-unaligned-raw")
                .field("format", "S16BE")
                .field("layout", "interleaved")
                .field("channels", 2i32)
                .field("rate", 48_000i32)
                .build()
        );
    }

    #[test]
    fn test_authorization() {
        use std::io::{Cursor, Read};

        init();

        // Set up a harness that returns "Hello World" for any HTTP request
        // but requires authentication first
        let mut h = Harness::new(
            |req| {
                let headers = req.headers();

                if let Some(authorization) = headers.get("authorization") {
                    assert_eq!(authorization, "Basic dXNlcjpwYXNzd29yZA==");
                    hyper::Response::new(full_body("Hello World"))
                } else {
                    hyper::Response::builder()
                        .status(reqwest::StatusCode::UNAUTHORIZED.as_u16())
                        .header("WWW-Authenticate", "Basic realm=\"realm\"")
                        .body(empty_body())
                        .unwrap()
                }
            },
            |src| {
                src.set_property("user-id", "user");
                src.set_property("user-pw", "password");
            },
        );

        // Set the HTTP source to Playing so that everything can start
        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        // And now check if the data we receive is exactly what we expect it to be
        let expected_output = "Hello World";
        let mut cursor = Cursor::new(expected_output);

        while let Some(buffer) = h.wait_buffer_or_eos() {
            // On the first buffer also check if the duration reported by the HTTP source is what we
            // would expect it to be
            if cursor.position() == 0 {
                assert_eq!(
                    h.src.query_duration::<gst::format::Bytes>(),
                    Some(gst::format::Bytes::from_usize(expected_output.len()))
                );
            }

            // Map the buffer readable and check if it contains exactly the data we would expect at
            // this point after reading everything else we read in previous runs
            let map = buffer.map_readable().unwrap();
            let mut read_buf = vec![0; map.size()];
            assert_eq!(cursor.read(&mut read_buf).unwrap(), map.size());
            assert_eq!(&*map, &*read_buf);
        }

        // Check if everything was read
        assert_eq!(cursor.position(), 11);
    }

    #[test]
    fn test_404_error() {
        init();

        // Harness that always returns 404 and we check if that is mapped to the correct error code
        let mut h = Harness::new(
            |_req| {
                hyper::Response::builder()
                    .status(reqwest::StatusCode::NOT_FOUND.as_u16())
                    .body(empty_body())
                    .unwrap()
            },
            |_src| {},
        );

        h.run(|src| {
            let _ = src.set_state(gst::State::Playing);
        });

        let err_code = h.wait_for_error();
        if let Some(err) = err_code.kind::<gst::ResourceError>() {
            assert_eq!(err, gst::ResourceError::NotFound);
        }
    }

    #[test]
    fn test_403_error() {
        init();

        // Harness that always returns 403 and we check if that is mapped to the correct error code
        let mut h = Harness::new(
            |_req| {
                hyper::Response::builder()
                    .status(reqwest::StatusCode::FORBIDDEN.as_u16())
                    .body(empty_body())
                    .unwrap()
            },
            |_src| {},
        );

        h.run(|src| {
            let _ = src.set_state(gst::State::Playing);
        });

        let err_code = h.wait_for_error();
        if let Some(err) = err_code.kind::<gst::ResourceError>() {
            assert_eq!(err, gst::ResourceError::NotAuthorized);
        }
    }

    #[test]
    fn test_network_error() {
        init();

        // Harness that always fails with a network error
        let mut h = Harness::new(
            |_req| unreachable!(),
            |src| {
                src.set_property("location", "http://0.0.0.0:0");
            },
        );

        h.run(|src| {
            let _ = src.set_state(gst::State::Playing);
        });

        let err_code = h.wait_for_error();
        if let Some(err) = err_code.kind::<gst::ResourceError>() {
            assert_eq!(err, gst::ResourceError::OpenRead);
        }
    }

    #[test]
    fn test_seek_after_ready() {
        use std::io::{Cursor, Read};

        init();

        // Harness that checks if seeking in Ready state works correctly
        let mut h = Harness::new(
            |req| {
                let headers = req.headers();
                if let Some(range) = headers.get("Range") {
                    if range == "bytes=123-" {
                        let mut data_seek = vec![0; 8192 - 123];
                        for (i, d) in data_seek.iter_mut().enumerate() {
                            *d = ((i + 123) % 256) as u8;
                        }

                        hyper::Response::builder()
                            .header("content-length", 8192 - 123)
                            .header("accept-ranges", "bytes")
                            .header("content-range", "bytes 123-8192/8192")
                            .body(full_body(data_seek))
                            .unwrap()
                    } else {
                        panic!("Received an unexpected Range header")
                    }
                } else {
                    // `panic!("Received no Range header")` should be called here but due to a bug
                    // in `basesrc` we cant do that here. If we do a seek in READY state, basesrc
                    // will do a `start()` call without seek. Once we get seek forwarded, the call
                    // with seek is made. This issue has to be solved.
                    // issue link: https://gitlab.freedesktop.org/gstreamer/gstreamer/issues/413
                    let mut data_full = vec![0; 8192];
                    for (i, d) in data_full.iter_mut().enumerate() {
                        *d = (i % 256) as u8;
                    }

                    hyper::Response::builder()
                        .header("content-length", 8192)
                        .header("accept-ranges", "bytes")
                        .body(full_body(data_full))
                        .unwrap()
                }
            },
            |_src| {},
        );

        h.run(|src| {
            src.set_state(gst::State::Ready).unwrap();
        });

        let current_state = h.wait_for_state_change();
        assert_eq!(current_state, gst::State::Ready);

        h.run(|src| {
            src.seek_simple(gst::SeekFlags::FLUSH, 123.bytes()).unwrap();
            src.set_state(gst::State::Playing).unwrap();
        });

        let segment = h.wait_for_segment(false, true);
        assert_eq!(segment.start(), Some(123.bytes()));

        let mut expected_output = vec![0; 8192 - 123];
        for (i, d) in expected_output.iter_mut().enumerate() {
            *d = ((123 + i) % 256) as u8;
        }
        let mut cursor = Cursor::new(expected_output);

        while let Some(buffer) = h.wait_buffer_or_eos() {
            assert_eq!(buffer.offset(), 123 + cursor.position());

            let map = buffer.map_readable().unwrap();
            let mut read_buf = vec![0; map.size()];

            assert_eq!(cursor.read(&mut read_buf).unwrap(), map.size());
            assert_eq!(&*map, &*read_buf);
        }
    }

    #[test]
    fn test_seek_after_buffer_received() {
        use std::io::{Cursor, Read};

        init();

        // Harness that checks if seeking in Playing state after having received a buffer works
        // correctly
        let mut h = Harness::new(
            |req| {
                let headers = req.headers();
                if let Some(range) = headers.get("Range") {
                    if range == "bytes=123-" {
                        let mut data_seek = vec![0; 8192 - 123];
                        for (i, d) in data_seek.iter_mut().enumerate() {
                            *d = ((i + 123) % 256) as u8;
                        }

                        hyper::Response::builder()
                            .header("content-length", 8192 - 123)
                            .header("accept-ranges", "bytes")
                            .header("content-range", "bytes 123-8192/8192")
                            .body(full_body(data_seek))
                            .unwrap()
                    } else {
                        panic!("Received an unexpected Range header")
                    }
                } else {
                    let mut data_full = vec![0; 8192];
                    for (i, d) in data_full.iter_mut().enumerate() {
                        *d = (i % 256) as u8;
                    }

                    hyper::Response::builder()
                        .header("content-length", 8192)
                        .header("accept-ranges", "bytes")
                        .body(full_body(data_full))
                        .unwrap()
                }
            },
            |_src| {},
        );

        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        //wait for a buffer
        let buffer = h.wait_buffer_or_eos().unwrap();
        assert_eq!(buffer.offset(), 0);

        //seek to a position after a buffer is Received
        h.run(|src| {
            src.seek_simple(gst::SeekFlags::FLUSH, 123.bytes()).unwrap();
        });

        let segment = h.wait_for_segment(true, true);
        assert_eq!(segment.start(), Some(123.bytes()));

        let mut expected_output = vec![0; 8192 - 123];
        for (i, d) in expected_output.iter_mut().enumerate() {
            *d = ((123 + i) % 256) as u8;
        }
        let mut cursor = Cursor::new(expected_output);

        while let Some(buffer) = h.wait_buffer_or_eos() {
            assert_eq!(buffer.offset(), 123 + cursor.position());

            let map = buffer.map_readable().unwrap();
            let mut read_buf = vec![0; map.size()];

            assert_eq!(cursor.read(&mut read_buf).unwrap(), map.size());
            assert_eq!(&*map, &*read_buf);
        }
    }

    #[test]
    fn test_seek_with_stop_position() {
        use std::io::{Cursor, Read};

        init();

        // Harness that checks if seeking in Playing state after having received a buffer works
        // correctly
        let mut h = Harness::new(
            |req| {
                let headers = req.headers();
                if let Some(range) = headers.get("Range") {
                    if range == "bytes=123-130" {
                        let mut data_seek = vec![0; 8];
                        for (i, d) in data_seek.iter_mut().enumerate() {
                            *d = ((i + 123) % 256) as u8;
                        }

                        hyper::Response::builder()
                            .header("content-length", 8)
                            .header("accept-ranges", "bytes")
                            .header("content-range", "bytes 123-130/8192")
                            .body(full_body(data_seek))
                            .unwrap()
                    } else {
                        panic!("Received an unexpected Range header")
                    }
                } else {
                    let mut data_full = vec![0; 8192];
                    for (i, d) in data_full.iter_mut().enumerate() {
                        *d = (i % 256) as u8;
                    }

                    hyper::Response::builder()
                        .header("content-length", 8192)
                        .header("accept-ranges", "bytes")
                        .body(full_body(data_full))
                        .unwrap()
                }
            },
            |_src| {},
        );

        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        //wait for a buffer
        let buffer = h.wait_buffer_or_eos().unwrap();
        assert_eq!(buffer.offset(), 0);

        //seek to a position after a buffer is Received
        let start = 123.bytes();
        let stop = 131.bytes();
        h.run(move |src| {
            src.seek(
                1.0,
                gst::SeekFlags::FLUSH,
                gst::SeekType::Set,
                start,
                gst::SeekType::Set,
                stop,
            )
            .unwrap();
        });

        let segment = h.wait_for_segment(true, true);
        assert_eq!(segment.start(), Some(start));
        assert_eq!(segment.stop(), Some(stop));

        let mut expected_output = vec![0; 8];
        for (i, d) in expected_output.iter_mut().enumerate() {
            *d = ((123 + i) % 256) as u8;
        }
        let mut cursor = Cursor::new(expected_output);

        while let Some(buffer) = h.wait_buffer_or_eos() {
            assert_eq!(buffer.offset(), 123 + cursor.position());

            let map = buffer.map_readable().unwrap();
            let mut read_buf = vec![0; map.size()];

            assert_eq!(cursor.read(&mut read_buf).unwrap(), map.size());
            assert_eq!(&*map, &*read_buf);
        }
    }

    #[test]
    fn test_cookies() {
        init();

        // Set up a harness that returns "Hello World" for any HTTP request and sets a cookie in our
        // client
        let mut h = Harness::new(
            |_req| {
                hyper::Response::builder()
                    .header("Set-Cookie", "foo=bar")
                    .body(full_body("Hello World"))
                    .unwrap()
            },
            |_src| {
                // No additional setup needed here
            },
        );

        // Set the HTTP source to Playing so that everything can start
        h.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        let mut num_bytes = 0;
        while let Some(buffer) = h.wait_buffer_or_eos() {
            num_bytes += buffer.size();
        }
        assert_eq!(num_bytes, 11);

        // Set up a second harness that returns "Hello World" for any HTTP request that checks if our
        // client provides the cookie that was set in the previous request
        let mut h2 = Harness::new(
            |req| {
                let headers = req.headers();
                let cookies = headers
                    .get("Cookie")
                    .expect("No cookies set")
                    .to_str()
                    .unwrap();
                assert!(cookies.split(';').any(|c| c == "foo=bar"));
                hyper::Response::builder()
                    .body(full_body("Hello again!"))
                    .unwrap()
            },
            |_src| {
                // No additional setup needed here
            },
        );

        let context = h.src.context("fcast.reqwest.client").expect("No context");
        h2.src.set_context(&context);

        // Set the HTTP source to Playing so that everything can start
        h2.run(|src| {
            src.set_state(gst::State::Playing).unwrap();
        });

        let mut num_bytes = 0;
        while let Some(buffer) = h2.wait_buffer_or_eos() {
            num_bytes += buffer.size();
        }
        assert_eq!(num_bytes, 12);
    }

    /// Adapter from tokio IO traits to hyper IO traits.
    mod tokio_io {
        use pin_project_lite::pin_project;
        use std::{
            pin::Pin,
            task::{Context, Poll},
        };

        pin_project! {
            #[derive(Debug)]
            pub struct TokioIo<T> {
                #[pin]
                inner: T,
            }
        }

        impl<T> TokioIo<T> {
            pub fn new(inner: T) -> Self {
                Self { inner }
            }
        }

        impl<T> hyper::rt::Read for TokioIo<T>
        where
            T: tokio::io::AsyncRead,
        {
            fn poll_read(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                mut buf: hyper::rt::ReadBufCursor<'_>,
            ) -> Poll<Result<(), std::io::Error>> {
                let n = unsafe {
                    let mut tbuf = tokio::io::ReadBuf::uninit(buf.as_mut());
                    match tokio::io::AsyncRead::poll_read(self.project().inner, cx, &mut tbuf) {
                        Poll::Ready(Ok(())) => tbuf.filled().len(),
                        other => return other,
                    }
                };

                unsafe {
                    buf.advance(n);
                }
                Poll::Ready(Ok(()))
            }
        }

        impl<T> hyper::rt::Write for TokioIo<T>
        where
            T: tokio::io::AsyncWrite,
        {
            fn poll_write(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                buf: &[u8],
            ) -> Poll<Result<usize, std::io::Error>> {
                tokio::io::AsyncWrite::poll_write(self.project().inner, cx, buf)
            }

            fn poll_flush(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Result<(), std::io::Error>> {
                tokio::io::AsyncWrite::poll_flush(self.project().inner, cx)
            }

            fn poll_shutdown(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
            ) -> Poll<Result<(), std::io::Error>> {
                tokio::io::AsyncWrite::poll_shutdown(self.project().inner, cx)
            }

            fn is_write_vectored(&self) -> bool {
                tokio::io::AsyncWrite::is_write_vectored(&self.inner)
            }

            fn poll_write_vectored(
                self: Pin<&mut Self>,
                cx: &mut Context<'_>,
                bufs: &[std::io::IoSlice<'_>],
            ) -> Poll<Result<usize, std::io::Error>> {
                tokio::io::AsyncWrite::poll_write_vectored(self.project().inner, cx, bufs)
            }
        }
    }
}
