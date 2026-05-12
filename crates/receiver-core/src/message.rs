use std::net::IpAddr;

use tokio::sync::mpsc::UnboundedSender;
use tracing::error;

use crate::{MediaItemId, UiMediaTrackType, fcast::SessionId, player, raop};

#[derive(Clone, Debug)]
pub struct MessageSender(UnboundedSender<Message>);

impl MessageSender {
    pub fn new(tx: UnboundedSender<Message>) -> Self {
        Self(tx)
    }

    pub fn send(&self, msg: Message) {
        if let Err(err) = self.0.send(msg) {
            error!(?err, "Failed to send message");
        }
    }

    pub fn operation(&self, session_id: crate::SessionId, op: crate::Operation) {
        self.send(Message::Op { session_id, op })
    }

    pub fn raop(&self, msg: Raop) {
        self.send(Message::Raop(msg));
    }

    pub fn player(&self, msg: crate::player::PlayerEvent) {
        self.send(Message::NewPlayerEvent(msg));
    }

    pub fn image(&self, msg: crate::image::Event) {
        self.send(Message::Image(msg));
    }

    #[cfg(feature = "systray")]
    pub fn tray(&self, msg: Tray) {
        self.send(Message::Tray(msg));
    }

    #[cfg(not(target_os = "android"))]
    pub fn mdns(&self, msg: Mdns) {
        self.send(Message::Mdns(msg));
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    pub fn app_update(&self, msg: AppUpdate) {
        self.send(Message::AppUpdate(msg));
    }
}

#[derive(Debug)]
pub enum Mdns {
    NameSet(String),
    IpAdded(IpAddr),
    IpRemoved(IpAddr),
    SetIps(Vec<IpAddr>),
}

#[cfg(feature = "systray")]
#[derive(Debug)]
pub enum Tray {
    Quit,
    Toggle,
}

#[derive(Debug)]
pub enum Raop {
    ConfigAvailable(raop::Configuration),
    SenderConnected(tokio::net::TcpStream),
    SenderDisconnected,
    CoverArtSet(Vec<u8>),
    CoverArtRemoved,
    MetadataSet(raop::RaopMetadata),
    ProgressUpdate {
        position_sec: u64,
        duration_sec: u64,
    },
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug)]
pub enum AppUpdate {
    UpdateAvailable(app_updater::Release),
    UpdateApplication,
    RestartApp,
}

#[derive(Debug)]
pub enum Message {
    Quit,
    SessionFinished,
    ResumeOrPause,
    SeekPercent(f32),
    ToggleDebug,
    NewPlayerEvent(player::PlayerEvent),
    Op {
        /// The UI also sends operations with session_id == 0
        session_id: SessionId,
        op: crate::Operation,
    },
    Image(crate::image::Event),
    Mdns(Mdns),
    PlaylistDataResult {
        play_message: Option<fcast_protocol::v3::PlayMessage>,
    },
    MediaItemFinish(MediaItemId),
    SelectTrack {
        id: i32,
        variant: UiMediaTrackType,
    },
    #[cfg(feature = "systray")]
    Tray(Tray),
    ShouldSetLoadingStatus(MediaItemId),
    Raop(Raop),
    DumpPipeline,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    AppUpdate(AppUpdate),
    GuiWindowClosed(oneshot::Sender<()>),
}
