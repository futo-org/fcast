use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct PlayMessage {
    pub container: String,
    pub url: Option<String>,
    pub content: Option<String>,
    pub time: Option<f64>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct PlaybackUpdateMessage {
    pub time: f64,
    pub state: crate::PlaybackState,
}

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct VolumeUpdateMessage {
    pub volume: f64, //(0-1)
}
