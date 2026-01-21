use std::collections::HashMap;

use serde::{de, ser, Deserialize, Serialize};
use serde_json::{json, Value};
use serde_repr::{Deserialize_repr, Serialize_repr};
use serde_with::skip_serializing_none;

macro_rules! get_from_map {
    ($map:expr, $key:expr) => {
        $map.get($key).ok_or(de::Error::missing_field($key))
    };
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum MetadataObject {
    Generic {
        title: Option<String>,
        thumbnail_url: Option<String>,
        custom: Option<Value>,
    },
}

impl Serialize for MetadataObject {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        match self {
            MetadataObject::Generic {
                title,
                thumbnail_url,
                custom,
            } => {
                let mut map = serde_json::Map::new();
                map.insert("type".to_owned(), json!(0u64));
                map.insert(
                    "title".to_owned(),
                    match title {
                        Some(t) => Value::String(t.to_owned()),
                        None => Value::Null,
                    },
                );
                map.insert(
                    "thumbnailUrl".to_owned(),
                    match thumbnail_url {
                        Some(t) => Value::String(t.to_owned()),
                        None => Value::Null,
                    },
                );
                if let Some(custom) = custom {
                    map.insert("custom".to_owned(), custom.clone());
                }
                map.serialize(serializer)
            }
        }
    }
}

impl<'de> Deserialize<'de> for MetadataObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut map = serde_json::Map::deserialize(deserializer)?;

        let type_ = map
            .remove("type")
            .ok_or(de::Error::missing_field("type"))?
            .as_u64()
            .ok_or(de::Error::custom("`type` is not an integer"))?;
        let rest = Value::Object(map);

        match type_ {
            0 => {
                let title = match rest.get("title") {
                    Some(t) => Some(
                        t.as_str()
                            .ok_or(de::Error::custom("`title` is not a string"))?
                            .to_owned(),
                    ),
                    None => None,
                };
                let thumbnail_url = match rest.get("thumbnailUrl") {
                    Some(t) => Some(
                        t.as_str()
                            .ok_or(de::Error::custom("`thumbnailUrl` is not a string"))?
                            .to_owned(),
                    ),
                    None => None,
                };
                Ok(Self::Generic {
                    title,
                    thumbnail_url,
                    custom: rest.get("custom").cloned(),
                })
            }
            _ => Err(de::Error::custom(format!("Unknown metadata type {type_}"))),
        }
    }
}

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PlayMessage {
    /// The MIME type (video/mp4)
    pub container: String,
    // The URL to load (optional)
    pub url: Option<String>,
    // The content to load (i.e. a DASH manifest, json content, optional)
    pub content: Option<String>,
    // The time to start playing in seconds
    pub time: Option<f64>,
    // The desired volume (0-1)
    pub volume: Option<f64>,
    // The factor to multiply playback speed by (defaults to 1.0)
    pub speed: Option<f64>,
    // HTTP request headers to add to the play request Map<string, string>
    pub headers: Option<HashMap<String, String>>,
    pub metadata: Option<MetadataObject>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize_repr, Serialize_repr)]
#[repr(u8)]
pub enum ContentType {
    #[default]
    Playlist = 0,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize)]
pub struct MediaItem {
    /// The MIME type (video/mp4)
    pub container: String,
    /// The URL to load (optional)
    pub url: Option<String>,
    /// The content to load (i.e. a DASH manifest, json content, optional)
    pub content: Option<String>,
    /// The time to start playing in seconds
    pub time: Option<f64>,
    /// The desired volume (0-1)
    pub volume: Option<f64>,
    /// The factor to multiply playback speed by (defaults to 1.0)
    pub speed: Option<f64>,
    /// Indicates if the receiver should preload the media item
    pub cache: Option<bool>,
    /// Indicates how long the item content is presented on screen in seconds
    #[serde(rename = "showDuration")]
    pub show_duration: Option<f64>,
    /// HTTP request headers to add to the play request Map<string, string>
    pub headers: Option<HashMap<String, String>>,
    pub metadata: Option<MetadataObject>,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize)]
