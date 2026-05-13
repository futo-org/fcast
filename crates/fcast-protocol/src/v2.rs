use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use serde_with::skip_serializing_none;

#[skip_serializing_none]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct PlayMessage {
    pub container: String,
    pub url: Option<String>,
    pub content: Option<String>,
    pub time: Option<f64>,
    pub speed: Option<f64>,
    pub headers: Option<HashMap<String, String>>,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct PlaybackUpdateMessage {
    #[serde(rename = "generationTime")]
    pub generation_time: u64,
    pub time: f64,
    pub duration: f64,
    pub speed: f64,
    pub state: crate::PlaybackState,
}

#[allow(dead_code)]
#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct VolumeUpdateMessage {
    #[serde(rename = "generationTime")]
    pub generation_time: u64,
    pub volume: f64, //(0-1)
}
