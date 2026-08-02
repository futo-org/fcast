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

    /// Forward a player event. `generation` is the load generation the event belongs to (`None` for
    /// app-internal events not tied to a load); the application drops load-scoped events from
    /// superseded generations.
    pub fn player(&self, msg: crate::player::PlayerEvent, generation: Option<u64>) {
        self.send(Message::NewPlayerEvent {
            event: msg,
            generation,
        });
    }

    pub fn image(&self, msg: crate::image::Event) {
        self.send(Message::Image(msg));
    }

    pub fn queue_cache(&self, msg: crate::queue_cache::Event) {
        self.send(Message::QueueCache(msg));
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
    /// A mirror video stream was set up, the receiver should start playing the
    /// `airplay://mirror/<id>` source.
    MirrorStarted {
        stream_connection_id: u64,
    },
    /// A mirror session ended (TEARDOWN or sender disconnect), the receiver
    /// should stop playback if this is the session currently playing.
    MirrorStopped {
        stream_connection_id: u64,
    },
    /// The client stopped sending video (screen locked/asleep), the receiver
    /// should pause playback of this session.
    MirrorPaused {
        stream_connection_id: u64,
    },
    /// The client resumed sending video after a pause, the receiver should
    /// resume playback of this session.
    MirrorResumed {
        stream_connection_id: u64,
    },
    /// The client changed the volume (SET_PARAMETER). `volume` is the linear
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

/// User's choice from the port-conflict dialog. `Quit` is not represented here:
/// the Quit button ends the Slint loop, which drives the normal `Message::Quit`
/// shutdown.
#[derive(Debug, Clone, Copy)]
pub enum PortConflictChoice {
    /// Re-attempt binding the default FCast port (after the user closes the
    /// other instance).
    Retry,
    /// Bind an ephemeral port instead and re-advertise it over mDNS.
    UseDifferentPort,
}

#[derive(Debug)]
pub enum Message {
    Quit,
    /// Sent by the port-conflict dialog while the receiver is still trying to
    /// acquire its listening port (see `Application::resolve_listen_port`).
    PortConflictChoice(PortConflictChoice),
    SessionFinished,
    SeekPercent(f32),
    ToggleDebug,
    NewPlayerEvent {
        event: player::PlayerEvent,
        /// The load generation the event belongs to (see `fcastplaybin::FcastPlaybin::load_async`);
        /// `None` for app-internal events not tied to a load.
        generation: Option<u64>,
    },
    Op {
        origin: PacketOrigin,
        op: crate::Operation,
    },
    Image(crate::image::Event),
    QueueCache(crate::queue_cache::Event),
    Mdns(Mdns),
    PlaylistDataResult {
        play_message: Option<fcast_protocol::v3::PlayMessage>,
    },
    MediaItemFinish(MediaItemId),
    SelectTrack {
        id: i32,
        variant: UiMediaTrackType,
    },
    /// A boolean setting was toggled in the settings drawer. `key` is a dotted
    /// `section.key` (see `Config::set_bool`). Autosaved to the config file.
    SetConfigBool {
        key: String,
        value: bool,
    },
    /// A string setting was committed in the settings drawer (see
    /// `Config::set_string`). Autosaved to the config file.
    SetConfigString {
        key: String,
        value: String,
    },
    ShouldSetLoadingStatus(MediaItemId),
    /// Bounded wait for a parked `AddSubtitleSource`: the op arrived while the
    /// load it targets was still in flight (`Application::is_loading_media`,
    /// a sender may send `Load` and `AddSubtitleSource` back to back), or
    /// after the load but before the pipeline could answer the seekability
    /// query (`Player::seekable_known`). If neither has settled when this
    /// fires, the parked adds are rejected with `InvalidState`.
    PendingSubtitleAddCheck {
        item: MediaItemId,
        epoch: u64,
    },
    /// Bounded wait for a parked `Seek` (the unresolved-seekability window
    /// `PendingSubtitleAddCheck` also covers). If still unresolved when this
    /// fires, the parked seek is dropped (matching the old silent behavior
    /// for unseekable streams).
    PendingSeekCheck {
        epoch: u64,
    },
    /// The seek broadcast debounce expired (see `Application::seek_quiet`).
    SeekQuietTimeout {
        epoch: u64,
    },
    /// DIAGNOSTIC (load-stall investigation): a bounded wait after a pipeline
    /// load. If the pipeline still has not reached a steady PAUSED when this
    /// fires (a selected stream's pad never routed), dump why (see
    /// `Player::log_load_stall_diagnostics`). Diagnostics only, no recovery.
    LoadStallCheck {
        item: MediaItemId,
        epoch: u64,
    },
    Raop(Raop),
    #[cfg(feature = "airplay")]
    AirPlay(AirPlay),
    /// The inspector was opened or closed. Gates inspector work and resets its
    /// per-session sampling state on close.
    InspectorActive(bool),
    InspectorRefresh,
    /// One bitrate sample while the inspector is open (driven by its timer).
    InspectorBitrateTick,
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
