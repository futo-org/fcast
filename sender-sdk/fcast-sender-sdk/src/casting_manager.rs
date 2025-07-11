use std::{
    collections::HashMap,
    net::{Ipv4Addr, SocketAddr},
    sync::{
        atomic::{AtomicU16, Ordering},
        Arc,
    },
};

#[cfg(any_protocol)]
use crate::casting_device::CastProtocolType;
use anyhow::Context;

#[cfg(feature = "http-file-server")]
mod http_file_server_prelude {
    pub use bytes::Bytes;
    pub use http::{Request, Response};
    pub use http_body_util::{combinators::BoxBody, BodyExt, Full};
    pub use hyper::{body::Incoming, service::service_fn};
    pub use hyper_util::{
        rt::{TokioExecutor, TokioIo},
        server::conn::auto::Builder,
    };
    pub use std::convert::Infallible;
    pub use uuid::Uuid;

    pub fn empty_response(
        status: http::StatusCode,
    ) -> Result<Response<BoxBody<Bytes, Infallible>>, Infallible> {
        Ok(Response::builder()
            .status(status)
            .body(Full::default().boxed())
            .unwrap())
    }
}
#[cfg(feature = "http-file-server")]
use http_file_server_prelude::*;

use log::{debug, error};
#[cfg(all(feature = "discovery", any_protocol))]
use mdns_sd::ServiceEvent;
#[cfg(feature = "fcast")]
use serde::Deserialize;
use tokio::{
    net::TcpListener,
    sync::mpsc::{Receiver, Sender},
};
use tokio_stream::StreamExt;

#[cfg(feature = "airplay1")]
use crate::airplay1::AirPlay1CastingDevice;
#[cfg(feature = "airplay2")]
use crate::airplay2::AirPlay2CastingDevice;
#[cfg(feature = "chromecast")]
use crate::chromecast::ChromecastCastingDevice;
#[cfg(feature = "fcast")]
use crate::fcast::FCastCastingDevice;
#[cfg(any_protocol)]
use crate::{
    casting_device::{CastingDevice, CastingDeviceEventHandler, CastingDeviceInfo},
    IpAddr,
};
use crate::{AsyncRuntime, AsyncRuntimeError};

#[cfg(feature = "chromecast")]
const CHROMECAST_FRIENDLY_NAME_TXT: &str = "fn";

/// http://:{port}/{location}
#[cfg_attr(feature = "uniffi", derive(uniffi::Record))]
#[derive(Debug)]
pub struct FileStoreEntry {
    pub location: String,
    pub port: u16,
}

#[cfg(feature = "fcast")]
#[derive(Deserialize)]
struct FCastService {
    port: u16,
    r#type: i32,
}

#[cfg(feature = "fcast")]
#[derive(Deserialize)]
struct FCastNetworkConfig {
    name: String,
    addresses: Vec<String>,
    services: Vec<FCastService>,
}

#[cfg(not(any_protocol))]
#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait CastingManagerEventHandler: Send + Sync {}

#[cfg(all(feature = "discovery", any_protocol))]
#[cfg_attr(feature = "uniffi", uniffi::export(with_foreign))]
pub trait CastingManagerEventHandler: Send + Sync {
    fn device_added(&self, device: Arc<dyn CastingDevice>);
    fn device_removed(&self, device: Arc<dyn CastingDevice>);
    /// This event is called when the casting manager received updates about the device over mDNS,
    /// and the new properties has been updated for it.
    fn device_changed(&self, device: Arc<dyn CastingDevice>);
}

enum Command {
    #[cfg(any_protocol)]
    Connect {
        device: Arc<dyn CastingDevice>,
        event_handler: Arc<dyn CastingDeviceEventHandler>,
    },
    #[cfg(feature = "http-file-server")]
    ServeFile { endpoint: Uuid, data: Vec<u8> },
    #[allow(dead_code)]
    Quit,
}

struct InnerManager {
    #[cfg(all(feature = "discovery", any_protocol))]
    event_handler: Arc<dyn CastingManagerEventHandler>,
    file_store_port: Arc<AtomicU16>,
}

