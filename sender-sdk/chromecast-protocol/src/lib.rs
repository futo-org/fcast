use serde::{Deserialize, Serialize};

pub use prost;

pub mod protos {
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}

pub const HEARTBEAT_NAMESPACE: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const RECEIVER_NAMESPACE: &str = "urn:x-cast:com.google.cast.receiver";
pub const MEDIA_NAMESPACE: &str = "urn:x-cast:com.google.cast.media";

#[derive(Serialize, Deserialize, Debug)]
pub struct Volume {
    /// Current stream volume level as a value between 0.0 and 1.0 where 1.0 is the maximum volume.
    pub level: Option<f64>,
    /// Whether the Cast device is muted, independent of the volume level
    pub muted: Option<bool>,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum StreamType {
    #[serde(rename = "NONE")]
    None,
    #[serde(rename = "BUFFERED")]
    Buffered,
    #[serde(rename = "LIVE")]
    Live,
}

/// <https://developers.google.com/cast/docs/media/messages#MediaInformation>
#[derive(Serialize, Deserialize, Debug)]
pub struct MediaInformation {
    /// Service-specific identifier of the content currently loaded by the media player. This is a
    /// free form string and is specific to the application. In most cases, this will be the URL to
    /// the media, but the sender can choose to pass a string that the receiver can interpret
    /// properly. Max length: 1k
    #[serde(rename = "contentId")]
    pub content_id: String,
    #[serde(rename = "streamType")]
    pub stream_type: StreamType,
    /// MIME content type of the media being played
    #[serde(rename = "contentType")]
    pub content_type: String,
    // TODO: `metadata`
    // metadata: Option<...>
    /// Duration of the currently playing stream in seconds
    pub duration: Option<f64>,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum IdleReason {
    /// A sender requested to stop playback using the STOP command
    #[serde(rename = "CANCELLED")]
    Cancelled,
    /// A sender requested playing a different media using the LOAD command
    #[serde(rename = "INTERRUPTED")]
    Interrupted,
    /// The media playback completed
    #[serde(rename = "FINISHED")]
    Finished,
    /// The media was interrupted due to an error; for example, if the player could not download the
    /// media due to network issues
    #[serde(rename = "ERROR")]
    Error,
}

#[derive(Serialize, Deserialize, Debug)]
pub enum PlayerState {
    /// Player has not been loaded yet
    #[serde(rename = "IDLE")]
    Idle,
    /// Player is actively playing content
    #[serde(rename = "PLAYING")]
    Playing,
    /// Player is in PLAY mode but not actively playing content (currentTime is not changing)
    #[serde(rename = "BUFFERING")]
    Buffering,
    /// Player is paused
    #[serde(rename = "PAUSED")]
    Paused,
}

/// Describes the current status of the media artifact with respect to the session.
///
/// <https://developers.google.com/cast/docs/media/messages#MediaStatus>
#[derive(Serialize, Deserialize, Debug)]
pub struct MediaStatus {
    /// Unique ID for the playback of this specific session. This ID is set by the receiver at LOAD
    /// and can be used to identify a specific instance of a playback. For example, two playbacks
    /// of "Wish you were here" within the same session would each have a unique mediaSessionId.
    #[serde(rename = "mediaSessionId")]
    pub media_session_id: u64,
    /// optional (for status messages) Full description of the content that is being played back.
    /// Only be returned in a status messages if the MediaInformation has changed.
    pub media: Option<MediaInformation>,
    /// Indicates whether the media time is progressing, and at what rate. This is independent of the
    /// player state since the media time can stop in any state.
    /// 1.0 is regular time, 0.5 is slow motion
    #[serde(rename = "playbackRate")]
    pub playback_rate: f64,
    #[serde(rename = "playerState")]
    pub player_state: PlayerState,
    /// optional If the playerState is IDLE and the reason it became IDLE is known, this property is
    /// provided. If the player is IDLE because it just started, this property will not be provided;
    /// if the player is in any other state this property should not be provided.
    #[serde(rename = "idleReason")]
    pub idle_reason: Option<IdleReason>,
    /// The current position of the media player since the beginning of the content, in seconds.
    /// If this a live stream content, then this field represents the time in seconds from the
    /// beginning of the event that should be known to the player.
    #[serde(rename = "currentTime")]
    pub current_time: f64,
    /// Flags describing which media commands the media player supports:
    ///
    /// * 1  Pause
    /// * 2  Seek
    /// * 4  Stream volume
    /// * 8  Stream mute
    /// * 16  Skip forward
    /// * 32  Skip backward
    ///
    /// Combinations are described as summations; for example, Pause+Seek+StreamVolume+Mute == 15.
    #[serde(rename = "supportedMediaCommands")]
    pub supported_media_commands: u64,
    /// Stream volume
    pub volume: Volume,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct NamespaceMap {
    pub name: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Application {
    #[serde(rename = "appId")]
    pub app_id: String,
    #[serde(rename = "appType")]
    pub app_type: String,
    #[serde(rename = "displayName")]
    pub display_name: String,
    #[serde(rename = "iconUrl")]
    pub icon_url: String,
    #[serde(rename = "isIdleScreen")]
    pub is_idle_screen: bool,
    #[serde(rename = "launchedFromCloud")]
    pub launched_from_cloud: bool,
    pub namespaces: Vec<NamespaceMap>,
    #[serde(rename = "sessionId")]
    pub session_id: String,
    #[serde(rename = "statusText")]
    pub status_text: String,
    #[serde(rename = "transportId")]
    pub transport_id: String,
    #[serde(rename = "universalAppId")]
    pub universal_app_id: String,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct VolumeStatus {
    #[serde(rename = "controlType")]
    pub control_type: String,
    pub level: f64,
    pub muted: bool,
    #[serde(rename = "stepInterval")]
    pub step_interval: f64,
}

#[derive(Serialize, Deserialize, Debug)]
pub struct Status {
    pub applications: Option<Vec<Application>>,
    // TODO: `userEq`
    pub volume: VolumeStatus,
}

pub mod namespaces {
    use super::*;

    pub trait Namespace {
        fn name(&self) -> &'static str;
    }

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(tag = "type")]
    pub enum Connection {
        #[serde(rename = "CONNECT")]
        Connect {
            #[serde(rename = "connType")]
            conn_type: u64,
        },
    }

    impl Namespace for Connection {
        fn name(&self) -> &'static str {
            "urn:x-cast:com.google.cast.tp.connection"
        }
    }

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(tag = "type")]
    pub enum Heartbeat {
        #[serde(rename = "PING")]
        Ping,
        #[serde(rename = "PONG")]
        Pong,
    }

