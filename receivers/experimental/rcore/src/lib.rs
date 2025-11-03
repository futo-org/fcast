use anyhow::{Context, Result, bail};
use common::Packet;
use fcast_protocol::{
    SeekMessage, SetSpeedMessage, SetVolumeMessage, VolumeUpdateMessage,
    v2::{PlayMessage, PlaybackUpdateMessage},
    v3::PlaybackState,
};
use gst::glib::base64_encode;
use log::{debug, error, warn};
use pipeline::Pipeline;
use session::{Session, SessionId};
use tokio::net::TcpListener;
use tokio::sync::mpsc::{self, Receiver, Sender};
use tokio::sync::{broadcast, oneshot};

use std::net::{IpAddr, Ipv4Addr};
use std::sync::Arc;
use std::time::{Duration, Instant};

pub use slint;

pub mod fcastwhepsrcbin;
pub mod pipeline;
pub mod session;
pub mod video;

pub mod common {
    use std::sync::OnceLock;
    use tokio::runtime::Runtime;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::tcp::{ReadHalf, WriteHalf},
    };

    pub const HEADER_BUFFER_SIZE: usize = 5;
    pub const MAX_BODY_SIZE: u32 = 32000 - 1;

    pub fn runtime() -> &'static Runtime {
        static RUNTIME: OnceLock<Runtime> = OnceLock::new();
        RUNTIME.get_or_init(|| Runtime::new().unwrap())
    }

    pub fn default_log_level() -> log::LevelFilter {
        if cfg!(debug_assertions) {
            log::LevelFilter::Debug
        } else {
            log::LevelFilter::Info
        }
    }

    use anyhow::{Context, bail};

    use fcast_protocol::{
        Opcode, PlaybackErrorMessage, SeekMessage, SetSpeedMessage, SetVolumeMessage,
        VersionMessage, VolumeUpdateMessage,
        v2::{PlayMessage, PlaybackUpdateMessage},
    };

    #[derive(Debug, PartialEq)]
    pub struct Header {
        pub size: u32,
        pub opcode: Opcode,
    }

    impl Header {
        pub fn new(opcode: Opcode, size: u32) -> Self {
            Self {
                size: size + 1,
                opcode,
            }
        }

        pub fn decode(buf: [u8; 5]) -> Self {
            Self {
                size: u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) - 1,
                opcode: Opcode::try_from(buf[4]).unwrap(),
            }
        }

        pub fn encode(&self) -> [u8; 5] {
            let size_slice = u32::to_le_bytes(self.size);
            [
                size_slice[0],
                size_slice[1],
                size_slice[2],
                size_slice[3],
                self.opcode as u8,
            ]
        }
    }

    #[derive(Debug, PartialEq)]
    pub enum Packet {
        None,
        Play(PlayMessage),
        Pause,
        Resume,
        Stop,
        Seek(SeekMessage),
        PlaybackUpdate(PlaybackUpdateMessage),
        VolumeUpdate(VolumeUpdateMessage),
        SetVolume(SetVolumeMessage),
        PlaybackError(PlaybackErrorMessage),
        SetSpeed(SetSpeedMessage),
        Version(VersionMessage),
        Ping,
        Pong,
    }

    impl From<&Packet> for Opcode {
        fn from(value: &Packet) -> Self {
            match value {
                Packet::None => Opcode::None,
                Packet::Play(_) => Opcode::Play,
                Packet::Pause => Opcode::Pause,
                Packet::Resume => Opcode::Resume,
                Packet::Stop => Opcode::Stop,
                Packet::Seek(_) => Opcode::Seek,
                Packet::PlaybackUpdate(_) => Opcode::PlaybackUpdate,
                Packet::VolumeUpdate(_) => Opcode::VolumeUpdate,
                Packet::SetVolume(_) => Opcode::SetVolume,
                Packet::PlaybackError(_) => Opcode::PlaybackError,
                Packet::SetSpeed(_) => Opcode::SetSpeed,
                Packet::Version(_) => Opcode::Version,
                Packet::Ping => Opcode::Ping,
                Packet::Pong => Opcode::Pong,
            }
        }
    }

    impl From<PlaybackErrorMessage> for Packet {
        fn from(value: PlaybackErrorMessage) -> Packet {
            Packet::PlaybackError(value)
        }
    }

    impl From<PlaybackUpdateMessage> for Packet {
        fn from(value: PlaybackUpdateMessage) -> Self {
            Self::PlaybackUpdate(value)
        }
    }

    impl From<VolumeUpdateMessage> for Packet {
        fn from(value: VolumeUpdateMessage) -> Self {
            Packet::VolumeUpdate(value)
        }
    }

    impl From<PlayMessage> for Packet {
        fn from(value: PlayMessage) -> Self {
            Packet::Play(value)
        }
    }

    impl Packet {
        pub fn decode(header: Header, body: &str) -> anyhow::Result<Self> {
            Ok(match header.opcode {
                Opcode::None => Self::None,
                Opcode::Play => Self::Play(serde_json::from_str(body).context("Play")?),
                Opcode::Pause => Self::Pause,
                Opcode::Resume => Self::Resume,
                Opcode::Stop => Self::Stop,
                Opcode::Seek => Self::Seek(serde_json::from_str(body)?),
                Opcode::PlaybackUpdate => {
                    Self::PlaybackUpdate(serde_json::from_str(body).context("PlaybackUpdate")?)
                }
                Opcode::VolumeUpdate => {
                    Self::VolumeUpdate(serde_json::from_str(body).context("VolumeUpdate")?)
                }
                Opcode::SetVolume => {
                    Self::SetVolume(serde_json::from_str(body).context("SetVolume")?)
                }
                Opcode::PlaybackError => {
                    Self::PlaybackError(serde_json::from_str(body).context("PlaybackError")?)
                }
                Opcode::SetSpeed => Self::SetSpeed(serde_json::from_str(body).context("SetSpeed")?),
                Opcode::Version => Self::Version(serde_json::from_str(body).context("Version")?),
                Opcode::Ping => Self::Ping,
                Opcode::Pong => Self::Pong,
                _ => bail!("Unsupported opcode: {:?}", header.opcode),
            })
        }

        pub fn encode(&self) -> Vec<u8> {
            let body = match self {
                Packet::Play(play_message) => {
                    serde_json::to_string(&play_message).unwrap().into_bytes()
                }
                Packet::Seek(seek_message) => {
                    serde_json::to_string(&seek_message).unwrap().into_bytes()
                }
                Packet::PlaybackUpdate(playback_update_message) => {
                    serde_json::to_string(&playback_update_message)
                        .unwrap()
                        .into_bytes()
                }
                Packet::VolumeUpdate(volume_update_message) => {
                    serde_json::to_string(&volume_update_message)
                        .unwrap()
                        .into_bytes()
                }
                Packet::SetVolume(set_volume_message) => serde_json::to_string(&set_volume_message)
                    .unwrap()
                    .into_bytes(),
                Packet::PlaybackError(playback_error_message) => {
                    serde_json::to_string(&playback_error_message)
                        .unwrap()
                        .into_bytes()
                }
                Packet::SetSpeed(set_speed_message) => serde_json::to_string(&set_speed_message)
                    .unwrap()
                    .into_bytes(),
                Packet::Version(version_message) => serde_json::to_string(&version_message)
                    .unwrap()
                    .into_bytes(),
                _ => Vec::new(),
            };

            assert!(body.len() < 32 * 1000);
            let header = Header::new(self.into(), body.len() as u32).encode();
            let mut pack = header.to_vec();
            pack.extend_from_slice(&body);
            pack
        }
    }

    /// Attempt to read and decode FCast packet from `stream`.
    pub async fn read_packet(stream: &mut ReadHalf<'_>) -> anyhow::Result<Packet> {
        let mut header_buf: [u8; HEADER_BUFFER_SIZE] = [0; HEADER_BUFFER_SIZE];

        stream.read_exact(&mut header_buf).await?;

        let header = Header::decode(header_buf);

        let mut body_string = String::new();

        if header.size > 0 {
            let mut body_buf = vec![0; header.size as usize];
            stream.read_exact(&mut body_buf).await?;
            body_string = String::from_utf8(body_buf)?;
        }

        Packet::decode(header, &body_string)
    }

    pub async fn write_packet(stream: &mut WriteHalf<'_>, packet: Packet) -> anyhow::Result<()> {
        let bytes = packet.encode();
        stream.write_all(&bytes).await?;
        Ok(())
    }
}