impl InnerManager {
    fn new(
        #[cfg(all(feature = "discovery", any_protocol))] event_handler: Arc<dyn CastingManagerEventHandler>,
        file_store_port: Arc<AtomicU16>,
    ) -> Self {
        Self {
            #[cfg(all(feature = "discovery", any_protocol))]
            event_handler,
            file_store_port,
        }
    }

    #[cfg(all(feature = "discovery", feature = "fcast"))]
    fn handle_fcast_mdns_resolved(
        &self,
        devices: &mut HashMap<String, Arc<dyn CastingDevice>>,
        service_info: mdns_sd::ServiceInfo,
    ) {
        debug!("Receiver added: {service_info:?}");
        let mut name = service_info.get_fullname().to_string();
        if let Some(stripped) = name.strip_suffix("._fcast._tcp.local.") {
            name = stripped.to_string();
        }
        let addresses = service_info
            .get_addresses()
            .iter()
            .map(IpAddr::from)
            .collect::<Vec<IpAddr>>();
        let port = service_info.get_port();
        if let Some(device) = devices.get(&name) {
            debug!("Updating FCast device `{name}`");
            device.set_addresses(addresses);
            device.set_port(port);
            self.event_handler.device_changed(Arc::clone(device));
        } else {
            debug!("New FCast device `{name}`");
            let device: Arc<dyn CastingDevice> = match FCastCastingDevice::new(
                CastingDeviceInfo::fcast(name.clone(), addresses, port),
            ) {
                Ok(dev) => Arc::new(dev),
                Err(err) => {
                    error!("Failed to crate device: {err}");
                    return;
                }
            };
            devices.insert(service_info.get_fullname().to_string(), Arc::clone(&device));
            self.event_handler.device_added(device);
        }
    }

