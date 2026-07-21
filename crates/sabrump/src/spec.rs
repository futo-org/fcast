//! Session configuration.

use crate::format::SabrFormat;
use crate::proto::ClientInfo;

/// Media role. Values match the wire/reference constants (`ROLE_VIDEO = 0`,
/// `ROLE_AUDIO = 1`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum Role {
    Video = 0,
    Audio = 1,
}

/// Everything needed to start a SABR session for one video.
#[derive(Debug, Clone)]
#[cfg_attr(feature = "serde", derive(serde::Serialize, serde::Deserialize))]
pub struct SabrStreamSpec {
    pub server_abr_streaming_url: String,
    /// Opaque config bytes. On the wire (serde) this is a base64 string so a
    /// sender can pass the plugin's base64 blob through unchanged.
    #[cfg_attr(feature = "serde", serde(with = "ustreamer_config_b64"))]
    pub ustreamer_config: Vec<u8>,
    pub video_id: String,
    pub is_live: bool,
    /// VOD duration in microseconds, or a non-positive value if unknown/live.
    pub duration_us: i64,
    pub video_formats: Vec<SabrFormat>,
    pub audio_formats: Vec<SabrFormat>,
    /// Base64-encoded PoToken (URL-safe or standard alphabet), if any.
    pub po_token: Option<String>,
    pub client_name: i32,
    pub client_version: String,
    pub os_name: String,
    pub os_version: String,
}

/// Serde adapter: (de)serialize `ustreamer_config` as a base64 string, decoding
/// leniently across the URL-safe/standard alphabets (padded or not).
#[cfg(feature = "serde")]
mod ustreamer_config_b64 {
    use base64::Engine;
    use base64::engine::general_purpose::{GeneralPurpose, STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
    use serde::{Deserialize, Deserializer, Serializer};

    pub fn serialize<S: Serializer>(value: &[u8], serializer: S) -> Result<S::Ok, S::Error> {
        serializer.serialize_str(&URL_SAFE_NO_PAD.encode(value))
    }

    pub fn deserialize<'de, D: Deserializer<'de>>(deserializer: D) -> Result<Vec<u8>, D::Error> {
        let s = String::deserialize(deserializer)?;
        const ENGINES: [GeneralPurpose; 4] = [URL_SAFE_NO_PAD, URL_SAFE, STANDARD_NO_PAD, STANDARD];
        for engine in ENGINES {
            if let Ok(bytes) = engine.decode(&s) {
                return Ok(bytes);
            }
        }
        Err(serde::de::Error::custom("invalid base64 ustreamer_config"))
    }
}

impl SabrStreamSpec {
    pub fn build_client_info(&self) -> ClientInfo {
        ClientInfo {
            client_name: self.client_name,
            client_version: self.client_version.clone(),
            os_name: self.os_name.clone(),
            os_version: self.os_version.clone(),
            ..Default::default()
        }
    }
}
