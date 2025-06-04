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

impl PlayMessage {
    pub fn new(
        container: String,
        url: Option<String>,
        content: Option<String>,
        time: Option<f64>,
        speed: Option<f64>,
        headers: Option<HashMap<String, String>>,
    ) -> Self {
        Self {
            container,
            url,
            content,
            time,
            speed,
            headers,
        }
    }
}

#[derive(Deserialize, Debug)]
pub struct PlaybackUpdateMessage {
    #[serde(rename = "generationTime")]
    pub generation_time: u64,
    pub time: f64,
    pub duration: f64,
    pub speed: f64,
    pub state: u8, //0 = None, 1 = Playing, 2 = Paused
}