    async fn work(self, cmd_rx: Receiver<Command>) -> anyhow::Result<()> {
        #[cfg(all(feature = "discovery", any_protocol))]
        let mdns = mdns_sd::ServiceDaemon::new().context("Failed to crate mdns ServiceDaemon")?;

        #[cfg(all(feature = "discovery", any_protocol))]
        macro_rules! browse {
            ($mdns:expr, $service:expr) => {
                $mdns
                    .browse($service)
                    .context(concat!("Failed to browse `", $service, "`"))
            };
        }

        #[cfg(all(feature = "discovery", feature = "fcast"))]
        let fcast_mdns_receiver = browse!(mdns, "_fcast._tcp.local.")?;
        #[cfg(all(feature = "discovery", feature = "fcast"))]
        let fastcast_mdns_receiver = browse!(mdns, "_fastcast._tcp.local.")?;
        #[cfg(all(feature = "discovery", feature = "chromecast"))]
        let chromecast_mdns_receiver = browse!(mdns, "_googlecast._tcp.local.")?;
        #[cfg(all(feature = "discovery", any(feature = "airplay1", feature = "airplay2")))]
        let airplay_mdns_receiver = browse!(mdns, "_airplay._tcp.local.")?;

        #[cfg(all(feature = "discovery", any_protocol))]
        let mut devices: HashMap<String, Arc<dyn CastingDevice>> = HashMap::new();

        #[cfg(feature = "http-file-server")]
        let file_store: Arc<tokio::sync::RwLock<HashMap<Uuid, Vec<u8>>>> =
            Arc::new(tokio::sync::RwLock::new(HashMap::new()));
        #[cfg(feature = "http-file-server")]
        let listen_addr = SocketAddr::new(std::net::IpAddr::V4(Ipv4Addr::UNSPECIFIED), 0u16);
        #[cfg(feature = "http-file-server")]
        let tcp_listener = TcpListener::bind(listen_addr).await?;
        #[cfg(feature = "http-file-server")]
        self.file_store_port
            .store(tcp_listener.local_addr()?.port(), Ordering::Relaxed);

        #[cfg(all(feature = "discovery", any_protocol))]
        macro_rules! handle_service_event {
            ($event:expr, $on_resolved:expr) => {
                match $event {
                    mdns_sd::ServiceEvent::ServiceResolved(service_info) => {
                        $on_resolved(service_info)
                    }
                    mdns_sd::ServiceEvent::ServiceRemoved(_, fullname) => {
                        if let Some(device) = devices.remove(&fullname) {
                            self.event_handler.device_removed(device);
                        } else {
                            debug!("Service `{fullname}` was removed but no device was found");
                        }
                    }
                    _ => (),
                }
            };
        }

        enum InternalMessage {
            #[cfg(feature = "http-file-server")]
            HttpRequester((tokio::net::TcpStream, std::net::SocketAddr)),
            Cmd(Command),
            #[cfg(all(feature = "discovery", feature = "fcast"))]
            FCastServiceEvent(ServiceEvent),
            #[cfg(all(feature = "discovery", feature = "chromecast"))]
            ChromecastServiceEvent(ServiceEvent),
            #[cfg(all(feature = "discovery", any(feature = "airplay1", feature = "airplay2")))]
            AirPlayServiceEvent(ServiceEvent),
        }

        #[allow(unused_mut)]
        let mut msg_stream =
            futures::stream::unfold(cmd_rx, |mut cmd_rx: Receiver<Command>| async move {
                match cmd_rx.recv().await {
                    Some(cmd) => Some((InternalMessage::Cmd(cmd), cmd_rx)),
                    None => {
                        error!("No more commands");
                        None
                    }
                }
            });

        #[cfg(feature = "http-file-server")]
        let http_conn_stream =
            futures::stream::unfold(tcp_listener, |tcp_listener: TcpListener| async move {
                match tcp_listener.accept().await {
                    Ok(tup) => Some((InternalMessage::HttpRequester(tup), tcp_listener)),
                    Err(err) => {
                        error!("Failed to accept HTTP client stream: {err}");
                        None
                    }
                }
            });

        #[cfg(all(feature = "discovery", feature = "fcast"))]
        let fcast_mdns_stream = futures::stream::unfold(
            (fcast_mdns_receiver, fastcast_mdns_receiver),
            |(fcast_mdns_receiver, fastcast_mdns_receiver): (
                mdns_sd::Receiver<ServiceEvent>,
                mdns_sd::Receiver<ServiceEvent>,
            )| async move {
                tokio::select! {
                    fcast = fcast_mdns_receiver.recv_async() => Some((
                        InternalMessage::FCastServiceEvent(fcast.ok()?),
                        (fcast_mdns_receiver, fastcast_mdns_receiver)
                    )),
                    fastcast = fastcast_mdns_receiver.recv_async() => Some((
                        InternalMessage::FCastServiceEvent(fastcast.ok()?),
                        (fcast_mdns_receiver, fastcast_mdns_receiver)
                    )),
                }
            },
        );

        #[cfg(all(feature = "discovery", feature = "chromecast"))]
        let chromecast_mdns_stream = futures::stream::unfold(
            chromecast_mdns_receiver,
            |chromecast_mdns_receiver: mdns_sd::Receiver<ServiceEvent>| async move {
                let event = chromecast_mdns_receiver.recv_async().await.ok()?;
                Some((
                    InternalMessage::ChromecastServiceEvent(event),
                    chromecast_mdns_receiver,
                ))
            },
        );

        #[cfg(all(feature = "discovery", any(feature = "airplay1", feature = "airplay2")))]
        let airplay_mdns_stream = futures::stream::unfold(
            airplay_mdns_receiver,
            |airplay_mdns_receiver: mdns_sd::Receiver<ServiceEvent>| async move {
                let event = airplay_mdns_receiver.recv_async().await.ok()?;
                Some((
                    InternalMessage::AirPlayServiceEvent(event),
                    airplay_mdns_receiver,
                ))
            },
        );

        tokio::pin!(msg_stream);
        #[cfg(feature = "http-file-server")]
        tokio::pin!(http_conn_stream);
        #[cfg(all(feature = "discovery", feature = "fcast"))]
        tokio::pin!(fcast_mdns_stream);
        #[cfg(all(feature = "discovery", feature = "chromecast"))]
        tokio::pin!(chromecast_mdns_stream);
        #[cfg(all(feature = "discovery", any(feature = "airplay1", feature = "airplay2")))]
        tokio::pin!(airplay_mdns_stream);

        #[allow(unused_mut)]
        #[cfg(feature = "http-file-server")]
        let mut msg_stream = msg_stream.merge(http_conn_stream);
        #[allow(unused_mut)]
        #[cfg(all(feature = "discovery", feature = "fcast"))]
        let mut msg_stream = msg_stream.merge(fcast_mdns_stream);
        #[allow(unused_mut)]
        #[cfg(all(feature = "discovery", feature = "chromecast"))]
        let mut msg_stream = msg_stream.merge(chromecast_mdns_stream);
        #[allow(unused_mut)]
        #[cfg(all(feature = "discovery", any(feature = "airplay1", feature = "airplay2")))]
        let mut msg_stream = msg_stream.merge(airplay_mdns_stream);

        while let Some(msg) = msg_stream.next().await {
            match msg {
                #[cfg(feature = "http-file-server")]
                InternalMessage::HttpRequester((stream, addr)) => {
                    let file_store = Arc::clone(&file_store);
                    tokio::spawn(async move {
                        debug!("Handling request from: {addr}");
                        let res = Builder::new(TokioExecutor::new())
                            .serve_connection(
                                TokioIo::new(stream),
                                service_fn(|request: Request<Incoming>| {
                                    let file_store = Arc::clone(&file_store);
                                    async move {
                                        if request.method() != http::method::Method::GET {
                                            return empty_response(
                                                http::StatusCode::METHOD_NOT_ALLOWED,
                                            );
                                        }

                                        let Some(path) = request.uri().path().strip_prefix('/')
                                        else {
                                            return empty_response(http::StatusCode::NOT_FOUND);
                                        };

                                        let Ok(requested_uuid) = Uuid::parse_str(path) else {
                                            error!("Requested path ({path}) is not a valid uuid");
                                            return empty_response(
                                                http::StatusCode::INTERNAL_SERVER_ERROR,
                                            );
                                        };

                                        let file_store = file_store.read().await;

                                        let Some(data) = file_store.get(&requested_uuid) else {
                                            error!("Resource not found: {requested_uuid}");
                                            return empty_response(http::StatusCode::NOT_FOUND);
                                        };

                                        let response = Response::builder()
                                            .header(
                                                http::header::CONTENT_TYPE,
                                                "application/octet-stream",
                                            )
                                            .body(Full::new(Bytes::from(data.clone())).boxed())
                                            .expect(
                                                "values provided to the builder should be valid",
                                            );

                                        Ok::<Response<BoxBody<Bytes, Infallible>>, Infallible>(
                                            response,
                                        )
                                    }
                                }),
                            )
                            .await;
                        if let Err(err) = res {
                            error!("Failed to handle request: {err}");
                        }
                    });
                }
                InternalMessage::Cmd(cmd) => match cmd {
                    #[cfg(any_protocol)]
                    Command::Connect {
                        event_handler,
                        device,
                    } => {
                        debug!("Trying to start device...");
                        match device.soft_start(event_handler) {
                            Ok(work_fut) => {
                                let device = Arc::clone(&device);
                                tokio::spawn(async move {
                                    work_fut.await;
                                    if let Err(err) = device.stop() {
                                        error!("Failed to stop device: {err}");
                                    }
                                });
                            }
                            Err(err) => {
                                error!("Failed to soft start device: {err}")
                            }
                        }
                    }
                    #[cfg(feature = "http-file-server")]
                    Command::ServeFile { endpoint, data } => {
                        let mut store = file_store.write().await;
                        if store.insert(endpoint, data).is_some() {
                            debug!("File dup: {endpoint}");
                        }
                    }
                    Command::Quit => break,
                },
                #[cfg(all(feature = "discovery", feature = "fcast"))]
                InternalMessage::FCastServiceEvent(service_event) => {
                    handle_service_event!(service_event, |service_info: mdns_sd::ServiceInfo| {
                        self.handle_fcast_mdns_resolved(&mut devices, service_info);
                    })
                }
                #[cfg(all(feature = "discovery", feature = "chromecast"))]
                InternalMessage::ChromecastServiceEvent(service_event) => {
                    handle_service_event!(service_event, |service_info: mdns_sd::ServiceInfo| {
                        let name = service_info
                            .get_property(CHROMECAST_FRIENDLY_NAME_TXT)
                            .map(|name| name.val_str().to_string())
                            .unwrap_or(service_info.get_fullname().to_string());
                        let addresses = service_info
                            .get_addresses()
                            .iter()
                            .map(IpAddr::from)
                            .collect::<Vec<IpAddr>>();
                        let port = service_info.get_port();
                        if let Some(device) = devices.get(&name) {
                            debug!("Updating Chromecast device `{name}`");
                            device.set_addresses(addresses);
                            device.set_port(port);
                            self.event_handler.device_changed(Arc::clone(device));
                        } else {
                            debug!("New Chromecast device `{name}`");
                            let device: Arc<dyn CastingDevice> = match ChromecastCastingDevice::new(
                                CastingDeviceInfo::chromecast(name, addresses, port),
                            ) {
                                Ok(dev) => Arc::new(dev),
                                Err(err) => {
                                    error!("Failed to crate device: {err}");
                                    return;
                                }
                            };
                            devices.insert(
                                service_info.get_fullname().to_string(),
                                Arc::clone(&device),
                            );
                            self.event_handler.device_added(device);
                        }
                    })
                }
                #[cfg(all(feature = "discovery", any(feature = "airplay1", feature = "airplay2")))]
                InternalMessage::AirPlayServiceEvent(service_event) => {
                    handle_service_event!(service_event, |service_info: mdns_sd::ServiceInfo| {
                        debug!("Receiver added: {service_info:?}");
                        let mut name = service_info.get_fullname().to_string();
                        if let Some(stripped) = name.strip_suffix("._airplay._tcp.local.") {
                            name = stripped.to_string();
                        }

                        if let Some(device) = devices.get(&name) {
                            debug!("Updating AirPlay device `{name}`");
                            device.set_addresses(
                                service_info
                                    .get_addresses()
                                    .iter()
                                    .map(IpAddr::from)
                                    .collect::<Vec<IpAddr>>(),
                            );
                            device.set_port(service_info.get_port());
                            self.event_handler.device_changed(Arc::clone(device));
                        } else {
                            debug!("New AirPlay device `{name}`");
                            let is_airplay_2 = {
                                if let Some(srcvers) =
                                    service_info.get_property("srcvers").map(|r| r.val_str())
                                {
                                    debug!("AirPlay srcvers: {srcvers}");
                                    if let Some(Ok(vers)) =
                                        srcvers.split('.').nth(0).map(|v| v.parse::<u32>())
                                    {
                                        vers >= 200
                                    } else {
                                        true
                                    }
                                } else {
                                    false
                                }
                            };
                            // let features = {
                            //     let fstr = service_info.get_property("features").map(|f| f.val_str()).unwrap_or("0x0,0x0");
                            //     let mut split = fstr.split(',');
                            //     let lo = u32::from_str_radix(
                            //         split.nth(0).unwrap_or("0x0").strip_prefix("0x").unwrap_or("0"), 16
                            //     ).unwrap_or(0);
                            //     let hi = u32::from_str_radix(
                            //         split.nth(1).unwrap_or("0x0").strip_prefix("0x").unwrap_or("0"), 16
                            //     ).unwrap_or(0);
                            //     crate::airplay_common::AirPlayFeatures::from_bits_retain(((hi as u64) << 32) + lo as u64)
                            // };
                            let device: Arc<dyn CastingDevice> = if is_airplay_2 {
                                // if !features.contains(AirPlayFeatures::SupportsHkPairingAndAccessControl) {
                                //     debug!("Ignoring AirPlay2 device because it does not support HomeKit pairing and access control");
                                //     return;
                                // }
                                #[cfg(feature = "airplay2")]
                                match AirPlay2CastingDevice::new(CastingDeviceInfo::airplay2(
                                    name.clone(),
                                    service_info
                                        .get_addresses()
                                        .iter()
                                        .map(IpAddr::from)
                                        .collect::<Vec<IpAddr>>(),
                                    service_info.get_port(),
                                )) {
                                    Ok(dev) => Arc::new(dev),
                                    Err(err) => {
                                        error!("Failed to crate device: {err}");
                                        return;
                                    }
                                }
                                #[cfg(not(feature = "airplay2"))]
                                return;
                            } else {
                                #[cfg(feature = "airplay1")]
                                match AirPlay1CastingDevice::new(CastingDeviceInfo::airplay1(
                                    name.clone(),
                                    service_info
                                        .get_addresses()
                                        .iter()
                                        .map(IpAddr::from)
                                        .collect::<Vec<IpAddr>>(),
                                    service_info.get_port(),
                                )) {
                                    Ok(dev) => Arc::new(dev),
                                    Err(err) => {
                                        error!("Failed to crate device: {err}");
                                        return;
                                    }
                                }

                                #[cfg(not(feature = "airplay1"))]
                                return;
                            };

                            devices.insert(
                                service_info.get_fullname().to_string(),
                                Arc::clone(&device),
                            );
                            self.event_handler.device_added(device);
                        }
                    })
                }
            }
        }

        Ok(())
    }
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Error))]
#[cfg_attr(feature = "uniffi", uniffi(flat_error))]
#[derive(thiserror::Error, Debug)]
pub enum CastingManagerError {
    #[error("Failed to send command to worker thread")]
    FailedToSendCommand,
    #[error("Invalid URL")]
    InvalidUrl,
    #[error("No services found in network config")]
    NoServices,
    #[error("No valid addresses found in network config")]
    NoAddresses,
    #[error("Failed to create async runtime")]
    AsyncRuntime(#[from] AsyncRuntimeError),
    #[error("File server is not running")]
    FileServerNotRunning,
}

#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct CastingManager {
    runtime: AsyncRuntime,
    cmd_tx: Sender<Command>,
    file_store_port: Arc<AtomicU16>,
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg(feature = "http-file-server")]
impl CastingManager {
    /// Host a file in the HTTP file store.
    pub fn host_file(&self, data: Vec<u8>) -> Result<FileStoreEntry, CastingManagerError> {
        let port = self.file_store_port.load(Ordering::Relaxed);
        if port == 0 {
            return Err(CastingManagerError::FileServerNotRunning);
        }
        let id = Uuid::new_v4();
        self.send_command(Command::ServeFile { endpoint: id, data })?;

        Ok(FileStoreEntry {
            location: id.to_string(),
            port,
        })
    }
}