#[derive(Debug)]
pub enum Event {
    Pause,
    Play(PlayMessage),
    Resume,
    Stop,
    SetSpeed(SetSpeedMessage),
    Seek(SeekMessage),
    SetVolume(SetVolumeMessage),
    Quit,
    PipelineEos,
    PipelineError,
    SessionFinished,
    ResumeOrPause,
    SeekPercent(f32),
    PipelineStateChanged(gst::State),
    ToggleDebug,
}

#[macro_export]
macro_rules! log_if_err {
    ($res:expr) => {
        if let Err(err) = $res {
            error!("{err}");
        }
    };
}

const FCAST_TCP_PORT: u16 = 46899;
const SENDER_UPDATE_INTERVAL: Duration = Duration::from_secs(1);

slint::include_modules!();

fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64
}

struct Application {
    pipeline: Pipeline,
    event_tx: Sender<Event>,
    ui_weak: slint::Weak<MainWindow>,
    updates_tx: broadcast::Sender<Arc<Vec<u8>>>,
    mdns: mdns_sd::ServiceDaemon,
    last_sent_update: Instant,
    debug_mode: bool,
}

fn get_all_available_addrs_ignore_v6_and_localhost() -> Result<Vec<Ipv4Addr>> {
    let mut ips: Vec<Ipv4Addr> = Vec::new();
    for iface in getifaddrs::getifaddrs()? {
        if let Some(ip_addr) = iface.address.ip_addr() {
            match ip_addr {
                std::net::IpAddr::V4(v4) if !v4.is_loopback() => ips.push(v4),
                std::net::IpAddr::V6(v6) if !v6.is_loopback() => {
                    debug!("Ignoring IPv6 address ({v6:?})")
                }
                _ => debug!("Ignoring loopback IP address ({ip_addr:?})"),
            }
        }
    }
    Ok(ips)
}

