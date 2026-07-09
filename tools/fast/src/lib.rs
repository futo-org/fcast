use fcast_protocol::{
    v3,
    v4::{QueuePosition, flat::ErrorKind},
};

pub mod engine;

#[derive(Debug)]
pub struct PlaylistItem {
    pub file_id: u32,
}

#[derive(Debug)]
pub enum Send {
    Version(u64),
    Initial,
    Ping,
    SetVolume(f64),
    SetSpeed(f64),
    Seek(f64),
    Stop,
    PlayV2 {
        file_id: u32,
    },
    PlayV3 {
        file_id: u32,
    },
    PlayV3WithBody {
        file_id: u32,
        time: Option<f64>,
        volume: Option<f64>,
        speed: Option<f64>,
    },
    PlayV3WithMetadata {
        file_id: u32,
        title: Option<&'static str>,
        thumbnail_url: Option<&'static str>,
    },
    PlayContent {
        mime: &'static str,
        content: &'static str,
    },
    Pause,
    Resume,
    SubscribeEvent(v3::EventSubscribeObject),
    UnsubscribeEvent(v3::EventSubscribeObject),
    PlaylistV3 {
        items: &'static [PlaylistItem],
    },
    PlaylistV3WithOptions {
        items: &'static [PlaylistItem],
        offset: Option<u64>,
        volume: Option<f64>,
        speed: Option<f64>,
    },
    SetPlaylistItem {
        index: u64,
    },
    SenderIntroduction,
    PlayV4 {
        file_id: u32,
    },
    PlayFakeUrlV4 {
        container: &'static str,
    },
    LoadQueueV4 {
        items: &'static [PlaylistItem],
        start_index: Option<u8>,
    },
    QueueInsertV4 {
        file_id: u32,
        position: QueuePosition,
    },
    QueueRemoveV4 {
        position: QueuePosition,
    },
    QueueSelectV4 {
        position: QueuePosition,
    },
    SetVolumeV4(f64),
    SetSpeedV4(f64),
    SetVolumeV4Raw(f64),
    SetSpeedV4Raw(f64),
    SetProgressIntervalV4 {
        millis: u64,
    },
    EmptyProgressIntervalV4,
    LoadQueueRepeatV4 {
        file_id: u32,
        count: u32,
        start_index: Option<u8>,
    },
    ErrorV4(ErrorKind),
    CompanionHelloResponseV4,
    ReceiverIntroductionV4,
    SeekV4(f64),
    EmptySeekV4,
    PauseV4,
    ResumeV4,
    StopV4,
    ChangeTrack {
        kind: TrackKind,
        index: Option<usize>,
    },
    ChangeTracks(&'static [(TrackKind, Option<usize>)]),
    ChangeTrackNoExpect {
        kind: TrackKind,
        index: usize,
    },
    ChangeTrackRawId {
        kind: TrackKind,
        id: u32,
    },
    ChangeTrackMismatched {
        send_as: TrackKind,
        take_from: TrackKind,
        index: usize,
    },
    /// Attach an external subtitle (served file) to the playing item.
    AddSubtitleSourceV4 {
        file_id: u32,
        select: bool,
        name: Option<&'static str>,
    },
    /// Attach an external subtitle pointing at a well-formed URL that 404s.
    AddSubtitleSourceFakeUrlV4 {
        select: bool,
    },
    /// `AddSubtitleSource` with an empty URL: the spec gives it no meaning,
    /// so the receiver must reject it as a malformed body.
    AddSubtitleSourceEmptyUrlV4,
    RawMessage {
        opcode: u8,
        body: &'static [u8],
    },
    CompanionHello,
    ServeCompanionFile {
        resource_id: u32,
        path: &'static str,
        mime: &'static str,
    },
    PlayCompanion {
        resource_id: u32,
    },
    PlayCompanionMissing {
        resource_id: u32,
        container: &'static str,
    },
    RawOpcode(u8),
}

#[derive(Debug)]
pub enum Receive {
    Version,
    Initial,
    Pong,
    Volume,
    PlaybackUpdate,
    ReceiverIntroduction,
    Error(ErrorKind),
    VolumeChangedV4(f64),
    SpeedChangedV4(f64),
    ProgressV4AtLeast(f64),
    /// The next v4 progress update with a non-zero position must be at least
    /// this many seconds. Verifies the position survived a pipeline reload
    /// (an early low value fails immediately instead of waiting for playback
    /// to catch up).
    NextProgressV4AtLeast(f64),
}

#[derive(Debug)]
pub enum Step {
    Send(Send),
    Receive(Receive),
    ServeFile {
        path: &'static str,
        id: u32,
        mime: &'static str,
        headers: Option<&'static [(&'static str, &'static str)]>,
    },
    SleepMillis(u64),
    MeasureProgressInterval {
        expected_ms: u64,
        tolerance_ms: u64,
        samples: usize,
    },
    ExpectClosed,
    AwaitTracks {
        video: usize,
        audio: usize,
        subtitle: usize,
    },
    AssertTrackState {
        video: Option<usize>,
        audio: Option<usize>,
        subtitle: Option<usize>,
    },
    /// Wait until the relayed track state matches AND holds steady for a
    /// short while. Interim relays during an external-subtitle reload dance
    /// (e.g. the mid-preroll text deselect) are churn to wait out, not
    /// failures — and how long the dance takes varies too much for a fixed
    /// sleep.
    AwaitTrackState {
        video: Option<usize>,
        audio: Option<usize>,
        subtitle: Option<usize>,
    },
    /// The most recent `TracksAvailable` must advertise exactly these many
    /// tracks of each kind.
    AssertTrackCounts {
        video: usize,
        audio: usize,
        subtitle: usize,
    },
    /// The most recently relayed `PlaybackStateChanged` must match.
    AssertPlaybackStateV4(fcast_protocol::v4::flat::PlaybackState),
    /// Wait until the relayed playback state matches AND holds steady for a
    /// short while. The external-subtitle dance's re-pause lands whenever
    /// playsink finishes its un-signalled text-branch churn — too variable
    /// for a fixed sleep followed by a point-in-time assert.
    AwaitPlaybackState(fcast_protocol::v4::flat::PlaybackState),
    OpenSecondSender,
    SetSecondSenderInterval {
        millis: u64,
    },
    /// Wait until the second sender has been sent a `TracksAvailable`
    /// advertising at least these many tracks of each kind.
    AwaitTracksOnSecondSender {
        video: usize,
        audio: usize,
        subtitle: usize,
    },
    /// `AddSubtitleSource` sent from the second sender.
    AddSubtitleSourceOnSecondSenderV4 {
        file_id: u32,
        select: bool,
        name: Option<&'static str>,
    },
    /// `ChangeTrack` sent from the second sender for the `index`th track it
    /// was advertised (`None` = disable the kind), waiting for the relayed
    /// confirmation on the second connection.
    ChangeTrackOnSecondSender {
        kind: TrackKind,
        index: Option<usize>,
    },
    /// The most recent `ChangeTrack` relayed to the second sender must match.
    AssertTrackStateOnSecondSender {
        video: Option<usize>,
        audio: Option<usize>,
        subtitle: Option<usize>,
    },
    ExpectLoadOnSecondSender,
    ExpectVolumeOnSecondSender(f64),
    ExpectQueueMutationOnSecondSender(QueueMutationKind),
    MeasureProgressBothSenders {
        a_expected_ms: u64,
        b_expected_ms: u64,
        tolerance_ms: u64,
        samples: usize,
    },
}

#[derive(Debug, Clone, Copy)]
pub enum QueueMutationKind {
    Insert,
    Remove,
    Select,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video = 0,
    Audio = 1,
    Subtitle = 2,
}

pub struct TestCase {
    pub name: &'static str,
    pub steps: &'static [Step],
}

macro_rules! cases {
    ($($case:ident),*) => {
        pub const TEST_CASES: &[TestCase] = &[
            $($case(),)*
        ];
    }
}

cases!(
    connect_version_2,
    connect_version_3,
    heartbeat,
    cast_photo_v2,
    cast_photos_v2,
    cast_photo_v3,
    cast_photos_v3,
    cast_video_v2,
    // cast_video_set_volume_v2,
    cast_video_v3,
    // cast_video_set_volume_v3,
    cast_pause_resume_v2,
    cast_pause_resume_v3,
    cast_pause_during_load_v2,
    subscribe_media_item_start_1,
    subscribe_media_item_start_2,
    subscribe_media_item_end,
    cast_simple_playlist,
    cast_photo_with_headers_v2,
    cast_photos_with_headers_v2,
    cast_photo_with_headers_v3,
    cast_photos_with_headers_v3,
    cast_video_with_headers_v2,
    cast_video_with_headers_v3,
    cast_simple_playlist_with_headers,
    cast_video_with_start_speed_volume_v3,
    connect_version_4,
    heartbeat_v4,
    cast_video_v4,
    cast_fake_image_url_resource_not_found_v4,
    cast_fake_video_url_resource_not_found_v4,
    cast_queue_v4,
    cast_queue_with_headers_v4,
    cast_queue_insert_remove_v4,
    cast_queue_select_no_load_v4,
    cast_queue_select_out_of_range_v4,
    cast_queue_remove_current_v4,
    cast_pause_resume_v4,
    cast_pause_during_load_v4,
    subtitle_change_and_disable_v4,
    subtitle_change_keeps_playing_v4,
    subtitle_bitmap_change_then_load_v4,
    subtitle_disable_while_paused_v4,
    video_disable_with_subs_v4,
    audio_track_switch_v4,
    audio_disable_enable_v4,
    track_change_rejections_v4,
    rapid_track_changes_v4,
    external_sub_add_while_playing_v4,
    external_sub_add_while_paused_v4,
    external_sub_add_unselected_v4,
    external_sub_keeps_embedded_selection_v4,
    external_sub_switch_embedded_external_v4,
    external_sub_add_two_selected_v4,
    external_sub_switch_between_externals_v4,
    external_sub_seek_v4,
    external_sub_empty_url_malformed_v4,
    external_sub_add_invalid_url_v4,
    external_sub_no_media_rejected_v4,
    // cast_video_set_volume_v4,
    cast_video_set_speed_v4,
    cast_seek_v4,
    seek_while_paused_sends_progress_v4,
    cast_companion_photo_v4,
    cast_companion_video_v4,
    cast_companion_video_and_seek_v4,
    cast_companion_audio_v4,
    cast_companion_missing_image_resource_not_found_v4,
    cast_companion_missing_video_resource_not_found_v4,
    invalid_opcode_error_v4,
    unsupported_opcode_error_v4,
    cast_companion_empty_seek,
    cast_video_progress_interval_v4,
    version_zero_closes,
    version_downgrade_v5_to_v4,
    flatbuf_before_handshake_closes,
    ping_before_version_closes,
    garbage_flatbuf_closes_v4,
    truncated_flatbuf_closes_v4,
    wrong_direction_messages_v4,
    none_opcode_error_v4,
    empty_progress_interval_malformed_v4,
    volume_clamped_high_v4,
    volume_clamped_low_v4,
    set_speed_extremes_v4,
    seek_far_beyond_duration_v4,
    progress_interval_min_clamp_v4,
    queue_load_no_start_index_v4,
    queue_full_v4,
    queue_insert_front_v4,
    multi_sender_load_broadcast_v4,
    multi_sender_volume_broadcast_v4,
    multi_sender_progress_isolation_v4,
    multi_sender_queue_insert_broadcast_v4,
    multi_sender_queue_remove_broadcast_v4,
    multi_sender_queue_select_broadcast_v4,
    multi_sender_external_subs_v4,
    seek_v3,
    unsubscribe_event_v3
);

macro_rules! define_test_case {
    ($name:ident, $steps:expr) => {
        pub const fn $name() -> TestCase {
            TestCase {
                name: stringify!($name),
                steps: $steps,
            }
        }
    };
}

macro_rules! send {
    ($op:expr) => {
        Step::Send($op)
    };
}

macro_rules! recv {
    ($op:expr) => {
        Step::Receive($op)
    };
}

macro_rules! serve {
    ($path:expr, $id:expr, $mime:expr) => {
        Step::ServeFile {
            path: $path,
            id: $id,
            mime: $mime,
            headers: None,
        }
    };
    ($path:expr, $id:expr, $mime:expr, $headers:expr) => {
        Step::ServeFile {
            path: $path,
            id: $id,
            mime: $mime,
            headers: Some(&($headers)),
        }
    };
}

define_test_case!(
    connect_version_2,
    &[
        recv!(Receive::Version), //
        send!(Send::Version(2)), //
    ]
);

define_test_case!(
    connect_version_3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial)
    ]
);

