use serde::{Deserialize, Serialize};

pub mod v2;
pub mod v3;

#[derive(Debug, thiserror::Error)]
pub enum TryFromByteError {
    #[error("Unknown opcode: {0}")]
    UnknownOpcode(u8),
}

#[derive(Debug, PartialEq, Copy, Clone)]
pub enum Opcode {
    None = 0,
    Play = 1,
    Pause = 2,
    Resume = 3,
    Stop = 4,
    Seek = 5,
    PlaybackUpdate = 6,
    VolumeUpdate = 7,
    SetVolume = 8,
    PlaybackError = 9,
    SetSpeed = 10,
    Version = 11,
    Ping = 12,
    Pong = 13,
    // V3:
    Initial = 14,
    PlayUpdate = 15,
    SetPlaylistItem = 16,
    SubscribeEvent = 17,
    UnsubscribeEvent = 18,
    Event = 19,
}

impl TryFrom<u8> for Opcode {
    type Error = TryFromByteError;

    fn try_from(value: u8) -> Result<Self, Self::Error> {
        Ok(match value {
            0 => Opcode::None,
            1 => Opcode::Play,
            2 => Opcode::Pause,
            3 => Opcode::Resume,
            4 => Opcode::Stop,
            5 => Opcode::Seek,
            6 => Opcode::PlaybackUpdate,
            7 => Opcode::VolumeUpdate,
            8 => Opcode::SetVolume,
            9 => Opcode::PlaybackError,
            10 => Opcode::SetSpeed,
            11 => Opcode::Version,
            12 => Opcode::Ping,
            13 => Opcode::Pong,
            14 => Opcode::Initial,
            15 => Opcode::PlayUpdate,
            16 => Opcode::SetPlaylistItem,
            17 => Opcode::SubscribeEvent,
            18 => Opcode::UnsubscribeEvent,
            19 => Opcode::Event,
            _ => return Err(TryFromByteError::UnknownOpcode(value)),
        })
    }
}

#[allow(dead_code)]
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

#[allow(dead_code)]
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

#[derive(Serialize, Debug)]
pub struct SeekMessage {
    pub time: f64,
}