impl Application {
    pub async fn new(
        appsink: gst::Element,
        event_tx: Sender<Event>,
        ui_weak: slint::Weak<MainWindow>,
    ) -> Result<Self> {
        let pipeline = Pipeline::new(appsink, event_tx.clone()).await?;
        let (updates_tx, _) = broadcast::channel(10);

        // TODO: IPv6?
        let mdns = {
            let daemon = mdns_sd::ServiceDaemon::new()?;

            let ips: Vec<IpAddr> = get_all_available_addrs_ignore_v6_and_localhost()?
                .into_iter()
                .map(|addr| IpAddr::V4(addr))
                .collect::<Vec<IpAddr>>();

            if ips.is_empty() {
                bail!("No addresses available to use for mDNS discovery");
            }

            let name = format!(
                "OpenMirroring-{}",
                gethostname::gethostname().to_string_lossy()
            );

            let service = mdns_sd::ServiceInfo::new(
                "_fcast._tcp.local.",
                &name,
                &format!("{name}.local."),
                ips.as_slice(),
                FCAST_TCP_PORT,
                None::<std::collections::HashMap<String, String>>,
            )?;

            daemon.register(service)?;

            daemon
        };

        Ok(Self {
            pipeline,
            event_tx,
            ui_weak,
            updates_tx,
            mdns,
            last_sent_update: Instant::now() - SENDER_UPDATE_INTERVAL,
            debug_mode: false,
        })
    }

    fn notify_updates(&mut self) -> Result<()> {
        let pipeline_playback_state = match self.pipeline.get_playback_state() {
            Ok(s) => s,
            Err(err) => {
                error!("Failed to get playback state: {err}");
                return Ok(());
            }
        };

        let progress_str = {
            let update = &pipeline_playback_state;
            let time_secs = update.time % 60.0;
            let time_mins = (update.time / 60.0) % 60.0;
            let time_hours = update.time / 60.0 / 60.0;

            let duration_secs = update.duration % 60.0;
            let duration_mins = (update.duration / 60.0) % 60.0;
            let duration_hours = update.duration / 60.0 / 60.0;

            format!(
                "{:02}:{:02}:{:02} / {:02}:{:02}:{:02}",
                time_hours as u32,
                time_mins as u32,
                time_secs as u32,
                duration_hours as u32,
                duration_mins as u32,
                duration_secs as u32,
            )
        };
        let progress_percent =
            (pipeline_playback_state.time / pipeline_playback_state.duration * 100.0) as f32;
        let playback_state = {
            let is_live = self.pipeline.is_live();
            // use fcast_lib::models::PlaybackState;
            match pipeline_playback_state.state {
                PlaybackState::Playing | PlaybackState::Paused if is_live => GuiPlaybackState::Live,
                PlaybackState::Playing => GuiPlaybackState::Playing,
                PlaybackState::Paused => GuiPlaybackState::Paused,
                PlaybackState::Idle => GuiPlaybackState::Loading,
            }
        };

        self.ui_weak.upgrade_in_event_loop(move |ui| {
            ui.global::<Bridge>()
                .set_progress_label(progress_str.into());
            ui.invoke_update_progress_percent(progress_percent);
            ui.global::<Bridge>().set_playback_state(playback_state);
        })?;

        if self.updates_tx.receiver_count() > 0
            && self.last_sent_update.elapsed() >= SENDER_UPDATE_INTERVAL
        {
            let update = PlaybackUpdateMessage {
                generation_time: current_time_millis(),
                time: pipeline_playback_state.time,
                duration: pipeline_playback_state.duration,
                state: pipeline_playback_state.state as u8,
                speed: pipeline_playback_state.speed,
            };
            debug!("Sending update ({update:?})");
            self.updates_tx
                .send(Arc::new(Packet::from(update).encode()))?;
            self.last_sent_update = Instant::now();
        }

        Ok(())
    }