define_test_case!(
    heartbeat,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        send!(Send::Ping),
        recv!(Receive::Pong),
        send!(Send::Ping),
        recv!(Receive::Pong),
        send!(Send::Ping),
        recv!(Receive::Pong)
    ]
);

define_test_case!(
    cast_photo_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_photos_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        serve!("image/garden.jpg", 1, "image/jpeg"),
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::PlayV2 { file_id: 1 }),
        Step::SleepMillis(500),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_photo_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        send!(Send::PlayV3 { file_id: 0 }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_photos_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        serve!("image/garden.jpg", 1, "image/jpeg"),
        send!(Send::PlayV2 { file_id: 0 }),
        send!(Send::PlayV2 { file_id: 1 }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_video_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(750),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_video_set_volume_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(750),
        send!(Send::SetVolume(0.5)),
        send!(Send::SetVolume(1.0)),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_video_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        send!(Send::SubscribeEvent(
            v3::EventSubscribeObject::MediaItemStart
        )),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV3 { file_id: 0 }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_video_set_volume_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV3 { file_id: 0 }),
        send!(Send::SetVolume(0.5)),
        send!(Send::SetVolume(1.0)),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_pause_resume_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::Pause),
        send!(Send::Resume),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_pause_resume_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV3 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::Pause),
        send!(Send::Resume),
        send!(Send::Stop),
    ]
);

