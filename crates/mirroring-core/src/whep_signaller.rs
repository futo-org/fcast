// SPDX-License-Identifier: MPL-2.0
// From: https://gitlab.freedesktop.org/tkanakamalla/gst-plugins-rs/-/tree/whepsink

use gst::glib::{self, object::ObjectExt};
use gstrswebrtc::signaller::Signallable;

pub const ON_SERVER_STARTED_SIGNAL_NAME: &str = "on-server-started";
/// Emitted with an error string when the server cannot bind. The bus error
/// from the "error" signal is only logged, so this is what ends the session.
pub const ON_SERVER_FAILED_SIGNAL_NAME: &str = "on-server-failed";

mod imp {
    use bytes::Bytes;
    use gst::{glib, prelude::*, subclass::prelude::*};
    use gst_sdp::SDPMessage;
    use gst_webrtc::{WebRTCICEGatheringState, WebRTCSessionDescription};
    use http_body_util::{BodyExt, combinators::BoxBody};
    use hyper::{Method, Response, StatusCode};
    use parking_lot::Mutex;
    use tokio::{net::TcpListener, sync::mpsc};
    use tracing::{debug, error, warn};

    use gstrswebrtc::signaller::{Signallable, SignallableImpl};

    use std::{
        collections::HashMap,
        net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
        sync::LazyLock,
        time::Duration,
    };

    use crate::whep_signaller::{ON_SERVER_FAILED_SIGNAL_NAME, ON_SERVER_STARTED_SIGNAL_NAME};

    const DEFAULT_TIMEOUT_SECONDS: u32 = 30;

    const ENDPOINT_PATH: &str = "/endpoint";
    const RESOURCE_PATH: &str = "/resource";
    const CONTENT_SDP: &str = "application/sdp";
    const CONTENT_TRICKLE_ICE: &str = "application/trickle-ice-sdpfrag";

    struct Settings {
        server_port: u16,
        timeout: u32,
        shutdown_signal: Option<tokio::sync::oneshot::Sender<()>>,
        server_handle: Option<tokio::task::JoinHandle<()>>,
        sdp_answer: HashMap<String, mpsc::Sender<Option<gst_sdp::SDPMessage>>>,
        rt_handle: tokio::runtime::Handle,
    }

    impl Default for Settings {
        fn default() -> Self {
            Self {
                server_port: 0,
                timeout: DEFAULT_TIMEOUT_SECONDS,
                shutdown_signal: None,
                server_handle: None,
                sdp_answer: HashMap::new(),
                rt_handle: tokio::runtime::Handle::try_current().unwrap(),
            }
        }
    }

    fn body_full(data: &[u8]) -> BoxBody<Bytes, hyper::Error> {
        http_body_util::Full::new(Bytes::copy_from_slice(data))
            .map_err(|never| match never {})
            .boxed()
    }

    fn body_empty() -> BoxBody<Bytes, hyper::Error> {
        http_body_util::Empty::<Bytes>::new()
            .map_err(|never| match never {})
            .boxed()
    }

    fn resp_not_found() -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::http::Error> {
        Response::builder()
            .status(StatusCode::NOT_FOUND)
            .body(body_empty())
    }

    #[derive(Default)]
    pub struct Signaller {
        settings: Mutex<Settings>,
    }

    impl Signaller {
        pub fn on_webrtcbin_ready(&self) -> gst::glib::RustClosure {
            glib::closure!(|signaller: &super::WhepServerSignaller,
                            session_id: &str,
                            webrtcbin: &gst::Element| {
                webrtcbin.connect_notify(
                    Some("ice-gathering-state"),
                    glib::clone!(
                        #[weak]
                        signaller,
                        #[to_owned]
                        session_id,
                        move |webrtcbin, _pspec| {
                            let state = webrtcbin
                                .property::<WebRTCICEGatheringState>("ice-gathering-state");

                            match state {
                                WebRTCICEGatheringState::Gathering => {
                                    debug!("ICE gathering started");
                                }
                                WebRTCICEGatheringState::Complete => {
                                    debug!(session_id, "ICE gathering complete");
                                    let ans: Option<gst_sdp::SDPMessage>;
                                    let mut settings = signaller.imp().settings.lock();
                                    if let Some(answer_desc) =
                                        webrtcbin.property::<Option<WebRTCSessionDescription>>(
                                            "local-description",
                                        )
                                    {
                                        ans = Some(answer_desc.sdp().to_owned());
                                    } else {
                                        ans = None;
                                    }

                                    let Some(tx) = settings.sdp_answer.remove(&session_id) else {
                                        error!(session_id, "Missing SDP answer channel sender");
                                        return;
                                    };

                                    settings.rt_handle.spawn(async move {
                                        if let Err(err) = tx.send(ans).await {
                                            error!(?err, "Failed to send SDP");
                                        }
                                    });
                                }
                                _ => (),
                            }
                        }
                    ),
                );
            })
        }

