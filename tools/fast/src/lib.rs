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
    OpenSecondSender,
    SetSecondSenderInterval {
        millis: u64,
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
    version_downgrade_v5_to_v3,
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
    version_downgrade_v5_to_v3,
    &[
        recv!(Receive::Version),
        send!(Send::Version(5)),
        // Versions higher than the receiver implements should be downgraded to
        // v3, which proactively sends an Initial message.
        recv!(Receive::Initial),
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
        // An out-of-range volume should be clamped to 1.0, not rejected/echoed verbatim.
        send!(Send::SetVolumeV4Raw(1.5)),
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
        Step::SleepMillis(1000),
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