// A pause sent right behind the cast lands while the load is still in
// flight; the receiver must not let the load's collection-time auto-play
// swallow it (loads always go through Playing and re-pause afterwards).
define_test_case!(
    cast_pause_during_load_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        serve!("video/video_with_subs.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV2 { file_id: 0 }),
        send!(Send::Pause),
        // The load's buffering dance posts a transient Paused before the
        // collection-time auto-play; the pause only counts if it HOLDS.
        Step::AwaitPlaybackState(fcast_protocol::v4::flat::PlaybackState::Paused),
        send!(Send::Resume),
        Step::AwaitPlaybackState(fcast_protocol::v4::flat::PlaybackState::Playing),
        send!(Send::Stop),
    ]
);

define_test_case!(
    subscribe_media_item_start_1,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        Step::SleepMillis(500), // Electron receiver workaround
        send!(Send::SubscribeEvent(
            v3::EventSubscribeObject::MediaItemStart
        )),
        Step::SleepMillis(500), // Electron receiver workaround
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        send!(Send::PlayV3 { file_id: 0 }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_photo_with_headers_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        serve!(
            "image/flowers.jpg",
            0,
            "image/jpeg",
            [("User-Agent", "Fake"), ("Custom-Key", "ABC")]
        ),
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_photos_with_headers_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        serve!(
            "image/flowers.jpg",
            0,
            "image/jpeg",
            [("User-Agent", "Fake"), ("Custom-Key", "ABC")]
        ),
        serve!(
            "image/garden.jpg",
            1,
            "image/jpeg",
            [("User-Agent", "Fake"), ("Custom-Key", "ABC")]
        ),
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::PlayV2 { file_id: 1 }),
        Step::SleepMillis(500),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_photo_with_headers_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        serve!(
            "image/flowers.jpg",
            0,
            "image/jpeg",
            [("User-Agent", "Fake"), ("Custom-Key", "ABC")]
        ),
        send!(Send::PlayV3 { file_id: 0 }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_photos_with_headers_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        serve!(
            "image/flowers.jpg",
            0,
            "image/jpeg",
            [("User-Agent", "Fake"), ("Custom-Key", "ABC")]
        ),
        serve!(
            "image/garden.jpg",
            1,
            "image/jpeg",
            [("User-Agent", "Fake"), ("Custom-Key", "ABC")]
        ),
        send!(Send::SubscribeEvent(
            v3::EventSubscribeObject::MediaItemStart
        )),
        send!(Send::PlayV3 { file_id: 0 }),
        send!(Send::PlayV3 { file_id: 1 }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_video_with_headers_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        serve!(
            "video/BigBuckBunny.mp4",
            0,
            "video/mp4",
            [("User-Agent", "Fake"), ("Custom-Key", "ABC")]
        ),
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(750),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_video_with_headers_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        send!(Send::SubscribeEvent(
            v3::EventSubscribeObject::MediaItemStart
        )),
        serve!(
            "video/BigBuckBunny.mp4",
            0,
            "video/mp4",
            [("User-Agent", "Fake"), ("Custom-Key", "ABCDEF")]
        ),
        send!(Send::PlayV3 { file_id: 0 }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    subscribe_media_item_start_2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        Step::SleepMillis(500),
        send!(Send::SubscribeEvent(
            v3::EventSubscribeObject::MediaItemStart
        )),
        Step::SleepMillis(500),
        serve!("audio/Court_House_Blues_Take_1.mp3", 0, "audio/mp3"),
        send!(Send::PlayV3 { file_id: 0 }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    subscribe_media_item_end,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        Step::SleepMillis(500),
        send!(Send::SubscribeEvent(v3::EventSubscribeObject::MediaItemEnd)),
        Step::SleepMillis(500),
        serve!("audio/Dont_Go_Way_Nobody.mp3", 0, "audio/mp3"),
        send!(Send::PlayV3WithBody {
            file_id: 0,
            time: Some(243.0),
            speed: None,
            volume: None,
        }),
        Step::SleepMillis(1250),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_simple_playlist,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        Step::SleepMillis(500),
        send!(Send::SubscribeEvent(
            v3::EventSubscribeObject::MediaItemChanged
        )),
        Step::SleepMillis(500),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        serve!("image/garden.jpg", 1, "image/jpeg"),
        send!(Send::PlaylistV3 {
            items: &[PlaylistItem { file_id: 0 }, PlaylistItem { file_id: 1 },]
        }),
        Step::SleepMillis(500),
        send!(Send::SetPlaylistItem { index: 1 }),
        Step::SleepMillis(500),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_simple_playlist_with_headers,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        send!(Send::SubscribeEvent(
            v3::EventSubscribeObject::MediaItemChanged
        )),
        serve!(
            "image/flowers.jpg",
            0,
            "image/jpeg",
            [("User-Agent", "Fake"), ("Custom-Key", "ABC")]
        ),
        send!(Send::PlaylistV3 {
            items: &[PlaylistItem { file_id: 0 },]
        }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_video_with_start_speed_volume_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        send!(Send::SubscribeEvent(
            v3::EventSubscribeObject::MediaItemStart,
        )),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV3WithBody {
            file_id: 0,
            time: Some(10.0),
            speed: Some(1.5),
            volume: Some(0.5),
        }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    connect_version_4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
    ]
);

define_test_case!(
    heartbeat_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::Ping),
        recv!(Receive::Pong),
        send!(Send::Ping),
        recv!(Receive::Pong),
        send!(Send::Ping),
        recv!(Receive::Pong),
    ]
);

define_test_case!(
    cast_video_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(750),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_queue_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        serve!("image/garden.jpg", 1, "image/jpeg"),
        send!(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }, PlaylistItem { file_id: 1 }],
            start_index: Some(0),
        }),
        Step::SleepMillis(750),
        send!(Send::QueueSelectV4 {
            position: QueuePosition::Index(1),
        }),
        Step::SleepMillis(500),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_fake_image_url_resource_not_found_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::PlayFakeUrlV4 {
            container: "image/jpeg",
        }),
        recv!(Receive::Error(ErrorKind::ResourceNotFound)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_fake_video_url_resource_not_found_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::PlayFakeUrlV4 {
            container: "video/mp4",
        }),
        recv!(Receive::Error(ErrorKind::ResourceNotFound)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_queue_with_headers_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!(
            "image/flowers.jpg",
            0,
            "image/jpeg",
            [("Custom-Key", "ABC")]
        ),
        serve!(
            "video/BigBuckBunny.mp4",
            1,
            "video/mp4",
            [("Custom-Key", "video-ABC")]
        ),
        send!(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }, PlaylistItem { file_id: 1 }],
            start_index: Some(0),
        }),
        Step::SleepMillis(750),
        send!(Send::QueueSelectV4 {
            position: QueuePosition::Index(1),
        }),
        Step::SleepMillis(750),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_queue_insert_remove_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        serve!("image/garden.jpg", 1, "image/jpeg"),
        send!(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }],
            start_index: Some(0),
        }),
        Step::SleepMillis(500),
        send!(Send::QueueInsertV4 {
            file_id: 1,
            position: QueuePosition::Back,
        }),
        send!(Send::QueueSelectV4 {
            position: QueuePosition::Index(1),
        }),
        Step::SleepMillis(500),
        send!(Send::QueueRemoveV4 {
            position: QueuePosition::Index(0),
        }),
        Step::SleepMillis(500),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_queue_select_no_load_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::QueueSelectV4 {
            position: QueuePosition::Back,
        }),
        recv!(Receive::Error(ErrorKind::InvalidState)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_queue_select_out_of_range_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        send!(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }],
            start_index: Some(0),
        }),
        Step::SleepMillis(500),
        send!(Send::QueueSelectV4 {
            position: QueuePosition::Index(1),
        }),
        recv!(Receive::Error(ErrorKind::QueuePositionOutOfRange)),
        send!(Send::QueueSelectV4 {
            position: QueuePosition::Index(2),
        }),
        recv!(Receive::Error(ErrorKind::QueuePositionOutOfRange)),
        send!(Send::QueueSelectV4 {
            position: QueuePosition::Index(100),
        }),
        recv!(Receive::Error(ErrorKind::QueuePositionOutOfRange)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_queue_remove_current_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        send!(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }],
            start_index: Some(0),
        }),
        Step::SleepMillis(500),
        send!(Send::QueueRemoveV4 {
            position: QueuePosition::Index(0),
        }),
        recv!(Receive::Error(ErrorKind::QueueRemovePlayingItem)),
        send!(Send::QueueRemoveV4 {
            position: QueuePosition::Front,
        }),
        recv!(Receive::Error(ErrorKind::QueueRemovePlayingItem)),
        send!(Send::QueueRemoveV4 {
            position: QueuePosition::Back,
        }),
        recv!(Receive::Error(ErrorKind::QueueRemovePlayingItem)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_video_set_volume_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::SetVolumeV4(0.5)),
        send!(Send::SetVolumeV4(1.0)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_video_set_speed_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::SetSpeedV4(1.5)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_pause_resume_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::PauseV4),
        send!(Send::ResumeV4),
        send!(Send::StopV4),
    ]
);