    /// Returns `true` if the event loop should exit
    async fn handle_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::SessionFinished => {
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>().invoke_device_disconnected();
                })?;
            }
            Event::Pause => {
                self.pipeline.pause().context("failed to pause pipeline")?;
                self.notify_updates()
                    .context("failed to notify about updates")?;
            }
            Event::Play(play_message) => {
                let Some(mut url) = play_message.url else {
                    error!("Play message does not contain a URL");
                    return Ok(false);
                };
                if play_message.container == "application/x-whep" {
                    url = url.replace("http://", "fcastwhep://");
                }

                if let Err(err) = self.pipeline.set_playback_uri(&url) {
                    use pipeline::SetPlaybackUriError;
                    match err {
                        SetPlaybackUriError::PipelineStateChange(state_change_error) => {
                            return Err(state_change_error.into());
                        }
                        _ => {
                            error!("Failed to set playback URI: {err}");
                            return Ok(false);
                        }
                    }
                }
                if let Err(err) = self.pipeline.play_or_resume() {
                    error!("Failed to play_or_resume pipeline: {err}");
                } else {
                    self.ui_weak.upgrade_in_event_loop(|ui| {
                        ui.invoke_playback_started();
                        ui.global::<Bridge>().set_app_state(AppState::Playing);
                    })?;
                    self.notify_updates()
                        .context("failed to notify about updates")?;
                }
            }
            Event::Resume => self
                .pipeline
                .play_or_resume()
                .context("failed to play or resume pipeline")?,
            Event::ResumeOrPause => {
                let Some(playing) = self.pipeline.is_playing() else {
                    warn!("Pipeline is not in a state that can be resumed or paused");
                    return Ok(false);
                };
                if let Err(err) = if playing {
                    self.pipeline.pause()
                } else {
                    self.pipeline.play_or_resume()
                } {
                    error!("Failed to play or resume: {err}");
                }
                self.notify_updates()
                    .context("failed to notify about updates")?;
            }
            Event::Stop => {
                self.pipeline.stop()?;
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.invoke_playback_stopped();
                    ui.global::<Bridge>().set_app_state(AppState::Idle);
                })?;
            }
            Event::SetSpeed(set_speed_message) => self
                .pipeline
                .set_speed(set_speed_message.speed)
                .context("failed to set speed")?,
            Event::Seek(seek_message) => {
                if let Err(err) = self.pipeline.seek(seek_message.time) {
                    error!("Seek error: {err}");
                    return Ok(false);
                }
                self.notify_updates()?;
            }
            Event::SeekPercent(percent) => {
                let Some(duration) = self.pipeline.get_duration() else {
                    error!("Failed to get playback duration");
                    return Ok(false);
                };
                if duration.is_zero() {
                    error!("Cannot seek when the duration is zero");
                    return Ok(false);
                }
                let seek_to = duration.seconds_f64() * (percent as f64 / 100.0);
                if let Err(err) = self.pipeline.seek(seek_to) {
                    error!("Seek error: {err}");
                    return Ok(false);
                }
                self.notify_updates()?;
            }
            Event::SetVolume(set_volume_message) => {
                self.pipeline.set_volume(set_volume_message.volume);
                self.ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.global::<Bridge>()
                        .set_volume(set_volume_message.volume as f32);
                })?;
                if self.updates_tx.receiver_count() > 0 {
                    let update = VolumeUpdateMessage {
                        generation_time: current_time_millis(),
                        volume: set_volume_message.volume,
                    };
                    debug!("Sending update ({update:?})");
                    self.updates_tx
                        .send(Arc::new(Packet::from(update).encode()))?;
                    self.last_sent_update = Instant::now();
                }
            }
            Event::Quit => return Ok(true),
            Event::PipelineEos => {
                debug!("Pipeline reached EOS");
                // self.pipeline.stop()?;
                // self.ui_weak.upgrade_in_event_loop(|ui| {
                //     ui.invoke_playback_stopped();
                // })?;
            }
            Event::PipelineError => {
                self.pipeline.stop().context("failed to stop pipeline")?;
                // TODO: send error message to sessions
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.invoke_playback_stopped_with_error("Error unclear (todo)".into());
                })?;
            }
            Event::PipelineStateChanged(state) => match state {
                gst::State::Paused | gst::State::Playing => self
                    .notify_updates()
                    .context("failed to notify about updates")?,
                _ => (),
            },
            Event::ToggleDebug => self.debug_mode = !self.debug_mode,
        }

        Ok(false)
    }

    pub async fn run_event_loop(
        mut self,
        mut event_rx: Receiver<Event>,
        fin_tx: oneshot::Sender<()>,
    ) -> Result<()> {
        let dispatch_listener = TcpListener::bind("0.0.0.0:46899").await?;

        let mut session_id: SessionId = 0;
        let mut update_interval = tokio::time::interval(Duration::from_millis(100));

        let event_tx_cl = self.event_tx.clone();
        // tokio::spawn(async move {
        //     tokio::time::sleep(Duration::from_millis(1000)).await;
        //     event_tx_cl.send(Event::Play(PlayMessage {
        //         // container: "video/mp4".to_string(),
        //         container: "video/mkv".to_string(),
        //         // url: Some("http://commondatastorage.googleapis.com/gtv-videos-bucket/sample/BigBuckBunny.mp4".to_string()),
        //         // url: Some("file:///home/merb/Videos/4K_sample_video.webm".to_string()),
        //         // url: Some("file:///home/merb/Videos/Ocean's Eleven (2001) (1080p BluRay x265 HEVC 10bit AAC 5.1 Tigole)/Ocean's Eleven (2001) (1080p BluRay x265 10bit Tigole).mkv".to_string()),
        //         url: Some("file:///home/merb/Videos/Top.Gun.1986.1080p.WEBRip.Regraded.Open.Matte.10Bit.AV1.DD.5.1.ViTO.mkv".to_string()),
        //         content: None,
        //         time: Some(0.0),
        //         speed: Some(1.0),
        //         headers: None,
        //     })).await.unwrap();
        // });

        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    if let Some(event) = event {
                        debug!("Got event: {event:?}");
                        match self.handle_event(event).await {
                            Ok(true) => break,
                            Err(err) => error!("Handle event error: {err}"),
                            _ => (),
                        }
                    } else {
                        break;
                    }
                }
                _ = update_interval.tick() => {
                    // if self.pipeline.is_playing() == Some(true) {
                        self.notify_updates()?;
                    // }
                }
                session = dispatch_listener.accept() => {
                    let (stream, _) = session?;

                    debug!("New connection id={session_id}");

                    tokio::spawn({
                        let id = session_id;
                        let event_tx = self.event_tx.clone();
                        let updates_rx = self.updates_tx.subscribe();
                        async move {
                            if let Err(err) =
                                Session::new(stream, id).run(updates_rx, &event_tx).await
                            {
                                error!("Session exited with error: {err}");
                            }

                            if let Err(err) = event_tx.send(Event::SessionFinished).await {
                                error!("Failed to send SessionFinished: {err}");
                            }
                        }
                    });

                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        ui.invoke_device_connected();
                    })?;

                    session_id += 1;
                }
            }
        }

        self.pipeline.stop()?;

        debug!("Quitting");

        if fin_tx.send(()).is_err() {
            bail!("Failed to send fin");
        }

        self.mdns.shutdown()?;

        Ok(())
    }
}

