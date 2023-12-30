use serde::{Serialize, Deserialize};

#[derive(Serialize, Debug)]
pub struct PlayMessage {
    pub container: String,
    pub url: Option<String>,
    pub content: Option<String>,
    pub time: Option<f64>,
    pub speed: Option<f64>
}

impl PlayMessage {
    pub fn new(container: String, url: Option<String>, content: Option<String>, time: Option<f64>, speed: Option<f64>) -> Self {
        Self { container, url, content, time, speed }
    }
}

#[derive(Serialize, Debug)]
pub struct SeekMessage {
    pub time: f64,
}

impl SeekMessage {
    pub fn new(time: f64) -> Self {
        Self { time }
    }
}

#[derive(Deserialize, Debug)]
pub struct PlaybackUpdateMessage {
    #[serde(rename = "generationTime")]
    pub generation_time: u64,
    pub time: f64,
    pub duration: f64,
    pub speed: f64,
    pub state: u8 //0 = None, 1 = Playing, 2 = Paused
}

#[derive(Deserialize, Debug)]
pub struct VolumeUpdateMessage {
    #[serde(rename = "generationTime")]
    pub generation_time: u64,
    pub volume: f64 //(0-1)
}

#[derive(Serialize, Debug)]
pub struct SetVolumeMessage {
    pub volume: f64,
}

impl SetVolumeMessage {
    pub fn new(volume: f64) -> Self {
        Self { volume }
    }
}

#[derive(Serialize, Debug)]
pub struct SetSpeedMessage {
    pub speed: f64,
}

impl SetSpeedMessage {
    pub fn new(speed: f64) -> Self {
        Self { speed }
    }
}

#[derive(Deserialize, Debug)]
pub struct PlaybackErrorMessage {
    pub message: String,
}

#[derive(Deserialize, Debug)]
pub struct VersionMessage {
    pub version: u64,
}