// v4 twin of `cast_pause_during_load_v2`: SetPlaybackState(Paused) racing
// the load must survive the collection-time auto-play.
define_test_case!(
    cast_pause_during_load_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_with_subs.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 0 }),
        send!(Send::PauseV4),
        Step::AwaitPlaybackState(fcast_protocol::v4::flat::PlaybackState::Paused),
        send!(Send::ResumeV4),
        Step::AwaitPlaybackState(fcast_protocol::v4::flat::PlaybackState::Playing),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    subtitle_change_and_disable_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_with_subs.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 3,
        },
        // Switch to the third subtitle track; the receiver must relay a
        // ChangeTrack echoing the id it actually selected.
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: Some(2),
        }),
        // Disable subtitles entirely; the receiver must relay a ChangeTrack
        // with a null id to confirm the track was deselected.
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: None,
        }),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    subtitle_disable_while_paused_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_with_subs.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 3,
        },
        // Let playback establish with a subtitle cue on screen.
        Step::SleepMillis(1200),
        send!(Send::PauseV4),
        Step::SleepMillis(300),
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: None,
        }),
        Step::SleepMillis(500),
        send!(Send::ResumeV4),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    subtitle_change_keeps_playing_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_with_subs.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 3,
        },
        // Let playback establish (become seekable and advance a bit).
        Step::SleepMillis(1200),
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: Some(2),
        }),
        recv!(Receive::ProgressV4AtLeast(3.0)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    subtitle_bitmap_change_then_load_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_with_vobsub.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 1,
        },
        // Let playback establish in steady PLAYING and the plain-load text
        // restore settle (the restore auto-selects the first text stream —
        // the VOBSUB track — so a direct select would be a no-op).
        Step::SleepMillis(2000),
        // Deselect first so the re-select below is a genuine change.
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: None,
        }),
        Step::SleepMillis(500),
        // Re-select the VOBSUB track: bitmap subtitles (subpicture/*) are
        // composited into the VIDEO chain, so playsink splices dvdspu into
        // the video path — an unsignalled video-chain rebuild. The
        // subtitle re-emit flush racing that rebuild has deadlocked the
        // pipeline for good (flush-start lost at a deactivating pad, video
        // sink clock-waiting on the lost-state-frozen audio clock, seek
        // thread stuck on the stream lock), killing the player worker.
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: Some(0),
        }),
        // Long enough for the DEFERRED re-emit flush (selection confirm
        // + SUBPICTURE_REFRESH_DELAY + a poll tick) to dispatch and settle
        // inside the observation window.
        Step::SleepMillis(3500),
        // A dead worker swallows every later job: prove a follow-up load
        // still completes.
        serve!("video/video_multi_track.mkv", 1, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 1 }),
        Step::AwaitPlaybackState(fcast_protocol::v4::flat::PlaybackState::Playing),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    video_disable_with_subs_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_with_subs.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 3,
        },
        // Let playback establish with the default selection (video + audio +
        // first subtitle track).
        Step::SleepMillis(1200),
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        send!(Send::ChangeTrack {
            kind: TrackKind::Video,
            index: None,
        }),
        Step::SleepMillis(500),
        Step::AssertTrackState {
            video: None,
            audio: Some(0),
            subtitle: None,
        },
        // Audio-only playback must keep advancing.
        recv!(Receive::ProgressV4AtLeast(3.0)),
        // Re-enabling video must come back cleanly; subtitles stay off.
        send!(Send::ChangeTrack {
            kind: TrackKind::Video,
            index: Some(0),
        }),
        Step::SleepMillis(500),
        Step::AssertTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: None,
        },
        send!(Send::StopV4),
    ]
);

