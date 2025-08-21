use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Serialize, Debug)]
pub struct PlayMessage {
    pub container: String,
    pub url: Option<String>,
    pub content: Option<String>,
    pub time: Option<f64>,
    pub speed: Option<f64>,
    pub headers: Option<HashMap<String, String>>,
}

#[allow(dead_code)]
#[derive(Deserialize, Debug)]
pub struct PlaybackUpdateMessage {
    #[serde(rename = "generationTime")]
    pub generation_time: u64,
    pub time: f64,
    pub duration: f64,
    pub speed: f64,
    pub state: u8, //0 = None, 1 = Playing, 2 = Paused
}