#[cfg_attr(feature = "uniffi", uniffi::export)]
#[cfg(feature = "fcast")]
impl CastingManager {
    /// Attempt to parse and instantiate a device from a URL.
    ///
    /// This currently only supported for FCast, where the URL format looks like this:
    ///
    /// ```text
    /// fcast://r/<base64-encodec-payload>
    /// ```
    ///
    /// The decodec payload is a JSON value with the following definition:
    ///
    /// ```json
    /// {
    ///     "name": string,
    ///     "addresses": [string],
    ///     "services": [
    ///         {
    ///             "port": u16,
    ///             "type": i32 // 0 = TCP
    ///         },
    ///         ...
    ///     ]
    /// }
    /// ```
    #[cfg(any_protocol)]
    pub fn handle_url(&self, url: String) -> Result<Arc<dyn CastingDevice>, CastingManagerError> {
        let url = match url::Url::parse(&url) {
            Ok(uri) => uri,
            Err(err) => {
                error!("Invalid URL: {err}");
                return Err(CastingManagerError::InvalidUrl);
            }
        };

        if url.scheme() != "fcast" {
            error!("Expected URL scheme to be fcast, was {}", url.scheme());
            return Err(CastingManagerError::InvalidUrl);
        }

        if url.host_str() != Some("r") {
            error!("Expected URL type to be r");
            return Err(CastingManagerError::InvalidUrl);
        }

        let connection_info = url
            .path_segments()
            .ok_or(CastingManagerError::InvalidUrl)?
            .next()
            .ok_or(CastingManagerError::InvalidUrl)?;

        use base64::{
            alphabet::URL_SAFE,
            engine::{general_purpose::GeneralPurpose, DecodePaddingMode, GeneralPurposeConfig},
            Engine as _,
        };
        let b64_engine = GeneralPurpose::new(
            &URL_SAFE,
            GeneralPurposeConfig::new().with_decode_padding_mode(DecodePaddingMode::Indifferent),
        );
        let json = match b64_engine.decode(connection_info) {
            Ok(json) => json,
            Err(err) => {
                error!("Failed to decode base64: {err}");
                return Err(CastingManagerError::InvalidUrl);
            }
        };
        let found_info: FCastNetworkConfig = match serde_json::from_slice(&json) {
            Ok(info) => info,
            Err(err) => {
                error!("Failed to decode network config json: {err}");
                return Err(CastingManagerError::InvalidUrl);
            }
        };

        let tcp_service = 'out: {
            for service in found_info.services {
                if service.r#type == 0 {
                    break 'out service;
                }
            }
            error!("No TCP service found in network config");
            return Err(CastingManagerError::NoServices);
        };

        let addrs = found_info
            .addresses
            .iter()
            .map(|a| a.parse::<std::net::IpAddr>())
            .map(|a| match a {
                Ok(a) => Some(IpAddr::from(&a)),
                Err(_) => None,
            })
            .collect::<Option<Vec<IpAddr>>>()
            .ok_or(CastingManagerError::NoAddresses)?;

        let device: Arc<dyn CastingDevice> = Arc::new(FCastCastingDevice::new(
            CastingDeviceInfo::fcast(found_info.name, addrs, tcp_service.port),
        )?);

        Ok(device)
    }
}