define_test_case!(
    audio_track_switch_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_multi_track.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 2,
            subtitle: 2,
        },
        Step::SleepMillis(1000),
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        // Switch to the second audio track; the receiver must relay the
        // change and playback must keep advancing.
        send!(Send::ChangeTrack {
            kind: TrackKind::Audio,
            index: Some(1),
        }),
        recv!(Receive::ProgressV4AtLeast(3.0)),
        Step::AssertTrackState {
            video: Some(0),
            audio: Some(1),
            subtitle: Some(0),
        },
        send!(Send::StopV4),
    ]
);

define_test_case!(
    audio_disable_enable_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_with_subs.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 3,
        },
        Step::SleepMillis(1000),
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        send!(Send::ChangeTrack {
            kind: TrackKind::Audio,
            index: None,
        }),
        recv!(Receive::ProgressV4AtLeast(3.0)),
        // Re-enabling audio must come back cleanly.
        send!(Send::ChangeTrack {
            kind: TrackKind::Audio,
            index: Some(0),
        }),
        Step::SleepMillis(500),
        Step::AssertTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        send!(Send::StopV4),
    ]
);

define_test_case!(
    track_change_rejections_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_with_subs.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 3,
        },
        Step::SleepMillis(800),
        // An id that was never advertised must be rejected, not silently
        // deselect the whole track type.
        send!(Send::ChangeTrackRawId {
            kind: TrackKind::Audio,
            id: 99,
        }),
        recv!(Receive::Error(ErrorKind::MalformedBody)),
        // As must an advertised id of the wrong kind.
        send!(Send::ChangeTrackMismatched {
            send_as: TrackKind::Audio,
            take_from: TrackKind::Video,
            index: 0,
        }),
        recv!(Receive::Error(ErrorKind::MalformedBody)),
        send!(Send::ChangeTrack {
            kind: TrackKind::Video,
            index: None,
        }),
        Step::SleepMillis(500),
        send!(Send::ChangeTrackNoExpect {
            kind: TrackKind::Subtitle,
            index: 0,
        }),
        recv!(Receive::Error(ErrorKind::InvalidState)),
        // None of the rejected requests may have disturbed the selection.
        Step::AssertTrackState {
            video: None,
            audio: Some(0),
            subtitle: None,
        },
        send!(Send::StopV4),
    ]
);

define_test_case!(
    rapid_track_changes_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_multi_track.mkv", 0, "video/x-matroska"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 2,
            subtitle: 2,
        },
        Step::SleepMillis(1000),
        // Back-to-back changes of different kinds must compose: the second
        // selection is rebuilt while the first is still unconfirmed and must
        // not revert it.
        send!(Send::ChangeTracks(&[
            (TrackKind::Audio, Some(1)),
            (TrackKind::Subtitle, Some(1)),
        ])),
        Step::SleepMillis(700),
        Step::AssertTrackState {
            video: Some(0),
            audio: Some(1),
            subtitle: Some(1),
        },
        send!(Send::StopV4),
    ]
);

define_test_case!(
    external_sub_add_while_playing_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        serve!("subs/sample_en.srt", 1, "application/x-subrip"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1000),
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        // Let playback establish and advance so position preservation across
        // the suburi reload is observable (well past the engine's 1.5s
        // transient-position threshold).
        recv!(Receive::ProgressV4AtLeast(4.0)),
        send!(Send::AddSubtitleSourceV4 {
            file_id: 1,
            select: true,
            name: Some("English (external)"),
        }),
        // The reload re-advertises the tracks; the external text stream
        // arrives in a second stream collection.
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 1,
        },
        // Playback must come back at (or beyond) the pre-add position.
        recv!(Receive::NextProgressV4AtLeast(3.0)),
        // The external track must have been selected and confirmed.
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        send!(Send::StopV4),
    ]
);

define_test_case!(
    external_sub_add_while_paused_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        serve!("subs/sample_en.srt", 1, "application/x-subrip"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1500),
        send!(Send::PauseV4),
        Step::SleepMillis(300),
        send!(Send::AddSubtitleSourceV4 {
            file_id: 1,
            select: true,
            name: None,
        }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 1,
        },
        // The restore sequence (settle, subtitle selection, settle delay,
        // position seek, re-pause) takes a while and its tail varies too
        // much for a fixed sleep — wait for the re-pause to land and hold.
        Step::AwaitPlaybackState(fcast_protocol::v4::flat::PlaybackState::Paused),
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        // Resuming afterwards must work and playback must advance.
        send!(Send::ResumeV4),
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        recv!(Receive::ProgressV4AtLeast(2.0)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    external_sub_add_unselected_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        serve!("subs/sample_en.srt", 1, "application/x-subrip"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1000),
        // "Add but don't show": decodebin3 auto-selects a lone text stream,
        // so the receiver must deselect it once it appears.
        send!(Send::AddSubtitleSourceV4 {
            file_id: 1,
            select: false,
            name: Some("English"),
        }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 1,
        },
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: None,
        },
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        recv!(Receive::ProgressV4AtLeast(2.0)),
        send!(Send::StopV4),
    ]
);

// Two externals attached one after another (both selected). The second does
// NOT replace the first — both are advertised, and the most recently selected
// one (Spanish, index 1) is the active selection.
define_test_case!(
    external_sub_add_two_selected_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        serve!("subs/sample_en.srt", 1, "application/x-subrip"),
        serve!("subs/sample_es.srt", 2, "application/x-subrip"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1000),
        send!(Send::AddSubtitleSourceV4 {
            file_id: 1,
            select: true,
            name: Some("English"),
        }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 1,
        },
        // Let the first add's restore sequence finish: its selection must
        // land and hold before the second add piles on.
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        send!(Send::AddSubtitleSourceV4 {
            file_id: 2,
            select: true,
            name: Some("Spanish"),
        }),
        // Both externals must now be advertised (English + Spanish).
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 2,
        },
        // Spanish (the last selected, advertised at index 1) is active.
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(1),
        },
        Step::AssertTrackCounts {
            video: 1,
            audio: 1,
            subtitle: 2,
        },
        send!(Send::StopV4),
    ]
);

