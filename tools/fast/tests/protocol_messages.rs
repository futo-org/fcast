use std::collections::HashMap;

use fcast_protocol::{
    Opcode, PlaybackErrorMessage, PlaybackState, SeekMessage, SetSpeedMessage, SetVolumeMessage,
    VersionMessage, v2,
    v3::{
        AVCapabilities, ContentType, EventMessage, EventObject, EventSubscribeObject, EventType,
        InitialReceiverMessage, InitialSenderMessage, LivestreamCapabilities, MediaItem,
        MetadataObject, PlayMessage, PlayUpdateMessage, PlaybackUpdateMessage, PlaylistContent,
        ReceiverCapabilities, SetPlaylistItemMessage, SubscribeEventMessage,
        UnsubscribeEventMessage,
    },
};
use serde::{Serialize, de::DeserializeOwned};

fn round_trip<T>(value: T)
where
    T: Serialize + DeserializeOwned + PartialEq + std::fmt::Debug,
{
    let json = serde_json::to_string(&value).expect("serialize");
    let back: T = serde_json::from_str(&json)
        .unwrap_or_else(|e| panic!("deserialize failed for {json}: {e}"));
    assert_eq!(value, back, "round trip changed the value (json={json})");
}

fn headers() -> HashMap<String, String> {
    HashMap::from([
        ("User-Agent".to_owned(), "fast".to_owned()),
        ("Authorization".to_owned(), "Bearer x".to_owned()),
    ])
}

fn sample_v3_play() -> PlayMessage {
    PlayMessage {
        container: "video/mp4".to_owned(),
        url: Some("http://localhost/a".to_owned()),
        content: None,
        time: Some(3.0),
        volume: Some(0.5),
        speed: Some(1.5),
        headers: Some(headers()),
        metadata: Some(MetadataObject::Generic {
            title: Some("t".to_owned()),
            thumbnail_url: Some("u".to_owned()),
            custom: Some(serde_json::json!({ "k": 1 })),
        }),
    }
}

fn sample_media_item() -> MediaItem {
    MediaItem {
        container: "audio/mp3".to_owned(),
        url: Some("http://localhost/b".to_owned()),
        content: None,
        time: Some(1.0),
        volume: Some(0.25),
        speed: Some(2.0),
        cache: Some(true),
        show_duration: Some(10.0),
        headers: Some(headers()),
        metadata: Some(MetadataObject::Generic {
            title: None,
            thumbnail_url: None,
            custom: None,
        }),
    }
}

#[test]
fn version_message() {
    round_trip(VersionMessage { version: 3 });
    assert_eq!(
        serde_json::to_string(&VersionMessage { version: 2 }).unwrap(),
        r#"{"version":2}"#
    );
}

#[test]
fn playback_error_message() {
    round_trip(PlaybackErrorMessage {
        message: "boom".to_owned(),
    });
}

#[test]
fn set_volume_message() {
    round_trip(SetVolumeMessage { volume: 0.42 });
}

#[test]
fn set_speed_message() {
    round_trip(SetSpeedMessage { speed: 1.75 });
}

#[test]
fn seek_message() {
    round_trip(SeekMessage { time: 12.5 });
    assert_eq!(
        serde_json::to_string(&SeekMessage { time: 5.0 }).unwrap(),
        r#"{"time":5.0}"#
    );
}

#[test]
fn playback_state_repr() {
    assert_eq!(serde_json::to_string(&PlaybackState::Idle).unwrap(), "0");
    assert_eq!(serde_json::to_string(&PlaybackState::Playing).unwrap(), "1");
    assert_eq!(serde_json::to_string(&PlaybackState::Paused).unwrap(), "2");
    assert_eq!(
        serde_json::from_str::<PlaybackState>("2").unwrap(),
        PlaybackState::Paused
    );
}

#[test]
fn v2_play_message() {
    round_trip(v2::PlayMessage {
        container: "video/mp4".to_owned(),
        url: Some("http://localhost/a".to_owned()),
        content: None,
        time: Some(1.0),
        speed: Some(1.0),
        headers: Some(headers()),
    });
}

#[test]
fn v2_playback_update_message() {
    round_trip(v2::PlaybackUpdateMessage {
        generation_time: 100,
        time: 1.0,
        duration: 2.0,
        speed: 1.0,
        state: PlaybackState::Playing,
    });
}

#[test]
fn v2_volume_update_message() {
    round_trip(v2::VolumeUpdateMessage {
        generation_time: 100,
        volume: 0.9,
    });
}

#[test]
fn metadata_object_variants() {
    round_trip(MetadataObject::Generic {
        title: Some("title".to_owned()),
        thumbnail_url: Some("thumb".to_owned()),
        custom: Some(serde_json::json!({ "extra": [1, 2, 3] })),
    });
    round_trip(MetadataObject::Generic {
        title: None,
        thumbnail_url: None,
        custom: None,
    });
}

#[test]
fn v3_play_message_url_content_and_metadata() {
    round_trip(sample_v3_play());
    round_trip(PlayMessage {
        container: "application/dash+xml".to_owned(),
        url: None,
        content: Some("<MPD></MPD>".to_owned()),
        time: None,
        volume: None,
        speed: None,
        headers: None,
        metadata: None,
    });
}

#[test]
fn media_item_message() {
    round_trip(sample_media_item());
}

