use std::{
    net::SocketAddr,
    sync::{Arc, Mutex},
    // time::Duration,
};

use anyhow::{anyhow, Context};
// use bytes::Bytes;
// use http::StatusCode;
// use http_body_util::{BodyExt, Empty, Full};
use log::{debug, error, info};
use tokio::{
    // net::TcpStream,
    runtime::Handle,
    sync::mpsc::{Receiver, Sender},
};
use uuid::Uuid;

use crate::{
    casting_device::{
        CastingDevice, CastingDeviceError, DeviceConnectionState, DeviceEventHandler,
        DeviceFeature, DeviceInfo, GenericEventSubscriptionGroup, ProtocolType,
    },
    utils, IpAddr,
};

#[derive(Debug, PartialEq)]
enum Command {
    ChangeSpeed(f64),
    Quit,
    LoadUrl {
        content_type: String,
        url: String,
        resume_position: f64,
        speed: Option<f64>,
    },
    PausePlayback,
    ResumePlayback,
    StopPlayback,
}

#[allow(dead_code)]
struct State {
    rt_handle: Handle,
    started: bool,
    command_tx: Option<Sender<Command>>,
    addresses: Vec<IpAddr>,
    name: String,
    port: u16,
}

impl State {
    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            rt_handle,
            started: false,
            command_tx: None,
            addresses: device_info.addresses,
            name: device_info.name,
            port: device_info.port,
        }
    }
}

#[allow(dead_code)]
#[cfg_attr(feature = "uniffi", derive(uniffi::Object))]
pub struct AirPlay1Device {
    state: Mutex<State>,
}

impl AirPlay1Device {
    const SUPPORTED_FEATURES: [DeviceFeature; 1] = [DeviceFeature::SetSpeed];

    pub fn new(device_info: DeviceInfo, rt_handle: Handle) -> Self {
        Self {
            state: Mutex::new(State::new(device_info, rt_handle)),
        }
    }
}

struct InnerDevice {
    event_handler: Arc<dyn DeviceEventHandler>,
    #[allow(dead_code)]
    session_id: String,
}

impl InnerDevice {
    pub fn new(event_handler: Arc<dyn DeviceEventHandler>) -> Self {
        Self {
            event_handler,
            session_id: Uuid::new_v4().to_string(),
        }
    }

    async fn post(
        &self,
        _addr: &SocketAddr,
        _path: &str,
        _body: Option<(&str, &str)>,
    ) -> anyhow::Result<bool> {
        // let stream =
        //     tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(addr)).await??;
        // let io = hyper_util::rt::TokioIo::new(stream);

        // let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        // tokio::task::spawn(async move {
        //     if let Err(err) = conn.await {
        //         error!("Connection failed: {err}");
        //     }
        // });

        // let req_builder = hyper::Request::builder()
        //     .method("POST")
        //     .uri(format!("/{path}"))
        //     .header("X-Apple-Device-ID", "0xdc2b61a0ce79")
        //     .header("User-Agent", "MediaControl/1.0")
        //     .header("X-Apple-Session-ID", &self.session_id);

        // let req = match body {
        //     Some((content_type, body)) => req_builder
        //         .header("Content-Length", body.len().to_string())
        //         .header("Content-Type", content_type)
        //         .body(Full::new(Bytes::from_owner(body.as_bytes().to_vec())).boxed())?,
        //     None => req_builder
        //         .header("Content-Length", "0")
        //         .body(Full::default().boxed())?,
        // };

        // let res = tokio::time::timeout(Duration::from_secs(1), sender.send_request(req)).await??;

        // if res.status() != StatusCode::OK {
        //     error!("Status code: {}", res.status());
        //     return Ok(false);
        // }

        Ok(true)
    }

    #[allow(dead_code)]
    async fn get(&self, _addr: &SocketAddr, _path: &str) -> anyhow::Result<String> {
        // let stream =
        //     tokio::time::timeout(Duration::from_secs(3), TcpStream::connect(addr)).await??;
        // let io = hyper_util::rt::TokioIo::new(stream);

        // let (mut sender, conn) = hyper::client::conn::http1::handshake(io).await?;
        // tokio::task::spawn(async move {
        //     if let Err(err) = conn.await {
        //         error!("Connection failed: {err}");
        //     }
        // });

        // let req = hyper::Request::builder()
        //     .method("GET")
        //     .uri(format!("/{path}"))
        //     .header("X-Apple-Device-ID", "0xdc2b61a0ce79")
        //     .header("User-Agent", "MediaControl/1.0")
        //     .header("X-Apple-Session-ID", &self.session_id)
        //     .header("Content-Length", "0")
        //     .body(Empty::<Bytes>::new())?;

        // let mut res = sender.send_request(req).await?;

        // if res.status() != StatusCode::OK {
        //     error!("Status code: {}", res.status());
        //     todo!();
        //     // return Ok(false);
        // }

        // let mut body: Vec<u8> = Vec::new();
        // while let Some(next) = res.frame().await {
        //     let frame = next?;
        //     if let Some(chunk) = frame.data_ref() {
        //         body.extend_from_slice(chunk);
        //     }
        // }

        // Ok(String::from_utf8(body)?)
        todo!()
    }