// Attach two externals (one selected, one not), then switch the selection
// between them and off with ChangeTrack. Both stay advertised throughout;
// only one is loaded as the suburi at a time.
define_test_case!(
    external_sub_switch_between_externals_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        serve!("subs/sample_en.srt", 1, "application/x-subrip"),
        serve!("subs/sample_es.srt", 2, "application/x-subrip"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1000),
        // English selected, Spanish attached-but-unselected (seamless, no
        // reload for the second).
        send!(Send::AddSubtitleSourceV4 {
            file_id: 1,
            select: true,
            name: Some("English"),
        }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 1,
        },
        // The first add's dance must fully land before the second add.
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        send!(Send::AddSubtitleSourceV4 {
            file_id: 2,
            select: false,
            name: Some("Spanish"),
        }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 2,
        },
        // English (index 0) stays the active selection (the unselected add
        // is seamless).
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        // Switch to Spanish (index 1): a suburi-swapping reload.
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: Some(1),
        }),
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(1),
        },
        Step::AssertTrackCounts {
            video: 1,
            audio: 1,
            subtitle: 2,
        },
        // Switch back to English (index 0): another reload.
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: Some(0),
        }),
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        // Playback survived the switching.
        recv!(Receive::ProgressV4AtLeast(4.0)),
        send!(Send::StopV4),
    ]
);

// An empty subtitle URL has no meaning in the protocol; the receiver must
// reject it as malformed without disturbing playback.
define_test_case!(
    external_sub_empty_url_malformed_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(800),
        send!(Send::AddSubtitleSourceEmptyUrlV4),
        recv!(Receive::Error(ErrorKind::MalformedBody)),
        // Playback is unaffected by the rejected request.
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        recv!(Receive::ProgressV4AtLeast(2.0)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    external_sub_keeps_embedded_selection_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_with_subs.mkv", 0, "video/x-matroska"),
        serve!("subs/sample_en.srt", 1, "application/x-subrip"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 3,
        },
        // Let the default selection establish (first embedded subtitle on).
        Step::SleepMillis(1200),
        // "Add but don't show" on media WITH embedded subtitles: the reload
        // must restore the embedded track that was showing, not turn all
        // subtitles off.
        send!(Send::AddSubtitleSourceV4 {
            file_id: 1,
            select: false,
            name: Some("External"),
        }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 4,
        },
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        // Playback must keep advancing after the reload.
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        recv!(Receive::ProgressV4AtLeast(2.0)),
        send!(Send::StopV4),
    ]
);

// The switch AWAY from the external track goes through a receiver-side
// reload (a plain SELECT_STREAMS would stall until the suburi stream's
// queued sparse data drained in real time), so that step includes a
// buffering blip before its ChangeTrack confirmation.
define_test_case!(
    external_sub_switch_embedded_external_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/video_with_subs.mkv", 0, "video/x-matroska"),
        serve!("subs/sample_en.srt", 1, "application/x-subrip"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 3,
        },
        Step::SleepMillis(1200),
        send!(Send::AddSubtitleSourceV4 {
            file_id: 1,
            select: true,
            name: Some("External"),
        }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 4,
        },
        // The external track (advertised last, after the embedded ones) must
        // be selected.
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(3),
        },
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        // ChangeTrack must move freely between external and embedded tracks:
        // to an embedded one (reload: deactivating the external track),
        // off (plain deselect of the embedded track), and back to the
        // external one (reload again: activating a parked suburi stream via
        // plain SELECT_STREAMS mid-playback can wedge the re-preroll).
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: Some(1),
        }),
        // The switch away is a full reload + restore; wait for it to land.
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(1),
        },
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: None,
        }),
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: None,
        },
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: Some(3),
        }),
        // The switch back onto the external track reloads as well.
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(3),
        },
        // No pipeline error or stall from the switching.
        recv!(Receive::ProgressV4AtLeast(4.0)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    external_sub_seek_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        serve!("subs/sample_en.srt", 1, "application/x-subrip"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1000),
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        recv!(Receive::ProgressV4AtLeast(2.0)),
        send!(Send::AddSubtitleSourceV4 {
            file_id: 1,
            select: true,
            name: Some("English"),
        }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 1,
        },
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        // Ordinary flushing seeks with the external text branch ACTIVE must
        // neither error the suburi source nor stall playback.
        send!(Send::SeekV4(30.0)),
        recv!(Receive::ProgressV4AtLeast(30.0)),
        // And the same while paused (seek + resume).
        send!(Send::PauseV4),
        Step::SleepMillis(300),
        send!(Send::SeekV4(60.0)),
        Step::SleepMillis(500),
        send!(Send::ResumeV4),
        recv!(Receive::ProgressV4AtLeast(60.0)),
        // The external track must still be selected after all the seeking.
        Step::AssertTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        send!(Send::StopV4),
    ]
);

define_test_case!(
    external_sub_add_invalid_url_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1000),
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        recv!(Receive::ProgressV4AtLeast(2.0)),
        // A 404 subtitle URL: the sub source fails without a bus error, so
        // the receiver must detect the missing text stream via its timeout
        // and report ResourceNotFound — while the main item keeps playing.
        send!(Send::AddSubtitleSourceFakeUrlV4 { select: true }),
        recv!(Receive::Error(ErrorKind::ResourceNotFound)),
        recv!(Receive::ProgressV4AtLeast(3.0)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    external_sub_no_media_rejected_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("subs/sample_en.srt", 0, "application/x-subrip"),
        // No media is loaded; the receiver must reject the request.
        send!(Send::AddSubtitleSourceV4 {
            file_id: 0,
            select: true,
            name: None,
        }),
        recv!(Receive::Error(ErrorKind::InvalidState)),
    ]
);

