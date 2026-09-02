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
        };
        let json = r#"{"container":"video/mp4","url":"http://a","content":"c","time":1.5}"#;
        assert_eq!(serde_json::to_string(&full).unwrap(), json);
        assert_eq!(serde_json::from_str::<PlayMessage>(json).unwrap(), full);
    }
}
