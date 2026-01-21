#[cfg(feature = "__schema")]
fn main() {
    use askama::Template;
    use fcast_protocol::{v4::*, PlaybackErrorMessage, VersionMessage};

    use schemars::{schema_for, JsonSchema};

    #[derive(JsonSchema)]
    #[allow(dead_code)]
    struct FCastProtocolV4Export {
        play_message: PlayMessage,
        seek_message: SeekMessage,
        volume_update_message: UpdateVolumeMessage,
        set_speed_message: SetSpeedMessage,
        version_message: VersionMessage,
        update_playback_state_message: UpdatePlaybackStateMessage,
        playback_error_message: PlaybackErrorMessage,
        position_changed_message: PositionChangedMessage,
        duration_changed_message: DurationChangedMessage,
        initial_sender_message: InitialSenderMessage,
        initial_receiver_message: InitialReceiverMessage,
        tracks_available_message: TracksAvailableMessage,
        change_track_message: ChangeTrackMessage,
        queue_insert_message: QueueInsertMessage,
        queue_remove_message: QueueRemoveMessage,
        queue_item_selected_message: QueueItemSelectedMessage,
        add_subtitle_source_message: AddSubtitleSourceMessage,
    }

    std::fs::write(
        "v4.scheme.json",
        serde_json::to_string_pretty(&schema_for!(FCastProtocolV4Export)).unwrap(),
    )
    .unwrap();

    #[derive(Template)]
    #[template(path = "v4_rust_code_block.md")]
    struct RustTypeTemplate {
        rust_type: String,
    }

    #[derive(Template)]
    #[template(path = "v4_docs.md")]
    struct V4DocumentationTemplate {
        play_message: RustTypeTemplate,
        seek_message: RustTypeTemplate,
        update_volume_message: RustTypeTemplate,
        set_speed_message: RustTypeTemplate,
        version_message: RustTypeTemplate,
        update_playback_state_message: RustTypeTemplate,
        playback_error_message: RustTypeTemplate,
        position_changed_message: RustTypeTemplate,
        duration_changed_message: RustTypeTemplate,
        initial_sender_message: RustTypeTemplate,
        initial_receiver_message: RustTypeTemplate,
        tracks_available_message: RustTypeTemplate,
        change_track_message: RustTypeTemplate,
        queue_insert_message: RustTypeTemplate,
        queue_remove_message: RustTypeTemplate,
        queue_item_selected_message: RustTypeTemplate,
        media_item: RustTypeTemplate,
        metadata: RustTypeTemplate,
        playback_state: RustTypeTemplate,
        queue_item: RustTypeTemplate,
        queue_position: RustTypeTemplate,
        media_track: RustTypeTemplate,
        media_track_metadata: RustTypeTemplate,
        track_type: RustTypeTemplate,
        device_info: RustTypeTemplate,
        receiver_capabilities: RustTypeTemplate,
        media_capabilities: RustTypeTemplate,
        display_capabilities: RustTypeTemplate,
        video_resolution: RustTypeTemplate,
        add_subtitle_source_message: RustTypeTemplate,
        set_status_update_interval_message: RustTypeTemplate,
    }

    fn strip_top_attribs(input: &str) -> String {
        input
            .lines()
            .filter(|line| !line.starts_with("#["))
            .map(|line| line.replace("pub ", ""))
            .collect::<Vec<_>>()
            .join("\n")
    }

    // stringify type
    macro_rules! st {
        ($typ:ident) => {
            strip_top_attribs($typ::type_string().trim_end_matches('\n'))
        };
    }

    // json schema template
    macro_rules! jt {
        ($typ:ident) => {
            RustTypeTemplate {
                rust_type: st!($typ),
                // schema_definition: serde_json::to_string_pretty(&schema_for!($typ)).unwrap(),
            }
        };
    }

    let doc = V4DocumentationTemplate {
        play_message: jt!(PlayMessage),
        seek_message: jt!(SeekMessage),
        update_volume_message: jt!(UpdateVolumeMessage),
        set_speed_message: jt!(SetSpeedMessage),
        version_message: jt!(VersionMessage),
        update_playback_state_message: jt!(UpdatePlaybackStateMessage),
        playback_error_message: jt!(PlaybackErrorMessage),
        position_changed_message: jt!(PositionChangedMessage),
        duration_changed_message: jt!(DurationChangedMessage),
        initial_sender_message: jt!(InitialSenderMessage),
        initial_receiver_message: jt!(InitialReceiverMessage),
        tracks_available_message: jt!(TracksAvailableMessage),
        change_track_message: jt!(ChangeTrackMessage),
        queue_insert_message: jt!(QueueInsertMessage),
        queue_remove_message: jt!(QueueRemoveMessage),
        queue_item_selected_message: jt!(QueueItemSelectedMessage),
        media_item: jt!(MediaItem),
        metadata: jt!(Metadata),
        playback_state: jt!(PlaybackState),
        queue_item: jt!(QueueItem),
        queue_position: jt!(QueuePosition),
        media_track: jt!(MediaTrack),
        media_track_metadata: jt!(MediaTrackMetadata),
        track_type: jt!(TrackType),
        device_info: jt!(DeviceInfo),
        receiver_capabilities: jt!(ReceiverCapabilities),
        media_capabilities: jt!(MediaCapabilities),
        display_capabilities: jt!(DisplayCapabilities),
        video_resolution: jt!(VideoResolution),
        add_subtitle_source_message: jt!(AddSubtitleSourceMessage),
        set_status_update_interval_message: jt!(SetStatusUpdateIntervalMessage),
    };

    std::fs::write(
        "../../../docs/docs/protocol/v4-draft.md",
        doc.render().unwrap(),
    )
    .unwrap();
}
