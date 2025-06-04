use serde::{Deserialize, Serialize};

pub mod v2;
pub mod v3;

#[derive(Deserialize, Debug)]
pub struct PlaybackErrorMessage {
    pub message: String,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct VersionMessage {
    pub version: u64,
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
pub struct VolumeUpdateMessage {
    #[serde(rename = "generationTime")]
    pub generation_time: u64,
    pub volume: f64, //(0-1)
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
pub struct SeekMessage {
    pub time: f64,
}

impl SeekMessage {
    pub fn new(time: f64) -> Self {
        Self { time }
    }
}
