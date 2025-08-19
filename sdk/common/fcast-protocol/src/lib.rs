//! # FCast Protocol
//!
//! Implementation of the data models documented [here](https://gitlab.futo.org/videostreaming/fcast/-/wikis/Protocol-version-3).

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
    /// Not used
    None = 0,
    /// Sender message to play media content, body is [`v3::PlayMessage`]
    Play = 1,
    /// Sender message to pause media content, no body
    Pause = 2,
    /// Sender message to resume media content, no body
    Resume = 3,
    /// Sender message to stop media content, no body
    Stop = 4,
    /// Sender message to seek, body is [`SeekMessage`]
    Seek = 5,
    /// Receiver message to notify an updated playback state, body is [`v3::PlaybackUpdateMessage`]
    PlaybackUpdate = 6,
    /// Receiver message to notify when the volume has changed, body is [`VolumeUpdateMessage`]
    VolumeUpdate = 7,
    /// Sender message to change volume, body is [`SetVolumeMessage`]
    SetVolume = 8,
    /// Server message to notify the sender a playback error happened, body is [`PlaybackErrorMessage`]
    PlaybackError = 9,
    /// Sender message to change playback speed, body is [`SetSpeedMessage`]
    SetSpeed = 10,
    /// Message to notify the other of the current version, body is [`VersionMessage`]
    Version = 11,
    /// Message to get the other party to pong, no body
    Ping = 12,
    /// Message to respond to a ping from the other party, no body
    Pong = 13,
    /// Message to notify the other party of device information and state, body is InitialSenderMessage
    /// if receiver or [`v3::InitialReceiverMessage`] if sender
    Initial = 14,
    /// Receiver message to notify all senders when any device has sent a [`v3::PlayMessage`], body is
    /// [`v3::PlayUpdateMessage`]
    PlayUpdate = 15,
    /// Sender message to set the item index in a playlist to play content from, body is
    /// [`v3::SetPlaylistItemMessage`]
    SetPlaylistItem = 16,
    /// Sender message to subscribe to a receiver event, body is [`v3::SubscribeEventMessage`]
    SubscribeEvent = 17,
    /// Sender message to unsubscribe to a receiver event, body is [`v3::UnsubscribeEventMessage`]
    UnsubscribeEvent = 18,
    /// Receiver message to notify when a sender subscribed event has occurred, body is [`v3::EventMessage`]
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
    pub version: u8,
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