pub struct PlaylistContent {
    #[serde(rename = "contentType")]
    pub variant: ContentType,
    pub items: Vec<MediaItem>,
    /// Start position of the first item to play from the playlist
    pub offset: Option<u64>, // int or float?
    /// The desired volume (0-1)
    pub volume: Option<f64>,
    /// The factor to multiply playback speed by (defaults to 1.0)
    pub speed: Option<f64>,
    /// Count of media items should be pre-loaded forward from the current view index
    #[serde(rename = "forwardCache")]
    pub forward_cache: Option<u64>,
    /// Count of media items should be pre-loaded backward from the current view index
    #[serde(rename = "backwardCache")]
    pub backward_cache: Option<u64>,
    pub metadata: Option<MetadataObject>,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct PlaybackUpdateMessage {
    // The time the packet was generated (unix time milliseconds)
    #[serde(rename = "generationTime")]
    pub generation_time: u64,
    // The playback state
    pub state: crate::PlaybackState,
    // The current time playing in seconds
    pub time: Option<f64>,
    // The duration in seconds
    pub duration: Option<f64>,
    // The playback speed factor
    pub speed: Option<f64>,
    // The playlist item index currently being played on receiver
    #[serde(rename = "itemIndex")]
    pub item_index: Option<u64>,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct InitialSenderMessage {
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "appName")]
    pub app_name: Option<String>,
    #[serde(rename = "appVersion")]
    pub app_version: Option<String>,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct LivestreamCapabilities {
    /// https://datatracker.ietf.org/doc/draft-murillo-whep/
    pub whep: Option<bool>,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct AVCapabilities {
    pub livestream: Option<LivestreamCapabilities>,
}

#[skip_serializing_none]
#[derive(Debug, PartialEq, Eq, Clone, Default, Serialize, Deserialize)]
pub struct ReceiverCapabilities {
    pub av: Option<AVCapabilities>,
}

#[skip_serializing_none]
#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize)]
pub struct InitialReceiverMessage {
    #[serde(rename = "displayName")]
    pub display_name: Option<String>,
    #[serde(rename = "appName")]
    pub app_name: Option<String>,
    #[serde(rename = "appVersion")]
    pub app_version: Option<String>,
    #[serde(rename = "playData")]
    pub play_data: Option<PlayMessage>,
    #[serde(rename = "experimentalCapabilities")]
    pub experimental_capabilities: Option<ReceiverCapabilities>,
}

#[skip_serializing_none]
#[allow(dead_code)]
#[derive(Debug, PartialEq, Clone, Default, Serialize, Deserialize)]
pub struct PlayUpdateMessage {
    #[serde(rename = "generationTime")]
    pub generation_time: Option<u64>,
    #[serde(rename = "playData")]
    pub play_data: Option<PlayMessage>,
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct SetPlaylistItemMessage {
    #[serde(rename = "itemIndex")]
    pub item_index: u64,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy, Serialize, Deserialize)]
pub enum KeyNames {
    ArrowLeft,
    ArrowRight,
    ArrowUp,
    ArrowDown,
    Enter,
}

#[allow(dead_code)]
impl KeyNames {
    pub fn all() -> Vec<String> {
        vec![
            "ArrowLeft".to_owned(),
            "ArrowRight".to_owned(),
            "ArrowUp".to_owned(),
            "ArrowDown".to_owned(),
            "Enter".to_owned(),
        ]
    }
}

// #[derive(Debug, PartialEq, Eq, Clone, Hash)]
#[derive(Debug, PartialEq, Eq, Clone)]
pub enum EventSubscribeObject {
    MediaItemStart,
    MediaItemEnd,
    MediaItemChanged,
    KeyDown { keys: Vec<String> },
    KeyUp { keys: Vec<String> },
}

impl Serialize for EventSubscribeObject {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serde_json::Map::new();
        let type_val: u64 = match self {
            EventSubscribeObject::MediaItemStart => 0,
            EventSubscribeObject::MediaItemEnd => 1,
            EventSubscribeObject::MediaItemChanged => 2,
            EventSubscribeObject::KeyDown { .. } => 3,
            EventSubscribeObject::KeyUp { .. } => 4,
        };

        map.insert("type".to_owned(), json!(type_val));

        let keys = match self {
            EventSubscribeObject::KeyDown { keys } => Some(keys),
            EventSubscribeObject::KeyUp { keys } => Some(keys),
            _ => None,
        };
        if let Some(keys) = keys {
            map.insert("keys".to_owned(), json!(keys));
        }

        map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EventSubscribeObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        let mut map = serde_json::Map::deserialize(deserializer)?;
        let type_ = map
            .remove("type")
            .ok_or(de::Error::missing_field("type"))?
            .as_u64()
            .ok_or(de::Error::custom("`type` is not an integer"))?;
        let rest = Value::Object(map);

        match type_ {
            0 => Ok(Self::MediaItemStart),
            1 => Ok(Self::MediaItemEnd),
            2 => Ok(Self::MediaItemChanged),
            3 | 4 => {
                let keys = get_from_map!(rest, "keys")?
                    .as_array()
                    .ok_or(de::Error::custom("`type` is not an array"))?
                    .iter()
                    .map(|v| v.as_str().map(|s| s.to_owned()))
                    .collect::<Option<Vec<String>>>()
                    .ok_or(de::Error::custom("`type` is not an array of strings"))?;
                if type_ == 3 {
                    Ok(Self::KeyDown { keys })
                } else {
                    Ok(Self::KeyUp { keys })
                }
            }
            _ => Err(de::Error::custom(format!("Unknown event type {type_}"))),
        }
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct SubscribeEventMessage {
    pub event: EventSubscribeObject,
}

#[derive(Debug, PartialEq, Eq, Clone, Serialize, Deserialize)]
pub struct UnsubscribeEventMessage {
    pub event: EventSubscribeObject,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
#[repr(u8)]
pub enum EventType {
    MediaItemStart = 0,
    MediaItemEnd = 1,
    MediaItemChange = 2,
    KeyDown = 3,
    KeyUp = 4,
}

#[derive(Debug, PartialEq, Clone)]
#[allow(clippy::large_enum_variant)]
pub enum EventObject {
    MediaItem {
        variant: EventType,
        item: MediaItem,
    },
    Key {
        variant: EventType,
        key: String,
        repeat: bool,
        handled: bool,
    },
}

impl Serialize for EventObject {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: serde::Serializer,
    {
        let mut map = serde_json::Map::new();

        match self {
            EventObject::MediaItem { variant, item } => {
                map.insert("type".to_owned(), json!(*variant as u8));
                map.insert(
                    "item".to_owned(),
                    serde_json::to_value(item).map_err(ser::Error::custom)?,
                );
            }
            EventObject::Key {
                variant,
                key,
                repeat,
                handled,
            } => {
                map.insert("type".to_owned(), json!(*variant as u8));
                map.insert(
                    "key".to_owned(),
                    serde_json::to_value(key).map_err(ser::Error::custom)?,
                );
                map.insert(
                    "repeat".to_owned(),
                    serde_json::to_value(repeat).map_err(ser::Error::custom)?,
                );
                map.insert(
                    "handled".to_owned(),
                    serde_json::to_value(handled).map_err(ser::Error::custom)?,
                );
            }
        }

        map.serialize(serializer)
    }
}

impl<'de> Deserialize<'de> for EventObject {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: de::Deserializer<'de>,
    {
        let mut map = serde_json::Map::deserialize(deserializer)?;
        let type_ = map
            .remove("type")
            .ok_or(de::Error::missing_field("type"))?
            .as_u64()
            .ok_or(de::Error::custom("`type` is not an integer"))?;
        let rest = Value::Object(map);

        match type_ {
            #[allow(clippy::manual_range_patterns)]
            0 | 1 | 2 => {
                let variant = match type_ {
                    0 => EventType::MediaItemStart,
                    1 => EventType::MediaItemEnd,
                    _ => EventType::MediaItemChange,
                };
                let item = get_from_map!(rest, "item")?;
                Ok(Self::MediaItem {
                    variant,
                    item: MediaItem::deserialize(item).map_err(de::Error::custom)?,
                })
            }
            3 | 4 => {
                let variant = if type_ == 3 {
                    EventType::KeyDown
                } else {
                    EventType::KeyUp
                };
                Ok(Self::Key {
                    variant,
                    key: get_from_map!(rest, "key")?
                        .as_str()
                        .ok_or(de::Error::custom("`key` is not a string"))?
                        .to_owned(),
                    repeat: get_from_map!(rest, "repeat")?
                        .as_bool()
                        .ok_or(de::Error::custom("`repeat` is not a bool"))?,
                    handled: get_from_map!(rest, "handled")?
                        .as_bool()
                        .ok_or(de::Error::custom("`handled` is not a bool"))?,
                })
            }
            _ => Err(de::Error::custom(format!("Unknown event type {type_}"))),
        }
    }
}

#[derive(Debug, PartialEq, Clone, Serialize, Deserialize)]
pub struct EventMessage {
    #[serde(rename = "generationTime")]
    pub generation_time: u64,
    pub event: EventObject,
}

pub use crate::v2::VolumeUpdateMessage;

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! s {
        ($s:expr) => {
            ($s).to_string()
        };
    }

    #[test]
    fn serialize_metadata_object() {
        assert_eq!(
            &serde_json::to_string(&MetadataObject::Generic {
                title: Some(s!("abc")),
                thumbnail_url: Some(s!("def")),
                custom: Some(serde_json::Value::Null),
            })
            .unwrap(),
            r#"{"custom":null,"thumbnailUrl":"def","title":"abc","type":0}"#
        );
        assert_eq!(
            &serde_json::to_string(&MetadataObject::Generic {
                title: None,
                thumbnail_url: None,
                custom: Some(serde_json::Value::Null),
            })
            .unwrap(),
            r#"{"custom":null,"thumbnailUrl":null,"title":null,"type":0}"#
        );
        assert_eq!(
            &serde_json::to_string(&MetadataObject::Generic {
                title: Some(s!("abc")),
                thumbnail_url: Some(s!("def")),
                custom: None,
            })
            .unwrap(),
            r#"{"thumbnailUrl":"def","title":"abc","type":0}"#
        );
    }

    #[test]
    fn deserialize_metadata_object() {
        assert_eq!(
            serde_json::from_str::<MetadataObject>(
                r#"{"type":0,"title":"abc","thumbnailUrl":"def","custom":null}"#
            )
            .unwrap(),
            MetadataObject::Generic {
                title: Some(s!("abc")),
                thumbnail_url: Some(s!("def")),
                custom: Some(serde_json::Value::Null),
            }
        );
        assert_eq!(
            serde_json::from_str::<MetadataObject>(r#"{"type":0,"custom":null}"#).unwrap(),
            MetadataObject::Generic {
                title: None,
                thumbnail_url: None,
                custom: Some(serde_json::Value::Null),
            }
        );
        assert_eq!(
            serde_json::from_str::<MetadataObject>(r#"{"type":0}"#).unwrap(),
            MetadataObject::Generic {
                title: None,
                thumbnail_url: None,
                custom: None,
            }
        );
        assert!(serde_json::from_str::<MetadataObject>(r#"{"type":1"#).is_err());
    }

    #[test]
    fn serialize_event_sub_obj() {
        assert_eq!(
            &serde_json::to_string(&EventSubscribeObject::MediaItemStart).unwrap(),
            r#"{"type":0}"#
        );
        assert_eq!(
            &serde_json::to_string(&EventSubscribeObject::MediaItemEnd).unwrap(),
            r#"{"type":1}"#
        );
        assert_eq!(
            &serde_json::to_string(&EventSubscribeObject::MediaItemChanged).unwrap(),
            r#"{"type":2}"#
        );
        assert_eq!(
            &serde_json::to_string(&EventSubscribeObject::KeyDown { keys: vec![] }).unwrap(),
            r#"{"keys":[],"type":3}"#
        );
        assert_eq!(
            &serde_json::to_string(&EventSubscribeObject::KeyDown { keys: vec![] }).unwrap(),
            r#"{"keys":[],"type":3}"#
        );
        assert_eq!(
            &serde_json::to_string(&EventSubscribeObject::KeyUp {
                keys: vec![s!("abc"), s!("def")]
            })
            .unwrap(),
            r#"{"keys":["abc","def"],"type":4}"#
        );
        assert_eq!(
            &serde_json::to_string(&EventSubscribeObject::KeyDown {
                keys: vec![s!("abc"), s!("def")]
            })
            .unwrap(),
            r#"{"keys":["abc","def"],"type":3}"#
        );
        assert_eq!(
            &serde_json::to_string(&EventSubscribeObject::KeyDown {
                keys: vec![s!("\"\"")]
            })
            .unwrap(),
            r#"{"keys":["\"\""],"type":3}"#
        );
    }

    #[test]
    fn deserialize_event_sub_obj() {
        assert_eq!(
            serde_json::from_str::<EventSubscribeObject>(r#"{"type":0}"#).unwrap(),
            EventSubscribeObject::MediaItemStart
        );
        assert_eq!(
            serde_json::from_str::<EventSubscribeObject>(r#"{"type":1}"#).unwrap(),
            EventSubscribeObject::MediaItemEnd
        );
        assert_eq!(
            serde_json::from_str::<EventSubscribeObject>(r#"{"type":2}"#).unwrap(),
            EventSubscribeObject::MediaItemChanged
        );
        assert_eq!(
            serde_json::from_str::<EventSubscribeObject>(r#"{"keys":[],"type":3}"#).unwrap(),
            EventSubscribeObject::KeyDown { keys: vec![] }
        );
        assert_eq!(
            serde_json::from_str::<EventSubscribeObject>(r#"{"keys":[],"type":4}"#).unwrap(),
            EventSubscribeObject::KeyUp { keys: vec![] }
        );
        assert_eq!(
            serde_json::from_str::<EventSubscribeObject>(r#"{"keys":["abc","def"],"type":3}"#)
                .unwrap(),
            EventSubscribeObject::KeyDown {
                keys: vec![s!("abc"), s!("def")]
            }
        );
        assert_eq!(
            serde_json::from_str::<EventSubscribeObject>(r#"{"keys":["abc","def"],"type":4}"#)
                .unwrap(),
            EventSubscribeObject::KeyUp {
                keys: vec![s!("abc"), s!("def")]
            }
        );
        assert!(serde_json::from_str::<EventSubscribeObject>(r#""type":5}"#).is_err());
    }

    const EMPTY_TEST_MEDIA_ITEM: MediaItem = MediaItem {
        container: String::new(),
        url: None,
        content: None,
        time: None,
        volume: None,
        speed: None,
        cache: None,
        show_duration: None,
        headers: None,
        metadata: None,
    };
    const TEST_MEDIA_ITEM_JSON: &str = r#"{"container":""}"#;

    #[test]
    fn serialize_event_obj() {
        assert_eq!(
            serde_json::to_string(&EventObject::MediaItem {
                variant: EventType::MediaItemStart,
                item: EMPTY_TEST_MEDIA_ITEM.clone(),
            })
            .unwrap(),
            format!(r#"{{"item":{TEST_MEDIA_ITEM_JSON},"type":0}}"#),
        );
        assert_eq!(
            serde_json::to_string(&EventObject::MediaItem {
                variant: EventType::MediaItemEnd,
                item: EMPTY_TEST_MEDIA_ITEM.clone(),
            })
            .unwrap(),
            format!(r#"{{"item":{TEST_MEDIA_ITEM_JSON},"type":1}}"#),
        );
        assert_eq!(
            serde_json::to_string(&EventObject::MediaItem {
                variant: EventType::MediaItemChange,
                item: EMPTY_TEST_MEDIA_ITEM.clone(),
            })
            .unwrap(),
            format!(r#"{{"item":{TEST_MEDIA_ITEM_JSON},"type":2}}"#),
        );
        assert_eq!(
            &serde_json::to_string(&EventObject::Key {
                variant: EventType::KeyDown,
                key: s!(""),
                repeat: false,
                handled: false,
            })
            .unwrap(),
            r#"{"handled":false,"key":"","repeat":false,"type":3}"#
        );
        assert_eq!(
            &serde_json::to_string(&EventObject::Key {
                variant: EventType::KeyUp,
                key: s!(""),
                repeat: false,
                handled: false,
            })
            .unwrap(),
            r#"{"handled":false,"key":"","repeat":false,"type":4}"#
        );
    }

    #[test]
    fn deserialize_event_obj() {
        assert_eq!(
            serde_json::from_str::<EventObject>(&format!(
                r#"{{"item":{TEST_MEDIA_ITEM_JSON},"type":0}}"#
            ))
            .unwrap(),
            EventObject::MediaItem {
                variant: EventType::MediaItemStart,
                item: EMPTY_TEST_MEDIA_ITEM.clone(),
            }
        );
        assert_eq!(
            serde_json::from_str::<EventObject>(&format!(
                r#"{{"item":{TEST_MEDIA_ITEM_JSON},"type":1}}"#
            ))
            .unwrap(),
            EventObject::MediaItem {
                variant: EventType::MediaItemEnd,
                item: EMPTY_TEST_MEDIA_ITEM.clone(),
            }
        );
        assert_eq!(
            serde_json::from_str::<EventObject>(&format!(
                r#"{{"item":{TEST_MEDIA_ITEM_JSON},"type":2}}"#
            ))
            .unwrap(),
            EventObject::MediaItem {
                variant: EventType::MediaItemChange,
                item: EMPTY_TEST_MEDIA_ITEM.clone(),
            }
        );
        assert_eq!(
            serde_json::from_str::<EventObject>(
                r#"{"handled":false,"key":"","repeat":false,"type":3}"#
            )
            .unwrap(),
            EventObject::Key {
                variant: EventType::KeyDown,
                key: s!(""),
                repeat: false,
                handled: false,
            }
        );
        assert_eq!(
            serde_json::from_str::<EventObject>(
                r#"{"handled":false,"key":"","repeat":false,"type":4}"#
            )
            .unwrap(),
            EventObject::Key {
                variant: EventType::KeyUp,
                key: s!(""),
                repeat: false,
                handled: false,
            }
        );
        assert!(serde_json::from_str::<EventObject>(r#"{"type":5}"#).is_err());
    }

    #[test]
    fn serialize_playlist_content() {
        assert_eq!(
            serde_json::to_string(&PlaylistContent {
                variant: ContentType::Playlist,
                items: Vec::new(),
                offset: None,
                volume: None,
                speed: None,
                forward_cache: None,
                backward_cache: None,
                metadata: None
            })
            .unwrap(),
            r#"{"contentType":0,"items":[]}"#,
        );
        assert_eq!(
            serde_json::to_string(&PlaylistContent {
                variant: ContentType::Playlist,
                items: Vec::new(),
                offset: None,
                volume: Some(1.0),
                speed: Some(1.0),
                forward_cache: None,
                backward_cache: None,
                metadata: None
            })
            .unwrap(),
            r#"{"contentType":0,"items":[],"volume":1.0,"speed":1.0}"#,
        );
        assert_eq!(
            serde_json::to_string(&PlaylistContent {
                variant: ContentType::Playlist,
                items: vec![MediaItem {
                    container: "video/mp4".to_string(),
                    url: Some("abc".to_string()),
                    content: None,
                    time: None,
                    volume: None,
                    speed: None,
                    cache: None,
                    show_duration: None,
                    headers: None,
                    metadata: None
                }],
                offset: None,
                volume: None,
                speed: None,
                forward_cache: None,
                backward_cache: None,
                metadata: None
            })
            .unwrap(),
            r#"{"contentType":0,"items":[{"container":"video/mp4","url":"abc"}]}"#,
        );
        assert_eq!(
            serde_json::to_string(&PlaylistContent {
                variant: ContentType::Playlist,
                items: vec![MediaItem {
                    container: "video/mp4".to_string(),
                    url: Some("abc".to_string()),
                    content: None,
                    time: None,
                    volume: None,
                    speed: None,
                    cache: None,
                    show_duration: None,
                    headers: None,
                    metadata: None
                }],
                offset: None,
                volume: Some(1.0),
                speed: Some(1.0),
                forward_cache: None,
                backward_cache: None,
                metadata: Some(MetadataObject::Generic { title: None, thumbnail_url: None, custom: None })
            })
            .unwrap(),
            r#"{"contentType":0,"items":[{"container":"video/mp4","url":"abc"}],"volume":1.0,"speed":1.0,"metadata":{"thumbnailUrl":null,"title":null,"type":0}}"#,
        );
    }
}
