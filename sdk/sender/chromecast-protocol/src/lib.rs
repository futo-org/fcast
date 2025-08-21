use serde::{de, ser, Deserialize, Serialize};

pub use prost;
use serde_json::{json, Value};

pub mod protos {
    include!(concat!(env!("OUT_DIR"), "/protos.rs"));
}

pub const HEARTBEAT_NAMESPACE: &str = "urn:x-cast:com.google.cast.tp.heartbeat";
pub const RECEIVER_NAMESPACE: &str = "urn:x-cast:com.google.cast.receiver";
pub const MEDIA_NAMESPACE: &str = "urn:x-cast:com.google.cast.media";
pub const CONNECTION_NAMESPACE: &str = "urn:x-cast:com.google.cast.tp.connection";

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

#[derive(Serialize, Deserialize, Debug, PartialEq)]
pub struct Image {
    pub url: String,
}

#[derive(Debug, PartialEq)]
pub enum Metadata {
    Generic {
        title: Option<String>,
        subtitle: Option<String>,
        images: Option<Vec<Image>>,
        release_date: Option<String>,
    },
}

impl Serialize for Metadata {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            Metadata::Generic {
                title,
                subtitle,
                images,
                release_date,
            } => {
                let mut map = serde_json::Map::new();
                map.insert("metadataType".to_owned(), json!(0u64));
                map.insert(
                    "title".to_owned(),
                    match title {
                        Some(t) => Value::String(t.to_owned()),
                        None => Value::Null,
                    },
                );
                map.insert(
                    "subtitle".to_owned(),
                    match subtitle {
                        Some(s) => Value::String(s.to_owned()),
                        None => Value::Null,
                    },
                );
                map.insert(
                    "images".to_owned(),
                    match images {
                        Some(i) => serde_json::to_value(i)
                            .map_err(|_| ser::Error::custom("failed to serialize `images`"))?,
                        None => Value::Null,
                    },
                );
                map.insert(
                    "releaseDate".to_owned(),
                    match release_date {
                        Some(r) => Value::String(r.to_owned()),
                        None => Value::Null,
                    },
                );
                map.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for Metadata {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut map = serde_json::Map::deserialize(deserializer)?;

        let type_ = map
            .remove("metadataType")
            .ok_or(de::Error::missing_field("metadataType"))?
            .as_u64()
            .ok_or(de::Error::custom("`metadataType` is not an integer"))?;
        let rest = Value::Object(map);

        match type_ {
            0 => {
                let title = match rest.get("title") {
                    Some(t) => t.as_str().map(|s| s.to_string()),
                    None => None,
                };
                let subtitle = match rest.get("subtitle") {
                    Some(s) => s.as_str().map(|s| s.to_string()),
                    None => None,
                };
                let images = match rest.get("images") {
                    Some(i) => match i.as_array() {
                        Some(images) => Some(
                            images
                                .iter()
                                .map(|maybe_image| {
                                    serde_json::from_value::<Image>(maybe_image.clone())
                                })
                                .collect::<Result<Vec<Image>, serde_json::Error>>()
                                .map_err(|_| {
                                    de::Error::custom("`images` is not an array of images")
                                })?,
                        ),
                        None => None,
                    },
                    None => None,
                };
                let release_date = match rest.get("releaseDate") {
                    Some(r) => r.as_str().map(|s| s.to_string()),
                    None => None,
                };

                Ok(Self::Generic {
                    title,
                    subtitle,
                    images,
                    release_date,
                })
            }
            _ => Err(de::Error::custom(format!("Unknown metadata type {type_}"))),
        }
    }
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
    pub metadata: Option<Metadata>,
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

#[derive(Serialize, Deserialize, Debug, PartialEq, Eq)]
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

/// <https://developers.google.com/cast/docs/reference/web_sender/chrome.cast.media.QueueItem>
#[derive(Serialize, Deserialize, Debug)]
pub struct QueueItem {
    /// Whether the media will automatically play.
    pub autoplay: bool,
    pub media: MediaInformation,
    /// Playback duration of the item in seconds. If it is larger than the actual duration - startTime it will be
    /// limited to the actual duration - startTime. It can be negative, in such case the duration will be the actual
    /// item duration minus the duration provided. A duration of value zero effectively means that the item will not be
    /// played.
    #[serde(rename = "playbackDuration")]
    pub playback_duration: i32,
    // This parameter is a hint for the receiver to preload this media item before it is played. It allows for a smooth
    // transition between items played from the queue.
    //
    // The time is expressed in seconds, relative to the beginning of this item playback (usually the end of the
    // previous item playback). Only positive values are valid. For example, if the value is 10 seconds, this item will
    // be preloaded 10 seconds before the previous item has finished. The receiver will try to honor this value but
    // will not guarantee it, for example if the value is larger than the previous item duration the receiver may just
    // preload this item shortly after the previous item has started playing (there will never be two items being
    // preloaded in parallel). Also, if an item is inserted in the queue just after the currentItem and the time to
    // preload is higher than the time left on the currentItem, the preload will just happen as soon as possible.
    // #[serde(rename = "preloadTime")]
    // pub preload_time: f64,
    /// Seconds from the beginning of the media to start playback.
    #[serde(rename = "startTime")]
    pub start_time: f64,
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
pub enum QueueRepeatMode {
    /// Items are played in order, and when the queue is completed (the last item has ended) the media session is
    /// terminated.
    #[serde(rename = "REPEAT_OFF")]
    Off,
    /// The items in the queue will be played indefinitely. When the last item has ended, the first item will be played
    /// again.
    #[serde(rename = "REPEAT_ALL")]
    All,
    /// The current item will be repeated indefinitely.
    #[serde(rename = "REPEAT_SINGLE")]
    Single,
    /// The items in the queue will be played indefinitely. When the last item has ended, the list of items will be
    /// randomly shuffled by the receiver, and the queue will continue to play starting from the first item of the
    /// shuffled items.
    #[serde(rename = "REPEAT_ALL_AND_SHUFFLE")]
    AllAndShuffle,
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

#[derive(Serialize, Deserialize, Debug)]
pub enum InvalidRequestReason {
    #[serde(rename = "INVALID_COMMAND")]
    InvalidCommand,
    #[serde(rename = "DUPLICATE_REQUESTID")]
    DuplicateRequestId,
    #[serde(rename = "INVALID_MEDIA_SESSION_ID")]
    InvalidMediaSessionId,
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
        #[serde(rename = "CLOSE")]
        Close,
    }

    impl Namespace for Connection {
        fn name(&self) -> &'static str {
            CONNECTION_NAMESPACE
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
        StopSession {
            #[serde(rename = "requestId")]
            request_id: u64,
            #[serde(rename = "sessionId")]
            session_id: String,
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
        #[serde(rename = "LAUNCH_STATUS")]
        LaunchStatus {
            #[serde(rename = "launchRequestId")]
            request_id: u64,
            status: String,
        },
    }

    impl Namespace for Receiver {
        fn name(&self) -> &'static str {
            RECEIVER_NAMESPACE
        }
    }

    // TODO: can media_session_id be a u64?
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
        #[serde(rename = "QUEUE_LOAD")]
        QueueLoad {
            #[serde(rename = "requestId")]
            request_id: u64,
            /// Array of items to load. It is sorted (first element will be played first). Must not be null or empty.
            items: Vec<QueueItem>,
            #[serde(rename = "repeatMode")]
            repeat_mode: QueueRepeatMode,
            /// The index of the item in the items array that must be the first currentItem (the item that will be
            /// played first). Note this is the index of the array (starts at 0) and not the itemId (as it is not known
            /// until the queue is created). If repeatMode is chrome.cast.media.RepeatMode.OFF playback will end when
            /// the last item in the array is played (elements before the startIndex will not be played). This may be
            /// useful for continuation scenarios where the user was already using the sender app and in the middle
            /// decides to cast. In this way the sender app does not need to map between the local and remote queue
            /// positions or saves one extra request to update the queue.
            #[serde(rename = "startIndex")]
            start_index: u32,
            #[serde(rename = "queueType")]
            queue_type: Option<String>,
        },
        #[serde(rename = "QUEUE_UPDATE")]
        QueueUpdate {
            #[serde(rename = "requestId")]
            request_id: u64,
            #[serde(rename = "mediaSessionId")]
            media_session_id: String,
            jump: Option<i32>,
        },
        /// https://developers.google.com/cast/docs/media/messages#InvalidPlayerState
        ///
        /// <https://developers.google.com/cast/docs/media/messages#InvalidPlayerState>
        #[serde(rename = "INVALID_PLAYER_STATE")]
        InvalidPlayerState {
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        /// Sent when the load request failed. The player state will be IDLE.
        ///
        /// <https://developers.google.com/cast/docs/media/messages#LoadFailed>
        #[serde(rename = "LOAD_FAILED")]
        LoadFailed {
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        #[serde(rename = "ERROR")]
        Error {
            #[serde(rename = "requestId")]
            request_id: u64,
            #[serde(rename = "detailedErrorCode")]
            detailed_error_code: Option<u64>,
            reason: Option<String>,
            #[serde(rename = "itemId")]
            item_id: u64,
        },
        /// Sent when the load request was cancelled (a second load request was received).
        ///
        /// <https://developers.google.com/cast/docs/media/messages#LoadCancelled>
        #[serde(rename = "LOAD_CANCELLED")]
        LoadCancelled {
            #[serde(rename = "requestId")]
            request_id: u64,
        },
        /// Sent when the request is invalid (an unknown request type, for example).
        ///
        /// <https://developers.google.com/cast/docs/media/messages#InvalidRequest>
        #[serde(rename = "INVALID_REQUEST")]
        InvalidRequest {
            #[serde(rename = "requestId")]
            request_id: u64,
            reason: InvalidRequestReason,
        },
    }

    impl Namespace for Media {
        fn name(&self) -> &'static str {
            MEDIA_NAMESPACE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn serde_generic_metadata() {
        let meta = Metadata::Generic {
            title: None,
            subtitle: None,
            images: None,
            release_date: None,
        };
        assert_eq!(
            serde_json::from_str::<Metadata>(&serde_json::to_string(&meta).unwrap()).unwrap(),
            meta,
        );
        let meta = Metadata::Generic {
            title: Some("title".to_owned()),
            subtitle: Some("subtitle".to_owned()),
            images: Some(vec![Image {
                url: "url".to_owned(),
            }]),
            release_date: None,
        };
        assert_eq!(
            serde_json::from_str::<Metadata>(&serde_json::to_string(&meta).unwrap()).unwrap(),
            meta,
        );
    }
}