#[derive(clap::Parser)]
#[command(version)]
struct CliArgs {
    // Disable animated background. Reduces resource usage
    // #[arg(short = 'b', long, default_value_t = false)]
    // no_background: bool,
}

/// Run the main app.
///
/// Slint and friends are assumed to be initialized by the platform specific target.
pub fn run() -> Result<()> {
    gst::init()?;

    fcastwhepsrcbin::plugin_init()?;

    let ips: Vec<Ipv4Addr> = get_all_available_addrs_ignore_v6_and_localhost()?;

    // TODO: fix, base64? format?
    let device_url = format!(
        "fcast://r/{}",
        base64_encode(
            format!(
                r#"{{"name":"Test","addresses":[{}],"services":[{{"port":46899,"type":1}}]}}"#,
                ips.iter()
                    .map(|addr| format!("\"{}\"", addr))
                    .collect::<Vec<String>>()
                    .join(","),
            )
            .as_bytes()
        )
        .as_str(),
    );

    let qr_code = qrcode::QrCode::new(device_url)?;
    let qr_code_dims = qr_code.width() as u32;
    let qr_code_colors = qr_code.into_colors();
    let mut qr_code_pixels =
        slint::SharedPixelBuffer::<slint::Rgb8Pixel>::new(qr_code_dims, qr_code_dims);
    qr_code_pixels.make_mut_slice().copy_from_slice(
        &qr_code_colors
            .into_iter()
            .map(|px| match px {
                qrcode::Color::Light => slint::Rgb8Pixel::new(0xFF, 0xFF, 0xFF),
                qrcode::Color::Dark => slint::Rgb8Pixel::new(0x0, 0x0, 0x0),
            })
            .collect::<Vec<slint::Rgb8Pixel>>(),
    );
    let qr_code_image = slint::Image::from_rgb8(qr_code_pixels);

    let (event_tx, event_rx) = mpsc::channel::<Event>(100);
    let (fin_tx, fin_rx) = oneshot::channel::<()>();

    let mut slint_sink = video::SlintOpenGLSink::new()?;
    let slint_appsink = slint_sink.video_sink();

    let ui = MainWindow::new()?;

    ui.global::<Bridge>().set_qr_code(qr_code_image);

    #[cfg(debug_assertions)]
    ui.global::<Bridge>().set_is_debugging(true);

    ui.window().set_rendering_notifier({
        let ui_weak = ui.as_weak();

        move |state, graphics_api| {
            if let slint::RenderingState::RenderingSetup = state {
                debug!("Got graphics API: {graphics_api:?}");
                let ui_weak = ui_weak.clone();

                slint_sink
                    .connect(graphics_api, move || {
                        ui_weak
                            .upgrade_in_event_loop(move |ui| {
                                ui.window().request_redraw();
                            })
                            .unwrap();
                    })
                    .unwrap();
            } else if let slint::RenderingState::BeforeRendering = state {
                let Some(ui) = ui_weak.upgrade() else {
                    error!("Failed to upgrade ui");
                    return;
                };

                // TODO: don't render the video when the frame is from the old source (i.e. playback was
                //       stopped, then new source was set and for a brief moment the last displayed frame
                //       of the old source becomes visible.)
                if ui.global::<Bridge>().get_playing() {
                    let Some((texture_id, size)) = slint_sink.fetch_next_frame_as_texture() else {
                        return;
                    };
                    let frame = unsafe {
                        slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                            texture_id,
                            size.into(),
                        )
                        .build()
                    };
                    ui.global::<Bridge>().set_video_frame(frame);
                }
            }
        }
    })?;

    common::runtime().spawn({
        let ui_weak = ui.as_weak();
        let event_tx = event_tx.clone();
        async move {
            Application::new(slint_appsink, event_tx, ui_weak)
                .await
                .unwrap()
                .run_event_loop(event_rx, fin_tx)
                .await
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_resume_or_pause({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.blocking_send(Event::ResumeOrPause));
        }
    });

    ui.global::<Bridge>().on_seek_to_percent({
        let event_tx = event_tx.clone();
        move |percent| {
            log_if_err!(event_tx.blocking_send(Event::SeekPercent(percent)));
        }
    });

    ui.global::<Bridge>().on_toggle_fullscreen({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak
                .upgrade()
                .expect("callbacks always get called from the event loop");
            ui.window().set_fullscreen(!ui.window().is_fullscreen());
        }
    });

    ui.global::<Bridge>().on_set_volume({
        let event_tx = event_tx.clone();
        move |volume| {
            log_if_err!(event_tx.blocking_send(Event::SetVolume(SetVolumeMessage {
                volume: volume as f64,
            })));
        }
    });

    ui.global::<Bridge>().on_force_quit(move || {
        log_if_err!(slint::quit_event_loop());
    });

    ui.global::<Bridge>().on_debug_toggled({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.blocking_send(Event::ToggleDebug));
        }
    });

    ui.global::<Bridge>().set_label(format!("{ips:?}").into());

    ui.run()?;

    debug!("Shutting down...");

    common::runtime().block_on(async move {
        event_tx.send(Event::Quit).await.unwrap();
        fin_rx.await.unwrap();
    });

    Ok(())
}
