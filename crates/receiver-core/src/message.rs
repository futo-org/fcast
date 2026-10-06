use std::net::IpAddr;

use tokio::sync::mpsc::UnboundedSender;
use tracing::error;

#[cfg(feature = "airplay")]
use crate::airplay;
use crate::{MediaItemId, SenderId, application::PacketOrigin, player};
#[cfg(feature = "raop")]
use crate::raop;

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

    #[cfg(feature = "raop")]
    pub fn raop(&self, msg: Raop) {
        self.send(Message::Raop(msg));
    }

    #[cfg(feature = "airplay")]
    pub fn airplay(&self, msg: AirPlay) {
        self.send(Message::AirPlay(msg));
    }

    /// Forward a player event. `generation` is the load generation it belongs
    /// to (`None` for app-internal events); load-scoped events from
    /// superseded generations are dropped.
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

    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
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

/// Audio focus and routing events from the activity. The policy (what pauses,
/// what resumes) lives in the application, which knows the playback state;
/// the activity only forwards what the system told it.
#[cfg(target_os = "android")]
#[derive(Debug)]
pub enum AndroidAudio {
    /// Permanent focus loss. Pause and stay paused.
    Loss,
    /// Transient loss (call, alarm, navigation prompt), ducking folded in.
    /// Pause, and remember to resume on regain.
    TransientLoss,
    /// Focus regained after a transient loss.
    Gain,
    /// The output route is about to get loud (headphones unplugged).
    BecomingNoisy,
}

#[cfg(feature = "airplay")]
#[derive(Debug)]
pub enum AirPlay {
    ConfigAvailable(airplay::Configuration),
    SenderConnected(tokio::net::TcpStream),
    /// A mirror video stream was set up; play the `airplay://mirror/<id>`
    /// source.
    MirrorStarted {
        stream_connection_id: u64,
    },
    /// A mirror session ended (TEARDOWN or sender disconnect).
    MirrorStopped {
        stream_connection_id: u64,
    },
    /// The client stopped sending video (screen locked/asleep).
    MirrorPaused {
        stream_connection_id: u64,
    },
    /// The client resumed sending video after a pause.
    MirrorResumed {
        stream_connection_id: u64,
    },
    /// SET_PARAMETER volume change; `volume` is the linear GStreamer gain
    /// (`0.0`..=`1.0`).
    VolumeChanged {
        stream_connection_id: u64,
        volume: f32,
    },
}

#[cfg(feature = "raop")]
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

#[cfg(target_os = "android")]
#[derive(Debug)]
pub enum AppUpdate {
    /// `channel` is where it was found, the download comes from there too.
    UpdateAvailable {
        channel: String,
        release: crate::android_updater::Release,
    },
    UpdateApplication,
    /// The apk is on disk, or why it is not.
    Downloaded(Result<std::path::PathBuf, crate::android_updater::Error>),
    /// PackageInstaller refused it or the user cancelled, from the activity.
    InstallFailed(String),
}

/// User's choice from the port-conflict dialog. No `Quit` variant: the Quit
/// button ends the Slint loop, which drives the normal `Message::Quit`
/// shutdown.
#[derive(Debug, Clone, Copy)]
pub enum PortConflictChoice {
    /// Re-attempt binding the default FCast port.
    Retry,
    /// Bind an ephemeral port instead and re-advertise it over mDNS.
    UseDifferentPort,
}

#[derive(Debug)]
pub enum Message {
    Quit,
    /// Sent by the port-conflict dialog while the listening port is still being
    /// acquired.
    PortConflictChoice(PortConflictChoice),
    SessionFinished,
    SeekPercent(f32),
    ToggleDebug,
    NewPlayerEvent {
        event: player::PlayerEvent,
        /// The load generation the event belongs to; `None` for app-internal
        /// events.
        generation: Option<u64>,
    },
    Op {
        origin: PacketOrigin,
        op: crate::Operation,
    },
    Image(crate::image::Event),
    QueueCache(crate::queue_cache::Event),
    Mdns(Mdns),
    #[cfg(target_os = "android")]
    AndroidAudio(AndroidAudio),
    PlaylistDataResult {
        play_message: Option<fcast_protocol::v3::PlayMessage>,
    },
    MediaItemFinish(MediaItemId),
    SelectTrack {
        id: i32,
        variant: player::TrackKind,
    },
    /// A boolean setting was toggled in the settings drawer; `key` is a dotted
    /// `section.key`.
    SetConfigBool {
        key: String,
        value: bool,
    },
    /// A string setting was committed in the settings drawer; `key` is a dotted
    /// `section.key`.
    SetConfigString {
        key: String,
        value: String,
    },
    ShouldSetLoadingStatus(MediaItemId),
    /// Bounded wait for `AddSubtitleSource` parked on an in-flight load or
    /// unresolved seekability; on expiry the parked adds are rejected with
    /// `InvalidState`.
    PendingSubtitleAddCheck {
        item: MediaItemId,
        epoch: u64,
    },
    /// Bounded wait for a `Seek` parked on unresolved seekability; on expiry it
    /// is dropped.
    PendingSeekCheck {
        epoch: u64,
    },
    /// The seek broadcast debounce expired.
    SeekQuietTimeout {
        epoch: u64,
    },
    /// Diagnostics only, no recovery: bounded wait after a load, dumping why
    /// the pipeline never reached a steady PAUSED.
    LoadStallCheck {
        item: MediaItemId,
        epoch: u64,
    },
    /// One tick of the "server busy" backoff countdown shown by the GUI.
    SourceBackoffTick {
        epoch: u64,
    },
    #[cfg(feature = "raop")]
    Raop(Raop),
    #[cfg(feature = "airplay")]
    AirPlay(AirPlay),
    /// The inspector was opened or closed; closing resets its per-session
    /// sampling state.
    #[cfg(not(target_os = "android"))]
    InspectorActive(bool),
    #[cfg(not(target_os = "android"))]
    InspectorRefresh,
    /// android: the soft keyboard came up or went away, from the activity's
    /// window insets. A text field in edit mode folds when it goes.
    SoftKeyboardVisible(bool),
    /// One bitrate sample while the inspector is open (driven by its timer).
    #[cfg(not(target_os = "android"))]
    InspectorBitrateTick,
    #[cfg(any(target_os = "macos", target_os = "windows", target_os = "android"))]
    AppUpdate(AppUpdate),
    /// android: a UI came up and wants the current state, see
    /// [`crate::gui::GuiController::attach`].
    GuiAttached {
        tx: tokio::sync::mpsc::UnboundedSender<crate::gui::UpdateGuiCommand>,
        generation: u64,
    },
    /// android: that UI went away.
    GuiDetached {
        generation: u64,
    },
    /// The renderer is going away. `shows` is the count of window shows the
    /// GUI thread had carried out when it started, a newer request means the
    /// window is already being brought back.
    GuiWindowClosed {
        shows: u64,
        feedback: oneshot::Sender<()>,
    },
    FCastSenderDisconnect(SenderId),
    /// A sender said who it is (v3 initial message, v4 introduction).
    SenderIntroduced {
        sender_id: SenderId,
        info: crate::gui::SenderInfo,
    },
}

pub enum ReceiverToFCastSender {
    Error {
        kind: fcast_protocol::v4::flat::ErrorKind,
        packet_num: Option<u32>,
    },
    ProgressUpdate {
        pos: gst::ClockTime,
        dur: gst::ClockTime,
    },
}
