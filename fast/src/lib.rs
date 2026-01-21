#[derive(Debug)]
pub enum Send {
    Version(u64),
    Initial,
    Ping,
    SetVolume(f64),
    Stop,
    PlayV2 { file_id: u32 },
    PlayV3 { file_id: u32 },
    Pause,
    Resume,
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
    cast_video_v2,
    cast_video_set_volume_v2,
    cast_video_v3,
    cast_video_set_volume_v3,
    cast_pause_resume_v2
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
        Step::ServeFile {
            path: "image/flowers.jpg",
            id: 0,
            mime: "image/jpeg"
        },
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
        Step::ServeFile {
            path: "image/flowers.jpg",
            id: 0,
            mime: "image/jpeg"
        },
        Step::ServeFile {
            path: "image/garden.jpg",
            id: 1,
            mime: "image/jpeg"
        },
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
        Step::ServeFile {
            path: "image/flowers.jpg",
            id: 0,
            mime: "image/jpeg"
        },
        send!(Send::PlayV3 { file_id: 0 }),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_video_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        Step::ServeFile {
            path: "video/BigBuckBunny.mp4",
            id: 0,
            mime: "video/mp4"
        },
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(2000),
        send!(Send::Stop),
    ]
);

define_test_case!(
    cast_video_set_volume_v2,
    &[
        recv!(Receive::Version),
        send!(Send::Version(2)),
        Step::ServeFile {
            path: "video/BigBuckBunny.mp4",
            id: 0,
            mime: "video/mp4"
        },
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(2000),
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
        Step::ServeFile {
            path: "video/BigBuckBunny.mp4",
            id: 0,
            mime: "video/mp4"
        },
        send!(Send::PlayV3 { file_id: 0 }),
        Step::SleepMillis(2000),
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
        Step::ServeFile {
            path: "video/BigBuckBunny.mp4",
            id: 0,
            mime: "video/mp4"
        },
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
        Step::ServeFile {
            path: "video/BigBuckBunny.mp4",
            id: 0,
            mime: "video/mp4"
        },
        send!(Send::PlayV2 { file_id: 0 }),
        Step::SleepMillis(500),
        send!(Send::Pause),
        send!(Send::Resume),
        send!(Send::Stop),
    ]
);