#[cfg(not(any_protocol))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastingManager {
    /// Arguments:
    /// * `manager_event_handler`: The event handler used to communicate changes from the background
    ///   thread
    #[cfg_attr(feature = "uniffi", uniffi::constructor)]
    pub fn new() -> Result<Self, AsyncRuntimeError> {
        // TODO: is 1 thread enough?
        let runtime = AsyncRuntime::new(Some(1), "casting-manager-async-runtime")?;

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<Command>(50);

        let file_store_port = Arc::new(AtomicU16::new(0));

        let file_store_port_clone = Arc::clone(&file_store_port);
        runtime.spawn(async move {
            if let Err(err) = InnerManager::new(file_store_port_clone).work(cmd_rx).await {
                error!("Error occurred when working: {err}");
            }
        });

        Ok(Self {
            runtime,
            cmd_tx,
            file_store_port,
        })
    }
}

#[cfg(all(feature = "discovery", any_protocol))]
#[cfg_attr(feature = "uniffi", uniffi::export)]
impl CastingManager {
    /// Arguments:
    /// * `manager_event_handler`: The event handler used to communicate changes from the background
    ///   thread
    #[cfg_attr(feature = "uniffi", uniffi::constructor)]
    pub fn new(
        manager_event_handler: Arc<dyn CastingManagerEventHandler>,
    ) -> Result<Self, AsyncRuntimeError> {
        // TODO: is 1 thread enough?
        let runtime = AsyncRuntime::new(Some(1), "casting-manager-async-runtime")?;

        let (cmd_tx, cmd_rx) = tokio::sync::mpsc::channel::<Command>(50);

        let file_store_port = Arc::new(AtomicU16::new(0));

        let file_store_port_clone = Arc::clone(&file_store_port);
        runtime.spawn(async move {
            if let Err(err) = InnerManager::new(manager_event_handler, file_store_port_clone)
                .work(cmd_rx)
                .await
            {
                error!("Error occurred when working: {err}");
            }
        });

        Ok(Self {
            runtime,
            cmd_tx,
            file_store_port,
        })
    }