define_test_case!(
    cast_seek_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::SeekV4(42.5)),
        Step::SleepMillis(250),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    seek_while_paused_sends_progress_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1000),
        send!(Send::PauseV4),
        Step::SleepMillis(500),
        send!(Send::SeekV4(30.0)),
        recv!(Receive::ProgressV4AtLeast(20.0)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_companion_photo_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::CompanionHello),
        send!(Send::ServeCompanionFile {
            resource_id: 0,
            path: "image/flowers.jpg",
            mime: "image/jpeg",
        }),
        send!(Send::PlayCompanion { resource_id: 0 }),
        Step::SleepMillis(750),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_companion_video_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::CompanionHello),
        send!(Send::ServeCompanionFile {
            resource_id: 0,
            path: "video/BigBuckBunny.mp4",
            mime: "video/mp4",
        }),
        send!(Send::PlayCompanion { resource_id: 0 }),
        Step::SleepMillis(1000),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_companion_video_and_seek_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::CompanionHello),
        send!(Send::ServeCompanionFile {
            resource_id: 0,
            path: "video/BigBuckBunny.mp4",
            mime: "video/mp4",
        }),
        send!(Send::PlayCompanion { resource_id: 0 }),
        Step::SleepMillis(1000),
        send!(Send::SeekV4(60.0)),
        Step::SleepMillis(1000),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_companion_audio_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::CompanionHello),
        send!(Send::ServeCompanionFile {
            resource_id: 0,
            path: "audio/Court_House_Blues_Take_1.mp3",
            mime: "audio/mp3",
        }),
        send!(Send::PlayCompanion { resource_id: 0 }),
        Step::SleepMillis(1000),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_companion_missing_image_resource_not_found_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::CompanionHello),
        send!(Send::PlayCompanionMissing {
            resource_id: 0,
            container: "image/jpeg",
        }),
        recv!(Receive::Error(ErrorKind::ResourceNotFound)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_companion_missing_video_resource_not_found_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::CompanionHello),
        send!(Send::PlayCompanionMissing {
            resource_id: 0,
            container: "video/mp4",
        }),
        recv!(Receive::Error(ErrorKind::ResourceNotFound)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    invalid_opcode_error_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::RawOpcode(0x7F)),
        recv!(Receive::Error(ErrorKind::InvalidOpcode)),
    ]
);

define_test_case!(
    unsupported_opcode_error_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::RawOpcode(fcast_protocol::Opcode::Pause as u8)),
        recv!(Receive::Error(ErrorKind::InvalidOpcode)),
    ]
);

define_test_case!(
    cast_video_progress_interval_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1000),
        Step::MeasureProgressInterval {
            expected_ms: 500,
            tolerance_ms: 150,
            samples: 5,
        },
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        Step::MeasureProgressInterval {
            expected_ms: 200,
            tolerance_ms: 150,
            samples: 6,
        },
        send!(Send::SetProgressIntervalV4 { millis: 1000 }),
        Step::MeasureProgressInterval {
            expected_ms: 1000,
            tolerance_ms: 300,
            samples: 3,
        },
        send!(Send::StopV4),
    ]
);

define_test_case!(
    cast_companion_empty_seek,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::CompanionHello),
        send!(Send::ServeCompanionFile {
            resource_id: 0,
            path: "audio/Court_House_Blues_Take_1.mp3",
            mime: "audio/mp3",
        }),
        send!(Send::PlayCompanion { resource_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::EmptySeekV4),
        recv!(Receive::Error(ErrorKind::MalformedBody)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    version_zero_closes,
    &[
        recv!(Receive::Version),
        send!(Send::Version(0)),
        Step::ExpectClosed,
    ]
);

define_test_case!(
    version_downgrade_v5_to_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(5)),
        // Versions higher than the receiver implements are clamped to v4, which
        // upgrades the connection to TLS and exchanges introductions.
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
    ]
);

define_test_case!(
    flatbuf_before_handshake_closes,
    &[
        recv!(Receive::Version),
        // A v4 flatbuffer opcode before negotiating a version is illegal.
        send!(Send::RawOpcode(fcast_protocol::Opcode::Flatbuf as u8)),
        Step::ExpectClosed,
    ]
);

define_test_case!(
    ping_before_version_closes,
    &[
        recv!(Receive::Version),
        send!(Send::RawOpcode(fcast_protocol::Opcode::Ping as u8)),
        Step::ExpectClosed,
    ]
);

define_test_case!(
    garbage_flatbuf_closes_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::RawMessage {
            opcode: fcast_protocol::Opcode::Flatbuf as u8,
            body: &[0xde, 0xad, 0xbe, 0xef, 0x01, 0x02, 0x03, 0x04],
        }),
        Step::ExpectClosed,
    ]
);

define_test_case!(
    truncated_flatbuf_closes_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::RawMessage {
            opcode: fcast_protocol::Opcode::Flatbuf as u8,
            body: &[0x01, 0x02],
        }),
        Step::ExpectClosed,
    ]
);

define_test_case!(
    wrong_direction_messages_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        // Messages only the receiver should ever send must be rejected when a
        // sender sends them.
        send!(Send::ReceiverIntroductionV4),
        recv!(Receive::Error(ErrorKind::InvalidPayloadType)),
        send!(Send::ErrorV4(ErrorKind::InvalidState)),
        recv!(Receive::Error(ErrorKind::InvalidPayloadType)),
        send!(Send::CompanionHelloResponseV4),
        recv!(Receive::Error(ErrorKind::InvalidPayloadType)),
    ]
);

define_test_case!(
    none_opcode_error_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::RawOpcode(fcast_protocol::Opcode::None as u8)),
        recv!(Receive::Error(ErrorKind::InvalidOpcode)),
    ]
);

define_test_case!(
    empty_progress_interval_malformed_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        send!(Send::EmptyProgressIntervalV4),
        recv!(Receive::Error(ErrorKind::MalformedBody)),
    ]
);

define_test_case!(
    volume_clamped_high_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::SetVolumeV4(0.5)),
        send!(Send::SetVolumeV4Raw(1.5)),
        recv!(Receive::Error(ErrorKind::VolumeOutOfRange)),
        recv!(Receive::VolumeChangedV4(1.0)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    volume_clamped_low_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::SetVolumeV4(0.5)),
        send!(Send::SetVolumeV4Raw(-0.5)),
        recv!(Receive::Error(ErrorKind::VolumeOutOfRange)),
        recv!(Receive::VolumeChangedV4(0.0)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    set_speed_extremes_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::SetSpeedV4(0.25)),
        Step::SleepMillis(500),
        send!(Send::SetSpeedV4(4.0)),
        Step::SleepMillis(500),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    seek_far_beyond_duration_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::SeekV4(99999.0)),
        recv!(Receive::Error(ErrorKind::SeekOutOfRange)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    progress_interval_min_clamp_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1000),
        send!(Send::SetProgressIntervalV4 { millis: 0 }),
        Step::MeasureProgressInterval {
            expected_ms: 150,
            tolerance_ms: 130,
            samples: 6,
        },
        send!(Send::StopV4),
    ]
);

