use fcast_protocol::v3;

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
    Stop,
    PlayV2 { file_id: u32 },
    PlayV3 { file_id: u32 },
    PlayV3WithBody {
        file_id: u32,
        time: Option<f64>,
        volume: Option<f64>,
        speed: Option<f64>,
    },
    Pause,
    Resume,
    SubscribeEvent(v3::EventSubscribeObject),
    PlaylistV3 { items: &'static [PlaylistItem] },
    SetPlaylistItem { index: u64 },
    // TODO: test that HTTP request headers are correctly handled by the receiver
}

#[derive(Debug)]
pub enum Receive {
    Version,
    Initial,
    Pong,
    Volume,
    PlaybackUpdate,
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
    cast_video_set_volume_v2,
    cast_video_v3,
    cast_video_set_volume_v3,
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
    cast_simple_playlist_with_headers
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
        serve!("video/BigBuckBunny.mp4", 0, "video/mp4", [("User-Agent", "Fake"), ("Custom-Key", "ABC")]),
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
        serve!("audio/Court_House_Blues_Take_1.mp3", 0, "audio/mp4"),
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
        send!(Send::SubscribeEvent(
            v3::EventSubscribeObject::MediaItemEnd
        )),
        Step::SleepMillis(500),
        serve!("audio/Dont_Go_Way_Nobody.mp3", 0, "audio/mp4"),
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
            items: &[
                PlaylistItem { file_id: 0 },
                PlaylistItem { file_id: 1 },
            ]
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
        serve!("image/flowers.jpg", 0, "image/jpeg", [("User-Agent", "Fake"), ("Custom-Key", "ABC")]),
        send!(Send::PlaylistV3 {
            items: &[
                PlaylistItem { file_id: 0 },
            ]
        }),
        send!(Send::Stop),
    ]
);