    /// Try to connect to a device.
    pub fn connect_device(
        &self,
        device: Arc<dyn CastingDevice>,
        event_handler: Arc<dyn CastingDeviceEventHandler>,
    ) -> Result<(), CastingManagerError> {
        self.send_command(Command::Connect {
            device,
            event_handler,
        })
    }

    fn device_from_casting_device_info(
        &self,
        info: CastingDeviceInfo,
    ) -> Result<Arc<dyn CastingDevice>, CastingManagerError> {
        match info.r#type {
            #[cfg(feature = "chromecast")]
            CastProtocolType::Chromecast => Ok(Arc::new(ChromecastCastingDevice::new(info)?)),
            #[cfg(feature = "airplay1")]
            CastProtocolType::AirPlay => Ok(Arc::new(AirPlay1CastingDevice::new(info)?)),
            #[cfg(feature = "airplay2")]
            CastProtocolType::AirPlay2 => Ok(Arc::new(AirPlay2CastingDevice::new(info)?)),
            #[cfg(feature = "fcast")]
            CastProtocolType::FCast => Ok(Arc::new(FCastCastingDevice::new(info)?)),
        }
    }
}

impl CastingManager {
    fn send_command(&self, cmd: Command) -> Result<(), CastingManagerError> {
        let tx = self.cmd_tx.clone();
        self.runtime.spawn(async move { tx.send(cmd).await });

        Ok(())
    }
}