        async fn patch_handler(
            &self,
            _id: String,
        ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::http::Error> {
            // FIXME: implement ICE Trickle and ICE restart
            // emit signal `handle-ice` to for ICE trickle
            Response::builder()
                .status(StatusCode::NOT_IMPLEMENTED)
                .body(body_empty())
            //FIXME: add state checking once ICE trickle is implemented
        }

        async fn delete_handler(
            &self,
            id: String,
        ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::http::Error> {
            if self
                .obj()
                .emit_by_name::<bool>("session-ended", &[&id.as_str()])
            {
                //do nothing
                // FIXME: revisit once the return values are changed in
                // webrtcsink/imp.rs and webrtcsrc/imp.rs
            }

            debug!(id, "Ended session");

            Response::builder().body(body_empty())
        }

        async fn options_handler(
            &self,
        ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::http::Error> {
            Response::builder()
                .header("Access-Post", CONTENT_SDP)
                .body(body_empty())
        }

        async fn post_handler(
            &self,
            body: Bytes,
            id: Option<String>,
        ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::http::Error> {
            let session_id = match id {
                Some(id) => {
                    debug!(id, "Got session id from the URL");
                    id
                }
                None => {
                    debug!("No session id in the URL, generating UUID");
                    uuid::Uuid::new_v4().to_string()
                }
            };

            let (tx, mut rx) = mpsc::channel::<Option<SDPMessage>>(1);

            let wait_timeout = {
                let mut settings = self.settings.lock();
                let wait_timeout = settings.timeout;
                settings.sdp_answer.insert(session_id.clone(), tx);
                drop(settings);
                wait_timeout
            };

            match gst_sdp::SDPMessage::parse_buffer(body.as_ref()) {
                Ok(offer_sdp) => {
                    let offer = gst_webrtc::WebRTCSessionDescription::new(
                        gst_webrtc::WebRTCSDPType::Offer,
                        offer_sdp,
                    );
                    self.obj().emit_by_name::<()>(
                        "session-requested",
                        &[&session_id, &session_id, &offer],
                    );
                }
                Err(err) => {
                    error!(?err, "Could not parse offer SDP");
                    return resp_not_found();
                }
            }

            let result =
                tokio::time::timeout(Duration::from_secs(wait_timeout as u64), rx.recv()).await;

            let answer = match result {
                Ok(ans) => match ans {
                    Some(a) => a,
                    None => {
                        let err = "Channel closed, can't receive SDP".to_owned();
                        error!(err);
                        let res = Response::builder()
                            .status(StatusCode::INTERNAL_SERVER_ERROR)
                            .body(body_full(err.as_bytes()))?;

                        return Ok(res);
                    }
                },
                Err(err) => {
                    error!(?err, "Failed to get answer");

                    let res = Response::builder()
                        .status(StatusCode::INTERNAL_SERVER_ERROR)
                        .body(body_full(err.to_string().as_bytes()))?;

                    return Ok(res);
                }
            };

            // Note: including the ETag in the original "201 Created" response is only
            // REQUIRED if the WHEP resource supports ICE restarts and OPTIONAL
            // otherwise.

            let ans_text: Result<String, String>;
            if let Some(sdp) = answer {
                match sdp.as_text() {
                    Ok(text) => ans_text = Ok(text),
                    Err(err) => {
                        ans_text = Err(format!("Failed to get SDP answer: {err:?}"));
                        error!(?err, "Failed to get SDP answer");
                    }
                }
            } else {
                let err = "SDP Answer is empty!".to_string();
                error!(err);
                ans_text = Err(err);
            }

            // If ans_text is an error. Send error code and error string in the response
            if let Err(err) = ans_text {
                let res = Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(body_full(err.as_bytes()))?;
                return Ok(res);
            }

            let resource_url = RESOURCE_PATH.to_owned() + "/" + &session_id;
            let res = Response::builder()
                .status(StatusCode::CREATED)
                .header(hyper::header::CONTENT_TYPE, CONTENT_SDP)
                .header("location", resource_url)
                .body(body_full(
                    match ans_text {
                        Ok(ans) => ans,
                        Err(err) => err,
                    }
                    .as_bytes(),
                ))?;

            Ok(res)
        }

        async fn handle_request(
            &self,
            req: hyper::Request<hyper::body::Incoming>,
        ) -> Result<Response<BoxBody<Bytes, hyper::Error>>, hyper::http::Error> {
            let uri = req.uri();
            let headers = req.headers();
            let path = uri.path();
            let method = req.method();
            let content_type = match headers.get(hyper::header::CONTENT_TYPE.as_str()) {
                Some(val) => val.to_str().ok(),
                None => None,
            };

            debug!(?uri, ?path, ?method, ?content_type, "Handling request");

            async fn body_to_bytes(body: hyper::body::Incoming) -> Result<Bytes, hyper::Error> {
                Ok(body.collect().await?.to_bytes())
            }

            match (method, path, content_type) {
                // POST /endpoint
                (&Method::POST, ENDPOINT_PATH, Some(CONTENT_SDP)) => {
                    let body = body_to_bytes(req.into_body()).await.unwrap();
                    self.post_handler(body, None).await
                }
                // OPTIONS /endpoint
                (&Method::OPTIONS, ENDPOINT_PATH, _) => self.options_handler().await,
                // POST /endpoint/:id
                (&Method::POST, path, Some(CONTENT_SDP)) if path.starts_with(ENDPOINT_PATH) => {
                    match path.strip_prefix(&format!("{ENDPOINT_PATH}/")) {
                        Some(session_id) => {
                            let session_id = session_id.to_string();
                            let body = body_to_bytes(req.into_body()).await.unwrap();
                            self.post_handler(body, Some(session_id)).await
                        }
                        None => resp_not_found(),
                    }
                }
                // PATCH /resource/:id
                (&Method::PATCH, path, Some(CONTENT_TRICKLE_ICE))
                    if path.starts_with(RESOURCE_PATH) =>
                {
                    match path.strip_prefix(&format!("{RESOURCE_PATH}/")) {
                        Some(session_id) => self.patch_handler(session_id.to_string()).await,
                        None => resp_not_found(),
                    }
                }
                // DELETE /resource/:id
                (&Method::DELETE, path, _) if path.starts_with(RESOURCE_PATH) => {
                    match path.strip_prefix(&format!("{RESOURCE_PATH}/")) {
                        Some(session_id) => self.delete_handler(session_id.to_string()).await,
                        None => resp_not_found(),
                    }
                }
                _ => resp_not_found(),
            }
        }

        fn serve(&self) -> Option<tokio::task::JoinHandle<()>> {
            let mut settings = self.settings.lock();

            let (tx, mut rx) = tokio::sync::oneshot::channel::<()>();
            settings.shutdown_signal = Some(tx);
            drop(settings);

            let obj_weak = self.obj().downgrade();
            let self_weak = self.downgrade();
            let settings = self.settings.lock();
            let server_port = settings.server_port;
            let jh = settings.rt_handle.spawn(async move {
                let (listener, listener_extra, bound_port_v4, bound_port) =
                    match bind_listeners(server_port).await {
                        Ok(bound) => bound,
                        Err(err) => {
                            error!(?err, server_port, "Failed to bind WHEP server");
                            if let Some(obj) = obj_weak.upgrade() {
                                let msg = format!("Unable to start WHEP server: {err}");
                                obj.emit_by_name::<()>(ON_SERVER_FAILED_SIGNAL_NAME, &[&msg]);
                                // webrtcsink turns this into an element error on the bus
                                obj.emit_by_name::<()>("error", &[&msg]);
                            }
                            return;
                        }
                    };

                if let Some(obj) = obj_weak.upgrade() {
                    obj.emit_by_name::<()>(
                        ON_SERVER_STARTED_SIGNAL_NAME,
                        &[
                            &gst::glib::Value::from(bound_port_v4 as u32),
                            &gst::glib::Value::from(bound_port as u32),
                        ],
                    );
                } else {
                    error!("Failed to upgrade obj_weak ");
                }

                /// Returns false on an accept error.
                fn accept_connection(
                    self_weak: &glib::subclass::ObjectImplWeakRef<Signaller>,
                    conn: std::io::Result<(tokio::net::TcpStream, std::net::SocketAddr)>,
                ) -> bool {
                    let (stream, _) = match conn {
                        Ok(conn) => conn,
                        Err(err) => {
                            error!(?err, "Accept error");
                            return false;
                        }
                    };

                    let self_weak = self_weak.clone();
                    tokio::spawn(async move {
                        let stream = hyper_util::rt::TokioIo::new(Box::pin(stream));
                        let server = hyper_util::server::conn::auto::Builder::new(
                            hyper_util::rt::TokioExecutor::new(),
                        );

                        let conn = server.serve_connection_with_upgrades(
                            stream,
                            hyper::service::service_fn({
                                |req| {
                                    let self_weak = self_weak.clone();
                                    async move {
                                        if let Some(self_) = self_weak.upgrade() {
                                            self_.handle_request(req).await
                                        } else {
                                            resp_not_found()
                                        }
                                    }
                                }
                            }),
                        );

                        if let Err(err) = conn.await {
                            error!(?err, "Failed to handle connection");
                        }
                    });
                    true
                }

                fn handle_sig(
                    sig: std::result::Result<(), tokio::sync::oneshot::error::RecvError>,
                ) {
                    match sig {
                        Ok(_) => debug!("Server shut down signal received"),
                        Err(err) => error!(?err, "Sender dropped"),
                    }
                }

                loop {
                    let conn = tokio::select! {
                        conn = listener.accept() => conn,
                        conn = accept_extra(listener_extra.as_ref()) => conn,
                        sig = &mut rx => break handle_sig(sig),
                    };
                    if !accept_connection(&self_weak, conn) {
                        // EMFILE and friends fail again at once, back off instead of spinning
                        tokio::time::sleep(ACCEPT_ERROR_BACKOFF).await;
                    }
                }
            });

            debug!("Started the server...");

            Some(jh)
        }
    }

    const ACCEPT_ERROR_BACKOFF: Duration = Duration::from_millis(100);

    type Accepted = std::io::Result<(tokio::net::TcpStream, SocketAddr)>;

    /// Accepts on the optional second listener, pends forever without one.
    async fn accept_extra(listener: Option<&TcpListener>) -> Accepted {
        match listener {
            Some(listener) => listener.accept().await,
            None => std::future::pending().await,
        }
    }

    /// EAFNOSUPPORT, hardcoded since mcore has no libc dependency.
    /// std maps it to no stable ErrorKind.
    #[cfg(not(any(target_os = "windows", target_vendor = "apple", target_os = "freebsd")))]
    const EAFNOSUPPORT: i32 = 97;
    #[cfg(any(target_vendor = "apple", target_os = "freebsd"))]
    const EAFNOSUPPORT: i32 = 47;

    /// True when the host has no usable IPv6, the only case worth an IPv4
    /// retry.
    #[cfg(not(target_os = "windows"))]
    fn is_ipv6_unavailable(err: &std::io::Error) -> bool {
        err.kind() == std::io::ErrorKind::AddrNotAvailable
            || err.raw_os_error() == Some(EAFNOSUPPORT)
    }

    /// Binds the WHEP listeners as (primary, extra, port v4, port v6).
    /// `[::]` is dual-stack here, IPv4 only when IPv6 is unavailable.
    /// Port v6 is 0 when there is no IPv6 listener.
    #[cfg(not(target_os = "windows"))]
    async fn bind_listeners(
        port: u16,
    ) -> std::io::Result<(TcpListener, Option<TcpListener>, u16, u16)> {
        let v6 = TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)).await;
        match v6 {
            Ok(listener) => {
                let bound = listener.local_addr()?.port();
                Ok((listener, None, bound, bound))
            }
            Err(err) if is_ipv6_unavailable(&err) => {
                warn!(?err, "IPv6 unavailable, falling back to IPv4");
                let v4 =
                    TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port))
                        .await?;
                let bound_v4 = v4.local_addr()?.port();
                Ok((v4, None, bound_v4, 0))
            }
            // EADDRINUSE and friends would fail on IPv4 too or hide a real conflict
            Err(err) => Err(err),
        }
    }