    async fn inner_work(
        &mut self,
        addrs: Vec<SocketAddr>,
        mut cmd_rx: Receiver<Command>,
    ) -> anyhow::Result<()> {
        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connecting);

        let (used_remote_address, local_address) = {
            let Some(stream) =
                utils::try_connect_tcp(addrs, 5, &mut cmd_rx, |cmd| cmd == Command::Quit).await?
            else {
                debug!("Received Quit command in connect loop");
                self.event_handler
                    .connection_state_changed(DeviceConnectionState::Disconnected);
                return Ok(());
            };
            (
                stream.peer_addr().context("failed to get peer address")?,
                stream.local_addr().context("failed to get local address")?,
            )
        };

        info!("Successfully connected");

        self.event_handler
            .connection_state_changed(DeviceConnectionState::Connected {
                used_remote_addr: used_remote_address.ip().into(),
                local_addr: local_address.ip().into(),
            });

        loop {
            let cmd = cmd_rx.recv().await.ok_or(anyhow!("No more commands"))?;

            debug!("Received command: {cmd:?}");

            match cmd {
                Command::Quit => break,
                Command::ChangeSpeed(speed) => {
                    self.post(&used_remote_address, &format!("rate?={speed}"), None)
                        .await?;
                }
                Command::PausePlayback => {
                    self.post(&used_remote_address, "rate?value=0.000000", None)
                        .await?;
                }
                Command::ResumePlayback => {
                    self.post(&used_remote_address, "rate?value=1.000000", None)
                        .await?;
                }
                Command::StopPlayback => {
                    self.post(&used_remote_address, "stop", None).await?;
                }
                Command::LoadUrl {
                    content_type: _,
                    url,
                    resume_position: _,
                    speed,
                } => {
                    // TODO: resume_position
                    self.post(
                        &used_remote_address,
                        "play",
                        Some((
                            "text/parameters",
                            &format!("Content-Location: {url}\r\nStart-Position: 0"),
                        )),
                    )
                    .await?;

                    if let Some(speed) = speed {
                        self.post(&used_remote_address, &format!("rate?value={speed}"), None)
                            .await?;
                    }
                }
            }
        }

        info!("Shutting down...");

        Ok(())
    }

    pub async fn work(mut self, addrs: Vec<SocketAddr>, cmd_rx: Receiver<Command>) {
        debug!("Starting to work...");

        if let Err(err) = self.inner_work(addrs, cmd_rx).await {
            error!("Inner work error: {err}");
        }

        self.event_handler
            .connection_state_changed(DeviceConnectionState::Disconnected);
    }
}

impl AirPlay1Device {
    fn send_command(&self, cmd: Command) -> Result<(), CastingDeviceError> {
        let state = self.state.lock().unwrap();
        let Some(tx) = &state.command_tx else {
            error!("Missing command tx");
            return Err(CastingDeviceError::FailedToSendCommand);
        };

        debug!("Sending command: {cmd:?}");
        // TODO: `blocking_send()`? Would need to check for a runtime and use that if it exists.
        //        Can save clones when this function is called from sync environment.
        let tx = tx.clone();
        // state.runtime.spawn(async move { tx.send(cmd).await });
        state.rt_handle.spawn(async move { tx.send(cmd).await });

        Ok(())
    }
}

impl CastingDevice for AirPlay1Device {
    fn casting_protocol(&self) -> ProtocolType {
        ProtocolType::AirPlay
    }

    fn is_ready(&self) -> bool {
        let state = self.state.lock().unwrap();
        !state.addresses.is_empty() && state.port > 0 && !state.name.is_empty()
    }

    fn supports_feature(&self, feature: DeviceFeature) -> bool {
        Self::SUPPORTED_FEATURES.contains(&feature)
    }

    fn name(&self) -> String {
        let state = self.state.lock().unwrap();
        state.name.clone()
    }