    impl Namespace for Heartbeat {
        fn name(&self) -> &'static str {
            HEARTBEAT_NAMESPACE
        }
    }

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(tag = "type")]
    pub enum Receiver {
        #[serde(rename = "SET_VOLUME")]
        SetVolume {
            volume: Volume,
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        #[serde(rename = "STOP")]
        StopCasting {
            #[serde(rename = "sessionId")]
            session_id: String,
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        #[serde(rename = "LAUNCH")]
        Launch {
            #[serde(rename = "appId")]
            app_id: String,
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        #[serde(rename = "GET_STATUS")]
        GetStatus {
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        #[serde(rename = "RECEIVER_STATUS")]
        Status {
            #[serde(rename = "requestId")]
            request_id: u64,
            status: Status,
        },
    }

    impl Namespace for Receiver {
        fn name(&self) -> &'static str {
            RECEIVER_NAMESPACE
        }
    }

    #[derive(Serialize, Deserialize, Debug)]
    #[serde(tag = "type")]
    pub enum Media {
        /// Loads new content into the media player.
        ///
        /// <https://developers.google.com/cast/docs/media/messages#Load>
        #[serde(rename = "LOAD")]
        Load {
            /// ID of the request, to correlate request and response
            #[serde(rename = "requestId")]
            request_id: u64,
            /// Metadata (including contentId) of the media to load
            media: MediaInformation,
            /// If the autoplay parameter is specified, the media player will begin playing the
            /// content when it is loaded. Even if autoplay is not specified, media player
            /// implementation may choose to begin playback immediately. If playback is started,
            /// the player state in the response should be set to BUFFERING, otherwise it should
            /// be set to PAUSED. default is true
            #[serde(rename = "autoPlay")]
            auto_play: Option<bool>,
            /// Seconds since beginning of content. If the content is live content, and position is
            /// not specified, the stream will start at the live position
            #[serde(rename = "currentTime")]
            current_time: Option<f64>,
            /// The media playback rate.
            #[serde(rename = "playbackRate", skip_serializing_if = "Option::is_none")]
            playback_rate: Option<f64>,
        },
        /// Sets the current position in the stream. Triggers a STATUS event notification to all
        /// sender applications. If the position provided is outside the range of valid positions
        /// for the current content, then the player should pick a valid position as close to the
        /// requested position as possible.
        ///
        /// <https://developers.google.com/cast/docs/media/messages#Seek>
        #[serde(rename = "SEEK")]
        Seek {
            /// ID of the media session where the position of the stream is set
            #[serde(rename = "mediaSessionId")]
            media_session_id: String,
            /// ID of the request, to correlate request and response
            #[serde(rename = "requestId")]
            request_id: u64,
            // TODO: `resumeState`
            #[serde(rename = "currentTime")]
            current_time: Option<f64>,
        },
        /// Begins playback of the content that was loaded with the load call, playback is continued
        /// from the current time position.
        ///
        /// <https://developers.google.com/cast/docs/media/messages#Play>
        #[serde(rename = "PLAY")]
        Resume {
            #[serde(rename = "mediaSessionId")]
            media_session_id: String,
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        /// Pauses playback of the current content. Triggers a STATUS event notification to all sender
        /// applications.
        ///
        /// <https://developers.google.com/cast/docs/media/messages#Pause>
        #[serde(rename = "PAUSE")]
        Pause {
            /// ID of the media session to be paused
            #[serde(rename = "mediaSessionId")]
            media_session_id: String,
            /// ID of the request, to use to correlate request/response
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        /// Stops playback of the current content. Triggers a STATUS event notification to all sender
        /// applications. After this command the content will no longer be loaded and the
        /// mediaSessionId is invalidated.
        ///
        /// <https://developers.google.com/cast/docs/media/messages#Stop>
        #[serde(rename = "STOP")]
        Stop {
            /// ID of the media session for the content to be stopped
            #[serde(rename = "mediaSessionId")]
            media_session_id: String,
            /// ID of the request, to correlate request and response
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        /// Retrieves the media status.
        ///
        /// <https://developers.google.com/cast/docs/media/messages#GetStatus>
        #[serde(rename = "GET_STATUS")]
        GetStatus {
            /// Media session ID of the media for which the media status should be returned. If none
            /// is provided, then the status for all media session IDs will be provided.
            #[serde(rename = "mediaSessionId")]
            media_session_id: Option<u64>,
            /// ID of the request, to correlate request and response
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        /// Sent after a state change or after a media status request. Only the MediaStatus objects
        /// that changed or were requested will be sent.
        ///
        /// <https://developers.google.com/cast/docs/media/messages#MediaStatusMess>
        #[serde(rename = "MEDIA_STATUS")]
        Status {
            /// ID used to correlate this status response with the request that originated it or 0
            /// if the status message is spontaneous (not triggered by a sender request). Sender
            /// applications will generate unique request IDs by selecting a random number and
            /// continuously increasing it (they will not use 0).
            #[serde(rename = "requestId")]
            request_id: u64,
            /// Array of Media Status objects. NOTE: the media element in MediaStatus will only be
            /// returned if it has changed.
            status: Vec<MediaStatus>,
        },
        #[serde(rename = "SET_PLAYBACK_RATE")]
        SetPlaybackRate {
            #[serde(rename = "mediaSessionId")]
            media_session_id: u64,
            #[serde(rename = "requestId")]
            request_id: u64,
            #[serde(rename = "playbackRate")]
            playback_rate: f64,
        },
    }

    impl Namespace for Media {
        fn name(&self) -> &'static str {
            MEDIA_NAMESPACE
        }
    }
}
