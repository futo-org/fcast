use std::net::IpAddr;

use tokio::sync::mpsc::UnboundedSender;
use tracing::error;

#[cfg(feature = "airplay")]
use crate::airplay;
use crate::{MediaItemId, SenderId, UiMediaTrackType, application::PacketOrigin, player, raop};

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

    pub fn operation(&self, origin: PacketOrigin, op: crate::Operation) {
        self.send(Message::Op { origin, op })
    }

    pub fn raop(&self, msg: Raop) {
        self.send(Message::Raop(msg));
    }

    #[cfg(feature = "airplay")]
    pub fn airplay(&self, msg: AirPlay) {
        self.send(Message::AirPlay(msg));
    }

    pub fn player(&self, msg: crate::player::PlayerEvent) {
        self.send(Message::NewPlayerEvent(msg));
    }

    pub fn image(&self, msg: crate::image::Event) {
        self.send(Message::Image(msg));
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

#[cfg(feature = "airplay")]
#[derive(Debug)]
pub enum AirPlay {
    ConfigAvailable(airplay::Configuration),
    SenderConnected(tokio::net::TcpStream),
    /// A mirror video stream was set up; the receiver should start playing the
    /// `airplay://mirror/<id>` source.
    MirrorStarted {
        stream_connection_id: u64,
    },
    /// A mirror session ended (TEARDOWN or sender disconnect); the receiver
    /// should stop playback if this is the session currently playing.
    MirrorStopped {
        stream_connection_id: u64,
    },
    /// The client stopped sending video (screen locked/asleep); the receiver
    /// should pause playback of this session.
    MirrorPaused {
        stream_connection_id: u64,
    },
    /// The client resumed sending video after a pause; the receiver should
    /// resume playback of this session.
    MirrorResumed {
        stream_connection_id: u64,
    },
    /// The client changed the volume (SET_PARAMETER); `volume` is the linear
    /// GStreamer gain (`0.0`..=`1.0`). Applied to the shared player, which now
    /// decodes the mirror audio.
    VolumeChanged {
        stream_connection_id: u64,
        volume: f32,
    },
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
    SeekPercent(f32),
    ToggleDebug,
    NewPlayerEvent(player::PlayerEvent),
    Op {
        origin: PacketOrigin,
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
    ShouldSetLoadingStatus(MediaItemId),
    Raop(Raop),
    #[cfg(feature = "airplay")]
    AirPlay(AirPlay),
    #[cfg(debug_assertions)]
    DumpPipeline,
    #[cfg(debug_assertions)]
    InspectorRefresh,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    AppUpdate(AppUpdate),
    GuiWindowClosed(oneshot::Sender<()>),
    FCastSenderDisconnect(SenderId),
}

pub(crate) enum ReceiverToFCastSender {
    Error {
        kind: fcast_protocol::v4::flat::ErrorKind,
        packet_num: Option<u32>,
    },
    ProgressUpdate {
        pos: gst::ClockTime,
        dur: gst::ClockTime,
    },
}
