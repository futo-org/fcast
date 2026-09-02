use std::collections::HashMap;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Deserialize, Serialize)]
pub struct PlayMessage {
    pub container: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub content: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub time: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub speed: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
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

#[cfg(test)]
mod tests {
    use super::*;

    // Locks the None omission that replaced serde_with skip_serializing_none
    #[test]
    fn play_message_skips_none_fields() {
        let empty = PlayMessage {
            container: "video/mp4".to_string(),
            url: None,
            content: None,
            time: None,
            speed: None,
            headers: None,
        };
        assert_eq!(
            serde_json::to_string(&empty).unwrap(),
            r#"{"container":"video/mp4"}"#
        );
        assert_eq!(
            serde_json::from_str::<PlayMessage>(r#"{"container":"video/mp4"}"#).unwrap(),
            empty
        );

        let full = PlayMessage {
            container: "video/mp4".to_string(),
            url: Some("http://a".to_string()),
            content: Some("c".to_string()),
            time: Some(1.5),
            speed: Some(2.0),
            headers: Some(HashMap::from([("k".to_string(), "v".to_string())])),
        };
        let json = r#"{"container":"video/mp4","url":"http://a","content":"c","time":1.5,"speed":2.0,"headers":{"k":"v"}}"#;
        assert_eq!(serde_json::to_string(&full).unwrap(), json);
        assert_eq!(serde_json::from_str::<PlayMessage>(json).unwrap(), full);
    }
}