#[test]
fn media_item_from_play_message() {
    let play = sample_v3_play();
    let item: MediaItem = play.clone().into();
    assert_eq!(item.container, play.container);
    assert_eq!(item.url, play.url);
    assert_eq!(item.time, play.time);
    assert_eq!(item.volume, play.volume);
    assert_eq!(item.speed, play.speed);
    assert_eq!(item.headers, play.headers);
    assert_eq!(item.metadata, play.metadata);
    assert_eq!(item.cache, None);
    assert_eq!(item.show_duration, None);
}

#[test]
fn playlist_content_with_and_without_options() {
    round_trip(PlaylistContent {
        variant: ContentType::Playlist,
        items: vec![sample_media_item()],
        offset: Some(1),
        volume: Some(0.5),
        speed: Some(1.25),
        forward_cache: Some(2),
        backward_cache: Some(1),
        metadata: Some(MetadataObject::Generic {
            title: Some("pl".to_owned()),
            thumbnail_url: None,
            custom: None,
        }),
    });
    round_trip(PlaylistContent {
        variant: ContentType::Playlist,
        items: Vec::new(),
        offset: None,
        volume: None,
        speed: None,
        forward_cache: None,
        backward_cache: None,
        metadata: None,
    });
}

#[test]
fn v3_playback_update_message() {
    round_trip(PlaybackUpdateMessage {
        generation_time: 5,
        state: PlaybackState::Paused,
        time: Some(1.0),
        duration: Some(2.0),
        speed: Some(1.0),
        item_index: Some(3),
    });
    round_trip(PlaybackUpdateMessage {
        generation_time: 5,
        state: PlaybackState::Idle,
        time: None,
        duration: None,
        speed: None,
        item_index: None,
    });
}

#[test]
fn v3_playback_update_parses_v2_payload() {
    let v2 = v2::PlaybackUpdateMessage {
        generation_time: 7,
        time: 1.5,
        duration: 9.0,
        speed: 1.0,
        state: PlaybackState::Playing,
    };
    let json = serde_json::to_string(&v2).unwrap();
    let v3: PlaybackUpdateMessage = serde_json::from_str(&json).unwrap();
    assert_eq!(v3.generation_time, 7);
    assert_eq!(v3.state, PlaybackState::Playing);
    assert_eq!(v3.time, Some(1.5));
    assert_eq!(v3.item_index, None);
}

#[test]
fn initial_sender_message() {
    round_trip(InitialSenderMessage {
        display_name: Some("fast".to_owned()),
        app_name: Some("fast".to_owned()),
        app_version: Some("test".to_owned()),
    });
    round_trip(InitialSenderMessage::default());
}

#[test]
fn initial_receiver_message_with_capabilities() {
    round_trip(InitialReceiverMessage {
        display_name: Some("recv".to_owned()),
        app_name: Some("app".to_owned()),
        app_version: Some("1.0".to_owned()),
        play_data: Some(sample_v3_play()),
        experimental_capabilities: Some(ReceiverCapabilities {
            av: Some(AVCapabilities {
                livestream: Some(LivestreamCapabilities { whep: Some(true) }),
            }),
        }),
    });
    round_trip(InitialReceiverMessage::default());
}

#[test]
fn play_update_message() {
    round_trip(PlayUpdateMessage {
        generation_time: Some(1),
        play_data: Some(sample_v3_play()),
    });
    round_trip(PlayUpdateMessage {
        generation_time: None,
        play_data: None,
    });
}

#[test]
fn set_playlist_item_message() {
    round_trip(SetPlaylistItemMessage { item_index: 4 });
}

#[test]
fn event_subscribe_object_variants() {
    for event in [
        EventSubscribeObject::MediaItemStart,
        EventSubscribeObject::MediaItemEnd,
        EventSubscribeObject::MediaItemChanged,
        EventSubscribeObject::KeyDown {
            keys: vec!["Enter".to_owned(), "ArrowLeft".to_owned()],
        },
        EventSubscribeObject::KeyUp { keys: vec![] },
    ] {
        round_trip(SubscribeEventMessage {
            event: event.clone(),
        });
        round_trip(UnsubscribeEventMessage { event });
    }
}

#[test]
fn event_message_media_item_variants() {
    for variant in [
        EventType::MediaItemStart,
        EventType::MediaItemEnd,
        EventType::MediaItemChange,
    ] {
        round_trip(EventMessage {
            generation_time: 1,
            event: EventObject::MediaItem {
                variant,
                item: sample_media_item(),
            },
        });
    }
}

#[test]
fn event_message_key_variants() {
    for variant in [EventType::KeyDown, EventType::KeyUp] {
        round_trip(EventMessage {
            generation_time: 2,
            event: EventObject::Key {
                variant,
                key: "Enter".to_owned(),
                repeat: true,
                handled: false,
            },
        });
    }
}

#[test]
fn every_opcode_round_trips() {
    for code in 0u8..=21 {
        let op = Opcode::try_from(code)
            .unwrap_or_else(|e| panic!("opcode {code} should be defined: {e}"));
        assert_eq!(op as u8, code, "opcode {op:?} byte mismatch");
    }
    assert!(Opcode::try_from(22).is_err(), "22 should be unknown");
    assert!(Opcode::try_from(255).is_err(), "255 should be unknown");
}