    /// Binds the WHEP listeners as (primary, extra, port v4, port v6).
    /// Windows `[::]` is v6 only, so IPv4 gets its own required listener.
    /// Port v6 is 0 when IPv6 could not be bound.
    #[cfg(target_os = "windows")]
    async fn bind_listeners(
        port: u16,
    ) -> std::io::Result<(TcpListener, Option<TcpListener>, u16, u16)> {
        let v4 =
            TcpListener::bind(SocketAddr::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), port)).await?;
        let bound_v4 = v4.local_addr()?.port();
        match TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), port)).await {
            Ok(v6) => {
                let bound_v6 = v6.local_addr()?.port();
                Ok((v4, Some(v6), bound_v4, bound_v6))
            }
            Err(err) => {
                warn!(?err, "IPv6 bind failed, serving IPv4 only");
                Ok((v4, None, bound_v4, 0))
            }
        }
    }

    impl SignallableImpl for Signaller {
        fn start(&self) {
            debug!("starting the WHEP server");
            let jh = self.serve();
            let mut settings = self.settings.lock();
            settings.server_handle = jh;
        }

        fn stop(&self) {
            let mut settings = self.settings.lock();

            let handle = settings
                .server_handle
                .take()
                .expect("Server handle should be set");

            let tx = settings
                .shutdown_signal
                .take()
                .expect("Shutdown signal Sender needs to be valid");

            if tx.send(()).is_err() {
                error!("Failed to send shutdown signal. Receiver dropped");
            }

            debug!("Await server handle to join");
            settings.rt_handle.block_on(async {
                if let Err(err) = handle.await {
                    error!(?err, "Failed to join server handle");
                };
            });

            debug!("stopped the WHEP server");
        }

        fn end_session(&self, _session_id: &str) {
            //FIXME: send any events to the client
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for Signaller {
        const NAME: &'static str = "WHEPServerSignaller";
        type Type = super::WhepServerSignaller;
        type ParentType = glib::Object;
        type Interfaces = (Signallable,);
    }

    impl ObjectImpl for Signaller {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: LazyLock<Vec<glib::ParamSpec>> = LazyLock::new(|| {
                vec![
                    glib::ParamSpecBoolean::builder("manual-sdp-munging")
                        .nick("Manual SDP munging")
                        .blurb("Whether the signaller manages SDP munging itself")
                        .default_value(false)
                        .read_only()
                        .build(),
                    glib::ParamSpecUInt::builder("server-port")
                        .nick("Server port")
                        .blurb("The port to serve the HTTP server on")
                        .default_value(0)
                        .mutable_ready()
                        .build(),
                ]
            });
            PROPERTIES.as_ref()
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "manual-sdp-munging" => false.to_value(),
                "server-port" => (self.settings.lock().server_port as u32).to_value(),
                _ => unimplemented!(),
            }
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "server-port" => {
                    let mut settings = self.settings.lock();
                    let port: u32 = value.get().expect("type checked upstream");
                    settings.server_port = port as u16;
                }
                _ => unimplemented!(),
            }
        }

        fn signals() -> &'static [glib::subclass::Signal] {
            static SIGNALS: LazyLock<Vec<glib::subclass::Signal>> = LazyLock::new(|| {
                vec![
                    glib::subclass::Signal::builder(ON_SERVER_STARTED_SIGNAL_NAME)
                        // [Ipv4 listener port, Ipv6 listener port], 0 means no listener
                        .param_types([u32::static_type(), u32::static_type()])
                        .build(),
                    glib::subclass::Signal::builder(ON_SERVER_FAILED_SIGNAL_NAME)
                        // [Error message]
                        .param_types([String::static_type()])
                        .build(),
                ]
            });
            SIGNALS.as_ref()
        }
    }
}

glib::wrapper! {
    pub struct WhepServerSignaller(ObjectSubclass<imp::Signaller>) @implements Signallable;
}

impl Default for WhepServerSignaller {
    fn default() -> Self {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        let sig: WhepServerSignaller = glib::Object::new();
        sig.connect_closure("webrtcbin-ready", false, sig.imp().on_webrtcbin_ready());
        sig
    }
}