define_test_case!(
    queue_load_no_start_index_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        serve!("image/garden.jpg", 1, "image/jpeg"),
        send!(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }, PlaylistItem { file_id: 1 }],
            start_index: None,
        }),
        Step::SleepMillis(750),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    queue_full_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        send!(Send::LoadQueueRepeatV4 {
            file_id: 0,
            count: 256,
            start_index: Some(0),
        }),
        Step::SleepMillis(500),
        send!(Send::QueueInsertV4 {
            file_id: 0,
            position: QueuePosition::Back,
        }),
        recv!(Receive::Error(ErrorKind::QueueFull)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    queue_insert_front_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        serve!("image/garden.jpg", 1, "image/jpeg"),
        send!(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }],
            start_index: Some(0),
        }),
        Step::SleepMillis(500),
        send!(Send::QueueInsertV4 {
            file_id: 1,
            position: QueuePosition::Front,
        }),
        Step::SleepMillis(500),
        send!(Send::QueueSelectV4 {
            position: QueuePosition::Front,
        }),
        Step::SleepMillis(500),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    multi_sender_load_broadcast_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        Step::OpenSecondSender,
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::ExpectLoadOnSecondSender,
        send!(Send::StopV4),
    ]
);

define_test_case!(
    multi_sender_volume_broadcast_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        Step::OpenSecondSender,
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::SetVolumeV4(0.5)),
        Step::ExpectVolumeOnSecondSender(0.5),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    multi_sender_progress_isolation_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        Step::OpenSecondSender,
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::SleepMillis(1000),
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        Step::SetSecondSenderInterval { millis: 600 },
        Step::MeasureProgressBothSenders {
            a_expected_ms: 200,
            b_expected_ms: 600,
            tolerance_ms: 160,
            samples: 6,
        },
        send!(Send::StopV4),
    ]
);

define_test_case!(
    multi_sender_queue_insert_broadcast_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        Step::OpenSecondSender,
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        serve!("image/garden.jpg", 1, "image/jpeg"),
        send!(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }],
            start_index: Some(0),
        }),
        Step::SleepMillis(500),
        send!(Send::QueueInsertV4 {
            file_id: 1,
            position: QueuePosition::Back,
        }),
        Step::ExpectQueueMutationOnSecondSender(QueueMutationKind::Insert),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    multi_sender_queue_remove_broadcast_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        Step::OpenSecondSender,
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        serve!("image/garden.jpg", 1, "image/jpeg"),
        send!(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }, PlaylistItem { file_id: 1 }],
            start_index: Some(0),
        }),
        Step::SleepMillis(500),
        // Remove the back (non-playing) item; the front item is playing.
        send!(Send::QueueRemoveV4 {
            position: QueuePosition::Back,
        }),
        Step::ExpectQueueMutationOnSecondSender(QueueMutationKind::Remove),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    multi_sender_queue_select_broadcast_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        Step::OpenSecondSender,
        serve!("image/flowers.jpg", 0, "image/jpeg"),
        serve!("image/garden.jpg", 1, "image/jpeg"),
        send!(Send::LoadQueueV4 {
            items: &[PlaylistItem { file_id: 0 }, PlaylistItem { file_id: 1 }],
            start_index: Some(0),
        }),
        Step::SleepMillis(500),
        send!(Send::QueueSelectV4 {
            position: QueuePosition::Back,
        }),
        Step::ExpectQueueMutationOnSecondSender(QueueMutationKind::Select),
        send!(Send::StopV4),
    ]
);

// Two senders share one media session end to end: sender 1 casts and attaches
// an external subtitle, whose advertisement must be broadcast to sender 2;
// sender 2 attaches its own external; then each sender selects the track the
// OTHER peer added (suburi-swapping reloads in both directions, one of them
// driven by the second connection), with both senders observing every change.
define_test_case!(
    multi_sender_external_subs_v4,
    &[
        recv!(Receive::Version),
        send!(Send::Version(4)),
        send!(Send::SenderIntroduction),
        recv!(Receive::ReceiverIntroduction),
        Step::OpenSecondSender,
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        serve!("subs/sample_en.srt", 1, "application/x-subrip"),
        serve!("subs/sample_es.srt", 2, "application/x-subrip"),
        send!(Send::PlayV4 { file_id: 0 }),
        Step::ExpectLoadOnSecondSender,
        Step::SleepMillis(1000),
        // Sender 1 attaches and selects an external subtitle.
        send!(Send::AddSubtitleSourceV4 {
            file_id: 1,
            select: true,
            name: Some("English"),
        }),
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 1,
        },
        // The new track must be advertised to sender 2 as well.
        Step::AwaitTracksOnSecondSender {
            video: 1,
            audio: 1,
            subtitle: 1,
        },
        // Wait out the add's reload + restore dance: settled with the
        // external (index 0) selected.
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        // Sender 2 attaches its own external (unselected: a seamless add).
        Step::AddSubtitleSourceOnSecondSenderV4 {
            file_id: 2,
            select: false,
            name: Some("Spanish"),
        },
        // Both senders see both externals advertised.
        Step::AwaitTracks {
            video: 1,
            audio: 1,
            subtitle: 2,
        },
        Step::AwaitTracksOnSecondSender {
            video: 1,
            audio: 1,
            subtitle: 2,
        },
        Step::AssertTrackCounts {
            video: 1,
            audio: 1,
            subtitle: 2,
        },
        // Sender 2's add must not have disturbed sender 1's selection.
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        send!(Send::SetProgressIntervalV4 { millis: 200 }),
        // Sender 1 switches onto the peer's track (a suburi-swapping reload).
        send!(Send::ChangeTrack {
            kind: TrackKind::Subtitle,
            index: Some(1),
        }),
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(1),
        },
        // ...and sender 2 switches back onto the track sender 1 added
        // (another reload, this time requested over the second connection).
        Step::ChangeTrackOnSecondSender {
            kind: TrackKind::Subtitle,
            index: Some(0),
        },
        // Both senders observe the selection sender 2 made.
        Step::AwaitTrackState {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        Step::AssertTrackStateOnSecondSender {
            video: Some(0),
            audio: Some(0),
            subtitle: Some(0),
        },
        // All the cross-sender switching must not have wedged playback.
        recv!(Receive::ProgressV4AtLeast(4.0)),
        send!(Send::StopV4),
    ]
);

define_test_case!(
    seek_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV3 { file_id: 0 }),
        Step::SleepMillis(750),
        send!(Send::Seek(30.0)),
        Step::SleepMillis(500),
        send!(Send::Stop),
    ]
);

define_test_case!(
    unsubscribe_event_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(3)),
        send!(Send::Initial),
        recv!(Receive::Initial),
        send!(Send::SubscribeEvent(
            v3::EventSubscribeObject::MediaItemStart
        )),
        send!(Send::UnsubscribeEvent(
            v3::EventSubscribeObject::MediaItemStart
        )),
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4"),
        send!(Send::PlayV3 { file_id: 0 }),
        Step::SleepMillis(750),
        send!(Send::Stop),
    ]
);