    fn set_name(&self, name: String) {
        let mut state = self.state.lock().unwrap();
        state.name = name;
    }

    fn stop_casting(&self) -> Result<(), CastingDeviceError> {
        if let Err(err) = self.stop_playback() {
            error!("Failed to stop playback: {err}");
        }
        info!("Stopping active device because stopCasting was called.");
        self.disconnect()
    }

    fn seek(&self, _time_seconds: f64) -> Result<(), CastingDeviceError> {
        // self.send_command(Command::Seek(time_seconds))
        // TODO
        Ok(())
    }

    fn stop_playback(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::StopPlayback)
    }

    fn pause_playback(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::PausePlayback)
    }

    fn resume_playback(&self) -> Result<(), CastingDeviceError> {
        self.send_command(Command::ResumePlayback)
    }

    // fn load_video(
    //     &self,
    //     _stream_type: String,
    //     _content_type: String,
    //     _content_id: String,
    //     _resume_position: f64,
    //     _duration: f64,
    //     _speed: Option<f64>,
    // ) -> Result<(), CastingDeviceError> {
    //     todo!()
    // }

    fn load_url(
        &self,
        content_type: String,
        url: String,
        resume_position: Option<f64>,
        speed: Option<f64>,
    ) -> Result<(), CastingDeviceError> {
        self.send_command(Command::LoadUrl {
            content_type,
            url,
            resume_position: resume_position.unwrap_or(0.0),
            speed,
        })
    }

    fn load_video(
        &self,
        content_type: String,
        url: String,
        resume_position: f64,
        speed: Option<f64>,
    ) -> Result<(), CastingDeviceError> {
        self.load_url(content_type, url, Some(resume_position), speed)
    }

    fn load_image(&self, _content_type: String, _url: String) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn load_content(
        &self,
        _content_type: String,
        _content: String,
        _resume_position: f64,
        _duration: f64,
        _speed: Option<f64>,
    ) -> Result<(), CastingDeviceError> {
        todo!()
    }

    fn change_volume(&self, _volume: f64) -> Result<(), CastingDeviceError> {
        // TODO: not supported
        Ok(())
    }

    fn change_speed(&self, speed: f64) -> Result<(), CastingDeviceError> {
        self.send_command(Command::ChangeSpeed(speed))
    }

    fn disconnect(&self) -> Result<(), CastingDeviceError> {
        if let Err(err) = self.send_command(Command::Quit) {
            error!("Failed to stop worker: {err}");
        }
        let mut state = self.state.lock().unwrap();
        state.command_tx = None;
        state.started = false;
        Ok(())
    }

    fn connect(
        &self,
        event_handler: Arc<dyn DeviceEventHandler>,
    ) -> Result<(), CastingDeviceError> {
        let mut state = self.state.lock().unwrap();
        if state.started {
            return Err(CastingDeviceError::DeviceAlreadyStarted);
        }

        let addrs = crate::casting_device::ips_to_socket_addrs(&state.addresses, state.port);
        if addrs.is_empty() {
            return Err(CastingDeviceError::MissingAddresses);
        }

        state.started = true;
        info!("Starting with address list: {addrs:?}...");

        let (tx, rx) = tokio::sync::mpsc::channel::<Command>(50);
        state.command_tx = Some(tx);

        state
            .rt_handle
            .spawn(InnerDevice::new(event_handler).work(addrs, rx));

        Ok(())
    }

    fn get_device_info(&self) -> DeviceInfo {
        let state = self.state.lock().unwrap();
        DeviceInfo {
            name: state.name.clone(),
            r#type: ProtocolType::AirPlay,
            addresses: state.addresses.clone(),
            port: state.port,
        }
    }

    fn get_addresses(&self) -> Vec<IpAddr> {
        let state = self.state.lock().unwrap();
        state.addresses.clone()
    }

    fn subscribe_event(
        &self,
        _group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError> {
        Err(CastingDeviceError::UnsupportedSubscription)
    }

    fn unsubscribe_event(
        &self,
        _group: GenericEventSubscriptionGroup,
    ) -> Result<(), CastingDeviceError> {
        Err(CastingDeviceError::UnsupportedSubscription)
    }

    fn set_addresses(&self, addrs: Vec<IpAddr>) {
        let mut state = self.state.lock().unwrap();
        state.addresses = addrs;
    }

    fn get_port(&self) -> u16 {
        let state = self.state.lock().unwrap();
        state.port
    }

    fn set_port(&self, port: u16) {
        let mut state = self.state.lock().unwrap();
        state.port = port;
    }
}
