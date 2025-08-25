#[macro_use]
extern crate std;
#[prelude_import]
use std::prelude::rust_2021::*;
pub mod api {
    use std::collections::HashMap;
    use std::sync::Arc;
    pub use fcast_sender_sdk::IpAddr;
    use fcast_sender_sdk::context;
    pub use fcast_sender_sdk::device::{
        self, ApplicationInfo, CastingDeviceError, DeviceConnectionState,
        DeviceFeature, DeviceInfo, GenericEventSubscriptionGroup,
        GenericKeyEvent, GenericMediaEvent, LoadRequest, Metadata,
        PlaybackState, PlaylistItem, ProtocolType, Source,
    };
    use flutter_rust_bridge::frb;
    #[frb(mirror(IpAddr))]
    pub enum _IpAddr {
        V4 {
            o1: u8,
            o2: u8,
            o3: u8,
            o4: u8,
        },
        V6 {
            o1: u8,
            o2: u8,
            o3: u8,
            o4: u8,
            o5: u8,
            o6: u8,
            o7: u8,
            o8: u8,
            o9: u8,
            o10: u8,
            o11: u8,
            o12: u8,
            o13: u8,
            o14: u8,
            o15: u8,
            o16: u8,
            scope_id: u32,
        },
    }
    #[frb(mirror(ProtocolType))]
    pub enum _ProtocolType { Chromecast, FCast, }
    #[frb(mirror(DeviceConnectionState))]
    pub enum _DeviceConnectionState {
        Disconnected,
        Connecting,
        Connected {
            used_remote_addr: _IpAddr,
            local_addr: _IpAddr,
        },
    }
    #[frb(mirror(DeviceInfo))]
    pub struct _DeviceInfo {
        pub name: String,
        pub protocol: ProtocolType,
        pub addresses: Vec<IpAddr>,
        pub port: u16,
    }
    pub fn device_info_from_url(url: String) -> Option<DeviceInfo> {
        device::device_info_from_url(url)
    }
    #[frb(mirror(PlaybackState))]
    pub enum _PlaybackState {

        #[default]
        Idle,
        Buffering,
        Playing,
        Paused,
    }
    #[automatically_derived]
    impl ::core::default::Default for _PlaybackState {
        #[inline]
        fn default() -> _PlaybackState { Self::Idle }
    }
    #[frb(mirror(Source))]
    pub enum _Source {
        Url {
            url: String,
            #[doc = " MIME content type"]
            content_type: String,
        },
        Content {
            content: String,
        },
    }
    #[frb(mirror(PlaylistItem))]
    pub struct _PlaylistItem {
        #[doc = " MIME type"]
        pub content_type: String,
        #[doc = " URL"]
        pub content_location: String,
        #[doc = " Seconds from beginning of media to start playback"]
        pub start_time: Option<f64>,
    }
    #[frb(mirror(GenericEventSubscriptionGroup))]
    pub enum _GenericEventSubscriptionGroup { Keys, Media, }
    #[frb(mirror(GenericKeyEvent))]
    pub struct _GenericKeyEvent {
        pub released: bool,
        pub repeat: bool,
        pub handled: bool,
        pub name: String,
    }
    #[frb(mirror(GenericMediaEvent))]
    pub enum _GenericMediaEvent { Started, Ended, Changed, }
    pub trait DeviceEventHandler: Send + Sync {
        fn connection_state_changed(&self, state: _DeviceConnectionState);
        fn volume_changed(&self, volume: f64);
        fn time_changed(&self, time: f64);
        fn playback_state_changed(&self, state: _PlaybackState);
        fn duration_changed(&self, duration: f64);
        fn speed_changed(&self, speed: f64);
        fn source_changed(&self, source: _Source);
        fn key_event(&self, event: _GenericKeyEvent);
        fn media_event(&self, event: _GenericMediaEvent);
        fn playback_error(&self, message: String);
    }
    #[frb(ignore)]
    struct DeviceEventHandlerWrapper(Arc<dyn DeviceEventHandler>);
    impl device::DeviceEventHandler for DeviceEventHandlerWrapper {
        fn connection_state_changed(&self, state: DeviceConnectionState) {
            #[rustfmt::skip]
            macro_rules! ip_addr_wrapper_to_orig {
                ($addr:expr) =>
                {
                    match $addr
                    {
                        IpAddr::V4 { o1, o2, o3, o4 } => _IpAddr::V4
                        { o1, o2, o3, o4 }, IpAddr::V6
                        {
                            o1, o2, o3, o4, o5, o6, o7, o8, o9, o10, o11, o12, o13, o14,
                            o15, o16, scope_id,
                        } => _IpAddr::V6
                        {
                            o1, o2, o3, o4, o5, o6, o7, o8, o9, o10, o11, o12, o13, o14,
                            o15, o16, scope_id,
                        },
                    }
                };
            }
            self.0.connection_state_changed(match state {
                    DeviceConnectionState::Disconnected =>
                        _DeviceConnectionState::Disconnected,
                    DeviceConnectionState::Connecting =>
                        _DeviceConnectionState::Connecting,
                    DeviceConnectionState::Connected {
                        used_remote_addr, local_addr } =>
                        _DeviceConnectionState::Connected {
                            used_remote_addr: match used_remote_addr {
                                IpAddr::V4 { o1, o2, o3, o4 } =>
                                    _IpAddr::V4 { o1, o2, o3, o4 },
                                IpAddr::V6 {
                                    o1,
                                    o2,
                                    o3,
                                    o4,
                                    o5,
                                    o6,
                                    o7,
                                    o8,
                                    o9,
                                    o10,
                                    o11,
                                    o12,
                                    o13,
                                    o14,
                                    o15,
                                    o16,
                                    scope_id } =>
                                    _IpAddr::V6 {
                                        o1,
                                        o2,
                                        o3,
                                        o4,
                                        o5,
                                        o6,
                                        o7,
                                        o8,
                                        o9,
                                        o10,
                                        o11,
                                        o12,
                                        o13,
                                        o14,
                                        o15,
                                        o16,
                                        scope_id,
                                    },
                            },
                            local_addr: match local_addr {
                                IpAddr::V4 { o1, o2, o3, o4 } =>
                                    _IpAddr::V4 { o1, o2, o3, o4 },
                                IpAddr::V6 {
                                    o1,
                                    o2,
                                    o3,
                                    o4,
                                    o5,
                                    o6,
                                    o7,
                                    o8,
                                    o9,
                                    o10,
                                    o11,
                                    o12,
                                    o13,
                                    o14,
                                    o15,
                                    o16,
                                    scope_id } =>
                                    _IpAddr::V6 {
                                        o1,
                                        o2,
                                        o3,
                                        o4,
                                        o5,
                                        o6,
                                        o7,
                                        o8,
                                        o9,
                                        o10,
                                        o11,
                                        o12,
                                        o13,
                                        o14,
                                        o15,
                                        o16,
                                        scope_id,
                                    },
                            },
                        },
                });
        }
        fn volume_changed(&self, volume: f64) {
            self.0.volume_changed(volume);
        }
        fn time_changed(&self, time: f64) { self.0.time_changed(time); }
        fn playback_state_changed(&self, state: PlaybackState) {
            self.0.playback_state_changed(match state {
                    PlaybackState::Idle => _PlaybackState::Idle,
                    PlaybackState::Buffering => _PlaybackState::Buffering,
                    PlaybackState::Playing => _PlaybackState::Playing,
                    PlaybackState::Paused => _PlaybackState::Paused,
                });
        }
        fn duration_changed(&self, duration: f64) {
            self.0.duration_changed(duration);
        }
        fn speed_changed(&self, speed: f64) { self.0.speed_changed(speed); }
        fn source_changed(&self, source: Source) {
            self.0.source_changed(match source {
                    Source::Url { url, content_type } =>
                        _Source::Url { url, content_type },
                    Source::Content { content } => _Source::Content { content },
                });
        }
        fn key_event(&self, event: GenericKeyEvent) {
            self.0.key_event(_GenericKeyEvent {
                    released: event.released,
                    repeat: event.repeat,
                    handled: event.handled,
                    name: event.name,
                });
        }
        fn media_event(&self, event: GenericMediaEvent) {
            self.0.media_event(match event {
                    GenericMediaEvent::Started => _GenericMediaEvent::Started,
                    GenericMediaEvent::Ended => _GenericMediaEvent::Ended,
                    GenericMediaEvent::Changed => _GenericMediaEvent::Changed,
                });
        }
        fn playback_error(&self, message: String) {
            self.0.playback_error(message);
        }
    }
    #[frb(mirror(CastingDeviceError))]
    pub enum _CastingDeviceError {

        #[error("failed to send command to worker thread")]
        FailedToSendCommand,

        #[error("missing addresses")]
        MissingAddresses,

        #[error("device already started")]
        DeviceAlreadyStarted,

        #[error("unsupported subscription")]
        UnsupportedSubscription,

        #[error("unsupported feature")]
        UnsupportedFeature,
    }
    #[allow(unused_qualifications)]
    #[automatically_derived]
    impl ::thiserror::__private::Error for _CastingDeviceError { }
    #[allow(unused_qualifications)]
    #[automatically_derived]
    impl ::core::fmt::Display for _CastingDeviceError {
        fn fmt(&self, __formatter: &mut ::core::fmt::Formatter)
            -> ::core::fmt::Result {

            #[allow(unused_variables, deprecated, clippy ::
            used_underscore_binding)]
            match self {
                _CastingDeviceError::FailedToSendCommand {} =>
                    __formatter.write_str("failed to send command to worker thread"),
                _CastingDeviceError::MissingAddresses {} =>
                    __formatter.write_str("missing addresses"),
                _CastingDeviceError::DeviceAlreadyStarted {} =>
                    __formatter.write_str("device already started"),
                _CastingDeviceError::UnsupportedSubscription {} =>
                    __formatter.write_str("unsupported subscription"),
                _CastingDeviceError::UnsupportedFeature {} =>
                    __formatter.write_str("unsupported feature"),
            }
        }
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for _CastingDeviceError {
        #[inline]
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::write_str(f,
                match self {
                    _CastingDeviceError::FailedToSendCommand =>
                        "FailedToSendCommand",
                    _CastingDeviceError::MissingAddresses => "MissingAddresses",
                    _CastingDeviceError::DeviceAlreadyStarted =>
                        "DeviceAlreadyStarted",
                    _CastingDeviceError::UnsupportedSubscription =>
                        "UnsupportedSubscription",
                    _CastingDeviceError::UnsupportedFeature =>
                        "UnsupportedFeature",
                })
        }
    }
    #[frb(mirror(DeviceFeature))]
    pub enum _DeviceFeature {
        SetVolume,
        SetSpeed,
        LoadContent,
        LoadUrl,
        KeyEventSubscription,
        MediaEventSubscription,
        LoadImage,
        LoadPlaylist,
        PlaylistNextAndPrevious,
        SetPlaylistItemIndex,
    }
    #[automatically_derived]
    impl ::core::clone::Clone for _DeviceFeature {
        #[inline]
        fn clone(&self) -> _DeviceFeature {
            match self {
                _DeviceFeature::SetVolume => _DeviceFeature::SetVolume,
                _DeviceFeature::SetSpeed => _DeviceFeature::SetSpeed,
                _DeviceFeature::LoadContent => _DeviceFeature::LoadContent,
                _DeviceFeature::LoadUrl => _DeviceFeature::LoadUrl,
                _DeviceFeature::KeyEventSubscription =>
                    _DeviceFeature::KeyEventSubscription,
                _DeviceFeature::MediaEventSubscription =>
                    _DeviceFeature::MediaEventSubscription,
                _DeviceFeature::LoadImage => _DeviceFeature::LoadImage,
                _DeviceFeature::LoadPlaylist => _DeviceFeature::LoadPlaylist,
                _DeviceFeature::PlaylistNextAndPrevious =>
                    _DeviceFeature::PlaylistNextAndPrevious,
                _DeviceFeature::SetPlaylistItemIndex =>
                    _DeviceFeature::SetPlaylistItemIndex,
            }
        }
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for _DeviceFeature {
        #[inline]
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::write_str(f,
                match self {
                    _DeviceFeature::SetVolume => "SetVolume",
                    _DeviceFeature::SetSpeed => "SetSpeed",
                    _DeviceFeature::LoadContent => "LoadContent",
                    _DeviceFeature::LoadUrl => "LoadUrl",
                    _DeviceFeature::KeyEventSubscription =>
                        "KeyEventSubscription",
                    _DeviceFeature::MediaEventSubscription =>
                        "MediaEventSubscription",
                    _DeviceFeature::LoadImage => "LoadImage",
                    _DeviceFeature::LoadPlaylist => "LoadPlaylist",
                    _DeviceFeature::PlaylistNextAndPrevious =>
                        "PlaylistNextAndPrevious",
                    _DeviceFeature::SetPlaylistItemIndex =>
                        "SetPlaylistItemIndex",
                })
        }
    }
    #[automatically_derived]
    impl ::core::marker::StructuralPartialEq for _DeviceFeature { }
    #[automatically_derived]
    impl ::core::cmp::PartialEq for _DeviceFeature {
        #[inline]
        fn eq(&self, other: &_DeviceFeature) -> bool {
            let __self_discr = ::core::intrinsics::discriminant_value(self);
            let __arg1_discr = ::core::intrinsics::discriminant_value(other);
            __self_discr == __arg1_discr
        }
    }
    #[automatically_derived]
    impl ::core::cmp::Eq for _DeviceFeature {
        #[inline]
        #[doc(hidden)]
        #[coverage(off)]
        fn assert_receiver_is_total_eq(&self) -> () {}
    }
    macro_rules! device_error_converter {
        ($result:expr) =>
        {
            match $result
            {
                Ok(r) => Ok(r), Err(err) =>
                Err(match err
                {
                    CastingDeviceError::FailedToSendCommand =>
                    _CastingDeviceError::FailedToSendCommand,
                    CastingDeviceError::MissingAddresses =>
                    _CastingDeviceError::MissingAddresses,
                    CastingDeviceError::DeviceAlreadyStarted =>
                    { _CastingDeviceError::DeviceAlreadyStarted }
                    CastingDeviceError::UnsupportedSubscription =>
                    { _CastingDeviceError::UnsupportedSubscription }
                    CastingDeviceError::UnsupportedFeature =>
                    _CastingDeviceError::UnsupportedFeature,
                }),
            }
        };
    }
    #[frb(mirror(Metadata))]
    pub struct _Metadata {
        pub title: Option<String>,
        pub thumbnail_url: Option<String>,
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for _Metadata {
        #[inline]
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::debug_struct_field2_finish(f, "_Metadata",
                "title", &self.title, "thumbnail_url", &&self.thumbnail_url)
        }
    }
    #[automatically_derived]
    impl ::core::clone::Clone for _Metadata {
        #[inline]
        fn clone(&self) -> _Metadata {
            _Metadata {
                title: ::core::clone::Clone::clone(&self.title),
                thumbnail_url: ::core::clone::Clone::clone(&self.thumbnail_url),
            }
        }
    }
    #[automatically_derived]
    impl ::core::marker::StructuralPartialEq for _Metadata { }
    #[automatically_derived]
    impl ::core::cmp::PartialEq for _Metadata {
        #[inline]
        fn eq(&self, other: &_Metadata) -> bool {
            self.title == other.title &&
                self.thumbnail_url == other.thumbnail_url
        }
    }
    #[frb(mirror(ApplicationInfo))]
    pub struct _ApplicationInfo {
        pub name: String,
        pub version: String,
        pub display_name: String,
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for _ApplicationInfo {
        #[inline]
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::debug_struct_field3_finish(f,
                "_ApplicationInfo", "name", &self.name, "version",
                &self.version, "display_name", &&self.display_name)
        }
    }
    #[frb(mirror(LoadRequest))]
    pub enum _LoadRequest {
        Url {
            content_type: String,
            url: String,
            resume_position: Option<f64>,
            speed: Option<f64>,
            volume: Option<f64>,
            metadata: Option<_Metadata>,
            request_headers: Option<HashMap<String, String>>,
        },
        Content {
            content_type: String,
            content: String,
            resume_position: f64,
            speed: Option<f64>,
            volume: Option<f64>,
            metadata: Option<_Metadata>,
            request_headers: Option<HashMap<String, String>>,
        },
        Video {
            content_type: String,
            url: String,
            resume_position: f64,
            speed: Option<f64>,
            volume: Option<f64>,
            metadata: Option<_Metadata>,
            request_headers: Option<HashMap<String, String>>,
        },
        Image {
            content_type: String,
            url: String,
            metadata: Option<_Metadata>,
            request_headers: Option<HashMap<String, String>>,
        },
        Playlist {
            items: Vec<PlaylistItem>,
        },
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for _LoadRequest {
        #[inline]
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            match self {
                _LoadRequest::Url {
                    content_type: __self_0,
                    url: __self_1,
                    resume_position: __self_2,
                    speed: __self_3,
                    volume: __self_4,
                    metadata: __self_5,
                    request_headers: __self_6 } => {
                    let names: &'static _ =
                        &["content_type", "url", "resume_position", "speed",
                                    "volume", "metadata", "request_headers"];
                    let values: &[&dyn ::core::fmt::Debug] =
                        &[__self_0, __self_1, __self_2, __self_3, __self_4,
                                    __self_5, &__self_6];
                    ::core::fmt::Formatter::debug_struct_fields_finish(f, "Url",
                        names, values)
                }
                _LoadRequest::Content {
                    content_type: __self_0,
                    content: __self_1,
                    resume_position: __self_2,
                    speed: __self_3,
                    volume: __self_4,
                    metadata: __self_5,
                    request_headers: __self_6 } => {
                    let names: &'static _ =
                        &["content_type", "content", "resume_position", "speed",
                                    "volume", "metadata", "request_headers"];
                    let values: &[&dyn ::core::fmt::Debug] =
                        &[__self_0, __self_1, __self_2, __self_3, __self_4,
                                    __self_5, &__self_6];
                    ::core::fmt::Formatter::debug_struct_fields_finish(f,
                        "Content", names, values)
                }
                _LoadRequest::Video {
                    content_type: __self_0,
                    url: __self_1,
                    resume_position: __self_2,
                    speed: __self_3,
                    volume: __self_4,
                    metadata: __self_5,
                    request_headers: __self_6 } => {
                    let names: &'static _ =
                        &["content_type", "url", "resume_position", "speed",
                                    "volume", "metadata", "request_headers"];
                    let values: &[&dyn ::core::fmt::Debug] =
                        &[__self_0, __self_1, __self_2, __self_3, __self_4,
                                    __self_5, &__self_6];
                    ::core::fmt::Formatter::debug_struct_fields_finish(f,
                        "Video", names, values)
                }
                _LoadRequest::Image {
                    content_type: __self_0,
                    url: __self_1,
                    metadata: __self_2,
                    request_headers: __self_3 } =>
                    ::core::fmt::Formatter::debug_struct_field4_finish(f,
                        "Image", "content_type", __self_0, "url", __self_1,
                        "metadata", __self_2, "request_headers", &__self_3),
                _LoadRequest::Playlist { items: __self_0 } =>
                    ::core::fmt::Formatter::debug_struct_field1_finish(f,
                        "Playlist", "items", &__self_0),
            }
        }
    }
    #[automatically_derived]
    impl ::core::clone::Clone for _LoadRequest {
        #[inline]
        fn clone(&self) -> _LoadRequest {
            match self {
                _LoadRequest::Url {
                    content_type: __self_0,
                    url: __self_1,
                    resume_position: __self_2,
                    speed: __self_3,
                    volume: __self_4,
                    metadata: __self_5,
                    request_headers: __self_6 } =>
                    _LoadRequest::Url {
                        content_type: ::core::clone::Clone::clone(__self_0),
                        url: ::core::clone::Clone::clone(__self_1),
                        resume_position: ::core::clone::Clone::clone(__self_2),
                        speed: ::core::clone::Clone::clone(__self_3),
                        volume: ::core::clone::Clone::clone(__self_4),
                        metadata: ::core::clone::Clone::clone(__self_5),
                        request_headers: ::core::clone::Clone::clone(__self_6),
                    },
                _LoadRequest::Content {
                    content_type: __self_0,
                    content: __self_1,
                    resume_position: __self_2,
                    speed: __self_3,
                    volume: __self_4,
                    metadata: __self_5,
                    request_headers: __self_6 } =>
                    _LoadRequest::Content {
                        content_type: ::core::clone::Clone::clone(__self_0),
                        content: ::core::clone::Clone::clone(__self_1),
                        resume_position: ::core::clone::Clone::clone(__self_2),
                        speed: ::core::clone::Clone::clone(__self_3),
                        volume: ::core::clone::Clone::clone(__self_4),
                        metadata: ::core::clone::Clone::clone(__self_5),
                        request_headers: ::core::clone::Clone::clone(__self_6),
                    },
                _LoadRequest::Video {
                    content_type: __self_0,
                    url: __self_1,
                    resume_position: __self_2,
                    speed: __self_3,
                    volume: __self_4,
                    metadata: __self_5,
                    request_headers: __self_6 } =>
                    _LoadRequest::Video {
                        content_type: ::core::clone::Clone::clone(__self_0),
                        url: ::core::clone::Clone::clone(__self_1),
                        resume_position: ::core::clone::Clone::clone(__self_2),
                        speed: ::core::clone::Clone::clone(__self_3),
                        volume: ::core::clone::Clone::clone(__self_4),
                        metadata: ::core::clone::Clone::clone(__self_5),
                        request_headers: ::core::clone::Clone::clone(__self_6),
                    },
                _LoadRequest::Image {
                    content_type: __self_0,
                    url: __self_1,
                    metadata: __self_2,
                    request_headers: __self_3 } =>
                    _LoadRequest::Image {
                        content_type: ::core::clone::Clone::clone(__self_0),
                        url: ::core::clone::Clone::clone(__self_1),
                        metadata: ::core::clone::Clone::clone(__self_2),
                        request_headers: ::core::clone::Clone::clone(__self_3),
                    },
                _LoadRequest::Playlist { items: __self_0 } =>
                    _LoadRequest::Playlist {
                        items: ::core::clone::Clone::clone(__self_0),
                    },
            }
        }
    }
    #[frb(opaque)]
    pub struct CastingDevice(Arc<dyn device::CastingDevice>);
    impl CastingDevice {
        fn casting_protocol(&self) -> ProtocolType {
            self.0.casting_protocol()
        }
        fn is_ready(&self) -> bool { self.0.is_ready() }
        fn supports_feature(&self, feature: DeviceFeature) -> bool {
            self.0.supports_feature(feature)
        }
        fn name(&self) -> String { self.0.name() }
        fn set_name(&self, name: String) { self.0.set_name(name); }
        fn seek(&self, time_seconds: f64) -> Result<(), _CastingDeviceError> {
            match self.0.seek(time_seconds) {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn stop_playback(&self) -> Result<(), _CastingDeviceError> {
            match self.0.stop_playback() {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn pause_playback(&self) -> Result<(), _CastingDeviceError> {
            match self.0.pause_playback() {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn resume_playback(&self) -> Result<(), _CastingDeviceError> {
            match self.0.resume_playback() {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn load(&self, request: LoadRequest)
            -> Result<(), _CastingDeviceError> {
            match self.0.load(request) {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn playlist_item_next(&self) -> Result<(), _CastingDeviceError> {
            match self.0.playlist_item_next() {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn playlist_item_previous(&self) -> Result<(), _CastingDeviceError> {
            match self.0.playlist_item_previous() {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        /// Set the item index for the currently playing playlist.
        ///
        /// # Arguments
        ///   * `index`: zero-based index into the playlist
        fn set_playlist_item_index(&self, index: u32)
            -> Result<(), _CastingDeviceError> {
            match self.0.set_playlist_item_index(index) {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn change_volume(&self, volume: f64)
            -> Result<(), _CastingDeviceError> {
            match self.0.change_volume(volume) {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn change_speed(&self, speed: f64)
            -> Result<(), _CastingDeviceError> {
            match self.0.change_speed(speed) {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn disconnect(&self) -> Result<(), _CastingDeviceError> {
            match self.0.disconnect() {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn connect(&self, app_info: Option<ApplicationInfo>,
            event_handler: Arc<dyn DeviceEventHandler>)
            -> Result<(), _CastingDeviceError> {
            match self.0.connect(app_info,
                    Arc::new(DeviceEventHandlerWrapper(event_handler))) {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn get_device_info(&self) -> DeviceInfo { self.0.get_device_info() }
        fn get_addresses(&self) -> Vec<IpAddr> { self.0.get_addresses() }
        fn set_addresses(&self, addrs: Vec<IpAddr>) {
            self.0.set_addresses(addrs);
        }
        fn get_port(&self) -> u16 { self.0.get_port() }
        fn set_port(&self, port: u16) { self.0.set_port(port); }
        fn subscribe_event(&self, group: GenericEventSubscriptionGroup)
            -> Result<(), _CastingDeviceError> {
            match self.0.subscribe_event(group) {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
        fn unsubscribe_event(&self, group: GenericEventSubscriptionGroup)
            -> Result<(), _CastingDeviceError> {
            match self.0.unsubscribe_event(group) {
                Ok(r) => Ok(r),
                Err(err) =>
                    Err(match err {
                            CastingDeviceError::FailedToSendCommand =>
                                _CastingDeviceError::FailedToSendCommand,
                            CastingDeviceError::MissingAddresses =>
                                _CastingDeviceError::MissingAddresses,
                            CastingDeviceError::DeviceAlreadyStarted => {
                                _CastingDeviceError::DeviceAlreadyStarted
                            }
                            CastingDeviceError::UnsupportedSubscription => {
                                _CastingDeviceError::UnsupportedSubscription
                            }
                            CastingDeviceError::UnsupportedFeature =>
                                _CastingDeviceError::UnsupportedFeature,
                        }),
            }
        }
    }
    pub enum ErrorMessage {

        #[error("{0}")]
        Error(String),
    }
    #[automatically_derived]
    impl ::core::fmt::Debug for ErrorMessage {
        #[inline]
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            match self {
                ErrorMessage::Error(__self_0) =>
                    ::core::fmt::Formatter::debug_tuple_field1_finish(f,
                        "Error", &__self_0),
            }
        }
    }
    #[allow(unused_qualifications)]
    #[automatically_derived]
    impl ::thiserror::__private::Error for ErrorMessage { }
    #[allow(unused_qualifications)]
    #[automatically_derived]
    impl ::core::fmt::Display for ErrorMessage {
        fn fmt(&self, __formatter: &mut ::core::fmt::Formatter)
            -> ::core::fmt::Result {
            use ::thiserror::__private::AsDisplay as _;

            #[allow(unused_variables, deprecated, clippy ::
            used_underscore_binding)]
            match self {
                ErrorMessage::Error(_0) =>
                    match (_0.as_display(),) {
                        (__display0,) =>
                            __formatter.write_fmt(format_args!("{0}", __display0)),
                    },
            }
        }
    }
    #[frb(opaque)]
    pub struct CastContext(context::CastContext);
    impl CastContext {
        #[frb(sync)]
        pub fn new() -> Result<Self, ErrorMessage> {
            Ok(Self(context::CastContext::new().map_err(|err|
                                ErrorMessage::Error(err.to_string()))?))
        }
        #[frb(sync)]
        pub fn create_device_from_info(&self, info: DeviceInfo)
            -> CastingDevice {
            CastingDevice(self.0.create_device_from_info(info))
        }
    }
}
mod frb_generated {
    #![allow(non_camel_case_types, unused, non_snake_case,
    clippy::needless_return, clippy::redundant_closure_call,
    clippy::redundant_closure, clippy::useless_conversion, clippy::unit_arg,
    clippy::unused_unit, clippy::double_parens, clippy::let_and_return,
    clippy::too_many_arguments, clippy::match_single_binding,
    clippy::clone_on_copy, clippy::let_unit_value, clippy::deref_addrof,
    clippy::explicit_auto_deref, clippy::borrow_deref_ref,
    clippy::needless_borrow)]
    use crate::api::*;
    use crate::*;
    use flutter_rust_bridge::for_generated::byteorder::{
        NativeEndian, ReadBytesExt, WriteBytesExt,
    };
    use flutter_rust_bridge::for_generated::{
        transform_result_dco, Lifetimeable, Lockable,
    };
    use flutter_rust_bridge::{Handler, IntoIntoDart};
    #[doc(hidden)]
    pub(crate) struct FrbWrapper<T>(T);
    impl<T: Clone> Clone for FrbWrapper<T> {
        fn clone(&self) -> Self { FrbWrapper(self.0.clone()) }
    }
    impl<T: PartialEq> PartialEq for FrbWrapper<T> {
        fn eq(&self, other: &Self) -> bool { self.0.eq(&other.0) }
    }
    impl<T: Eq> Eq for FrbWrapper<T> {}
    impl<T: std::hash::Hash> std::hash::Hash for FrbWrapper<T> {
        fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
            self.0.hash(state)
        }
    }
    impl<T> From<T> for FrbWrapper<T> {
        fn from(t: T) -> Self { FrbWrapper(t) }
    }
    use std::collections::HashMap;
    use std::marker::PhantomData;
    use std::sync::Arc;
    pub struct MoiArc<T: ?Sized + MoiArcValue> {
        object_id: Option<ObjectId>,
        value: Option<Arc<T>>,
        _phantom: PhantomData<T>,
    }
    #[automatically_derived]
    impl<T: ::core::fmt::Debug + ?Sized + MoiArcValue> ::core::fmt::Debug for
        MoiArc<T> {
        #[inline]
        fn fmt(&self, f: &mut ::core::fmt::Formatter) -> ::core::fmt::Result {
            ::core::fmt::Formatter::debug_struct_field3_finish(f, "MoiArc",
                "object_id", &self.object_id, "value", &self.value,
                "_phantom", &&self._phantom)
        }
    }
    impl<T: ?Sized + MoiArcValue> Drop for MoiArc<T> {
        fn drop(&mut self) {
            if let Some(object_id) = self.object_id {
                Self::decrement_strong_count(object_id);
            }
        }
    }
    impl<T: ?Sized + MoiArcValue> AsRef<T> for MoiArc<T> {
        fn as_ref(&self) -> &T { self.value.as_ref().unwrap().as_ref() }
    }
    impl<T: ?Sized + MoiArcValue>
        ::flutter_rust_bridge::for_generated::BaseArc<T> for MoiArc<T> {
        fn new(value: T) -> Self where T: Sized {
            let mut pool = T::get_pool().write().unwrap();
            let object_id = pool.id_generator.next_id();
            let value = Arc::new(value);
            let old_value =
                pool.map.insert(object_id,
                    MoiArcPoolValue { ref_count: 1, value: value.clone() });
            if !old_value.is_none() {
                ::core::panicking::panic("assertion failed: old_value.is_none()")
            };
            Self {
                object_id: Some(object_id),
                value: Some(value),
                _phantom: PhantomData,
            }
        }
        fn try_unwrap(mut self) -> Result<T, Self> where T: Sized {
            let pool = &mut T::get_pool().write().unwrap();
            if pool.map.get(&self.object_id.unwrap()).unwrap().ref_count == 1
                {
                Self::decrement_strong_count_raw(self.object_id.unwrap(),
                    pool);
                self.object_id.take().unwrap();
                Ok(Arc::into_inner(self.value.take().unwrap()).unwrap())
            } else { Err(self) }
        }
        fn into_inner(self) -> Option<T> where T: Sized {
            self.try_unwrap().ok()
        }
        fn into_raw(mut self) -> usize { self.object_id.take().unwrap() }
    }
    impl<T: ?Sized + MoiArcValue> Clone for MoiArc<T> {
        fn clone(&self) -> Self {
            Self::increment_strong_count(self.object_id.unwrap());
            Self {
                object_id: self.object_id,
                value: self.value.clone(),
                _phantom: PhantomData,
            }
        }
    }
    impl<T: ?Sized + MoiArcValue> MoiArc<T> {
        pub(crate) fn from_raw(raw: usize) -> Self where T: Sized {
            let map = &T::get_pool().read().unwrap().map;
            Self {
                object_id: Some(raw),
                value: Some(map.get(&raw).unwrap().value.clone()),
                _phantom: PhantomData,
            }
        }
        pub fn increment_strong_count(raw: usize) {
            let map = &mut T::get_pool().write().unwrap().map;
            map.get_mut(&raw).unwrap().ref_count += 1;
        }
        pub fn decrement_strong_count(raw: usize) {
            let mut pool = T::get_pool().write().unwrap();
            let object = Self::decrement_strong_count_raw(raw, &mut pool);
            drop(pool);
            drop(object);
        }
        fn decrement_strong_count_raw(raw: usize,
            pool: &mut MoiArcPoolInner<T>) -> Option<MoiArcPoolValue<T>> {
            let value = pool.map.get_mut(&raw).unwrap();
            value.ref_count -= 1;
            if value.ref_count == 0 { pool.map.remove(&raw) } else { None }
        }
    }
    pub trait MoiArcValue: 'static {
        fn get_pool()
        -> &'static MoiArcPool<Self>;
    }
    type ObjectId = usize;
    pub type MoiArcPool<T> = std::sync::RwLock<MoiArcPoolInner<T>>;
    pub struct MoiArcPoolInner<T: ?Sized> {
        map: HashMap<ObjectId, MoiArcPoolValue<T>>,
        id_generator: IdGenerator,
    }
    impl<T: ?Sized> Default for MoiArcPoolInner<T> {
        fn default() -> Self {
            Self { map: HashMap::new(), id_generator: Default::default() }
        }
    }
    struct IdGenerator {
        next_id: ObjectId,
    }
    impl Default for IdGenerator {
        fn default() -> Self { Self { next_id: Self::MIN_ID } }
    }
    impl IdGenerator {
        const MIN_ID: ObjectId = 1;
        const MAX_ID: ObjectId = 2147483600;
        fn next_id(&mut self) -> ObjectId {
            let ans = self.next_id;
            self.next_id =
                if self.next_id >= Self::MAX_ID {
                    Self::MIN_ID
                } else { self.next_id + 1 };
            ans
        }
    }
    impl<T: ?Sized> MoiArcPoolInner<T> {}
    struct MoiArcPoolValue<T: ?Sized> {
        ref_count: i32,
        value: Arc<T>,
    }
    use ::flutter_rust_bridge::for_generated::decode_rust_opaque_nom;
    fn decode_rust_opaque_moi<T: MoiArcValue + Send + Sync>(ptr: usize)
        -> RustOpaqueMoi<T> {
        RustOpaqueMoi::from_arc(MoiArc::<T>::from_raw(ptr))
    }
    use ::flutter_rust_bridge::for_generated::StdArc;
    use ::flutter_rust_bridge::RustOpaqueNom;
    /// Please refer to `RustOpaque` for doc.
    pub type RustOpaqueMoi<T> =
        ::flutter_rust_bridge::for_generated::RustOpaqueBase<T, MoiArc<T>>;
    /// A wrapper to support [arbitrary Rust types](https://cjycode.com/flutter_rust_bridge/guides/types/arbitrary).
    pub type RustOpaque<T> = RustOpaqueMoi<T>;
    use ::flutter_rust_bridge::RustAutoOpaqueNom;
    /// Please refer to `RustAutoOpaque` for doc.
    pub type RustAutoOpaqueMoi<T> =
        ::flutter_rust_bridge::for_generated::RustAutoOpaqueBase<T,
        MoiArc<::flutter_rust_bridge::for_generated::RustAutoOpaqueInner<T>>>;
    /// Usually this is unneeded, and just write down arbitrary types.
    /// However, when you need arbitrary types at places that are not supported yet,
    /// use `RustOpaqueOpaque<YourArbitraryType>`.
    pub type RustAutoOpaque<T> = RustAutoOpaqueMoi<T>;
    pub trait CstDecode<T> {
        fn cst_decode(self)
        -> T;
    }
    impl<T, S> CstDecode<Option<T>> for *mut S where *mut S: CstDecode<T> {
        fn cst_decode(self) -> Option<T> {
            (!self.is_null()).then(|| self.cst_decode())
        }
    }
    pub trait SseDecode {
        fn sse_decode(deserializer:
            &mut ::flutter_rust_bridge::for_generated::SseDeserializer)
        -> Self;
    }
    pub trait SseEncode {
        fn sse_encode(self,
        serializer: &mut ::flutter_rust_bridge::for_generated::SseSerializer);
    }
    fn transform_result_sse<T, E>(raw: Result<T, E>)
        ->
            Result<::flutter_rust_bridge::for_generated::Rust2DartMessageSse,
            ::flutter_rust_bridge::for_generated::Rust2DartMessageSse> where
        T: SseEncode, E: SseEncode {
        use ::flutter_rust_bridge::for_generated::{Rust2DartAction, SseCodec};
        match raw {
            Ok(raw) =>
                Ok(SseCodec::encode(Rust2DartAction::Success,
                        |serializer| { raw.sse_encode(serializer) })),
            Err(raw) =>
                Err(SseCodec::encode(Rust2DartAction::Error,
                        |serializer| { raw.sse_encode(serializer) })),
        }
    }
    pub struct StreamSink<T,
        Rust2DartCodec: ::flutter_rust_bridge::for_generated::BaseCodec =
        ::flutter_rust_bridge::for_generated::SseCodec> {
        base: ::flutter_rust_bridge::for_generated::StreamSinkBase<T,
        Rust2DartCodec>,
    }
    #[automatically_derived]
    impl<T: ::core::clone::Clone, Rust2DartCodec: ::core::clone::Clone +
        ::flutter_rust_bridge::for_generated::BaseCodec> ::core::clone::Clone
        for StreamSink<T, Rust2DartCodec> {
        #[inline]
        fn clone(&self) -> StreamSink<T, Rust2DartCodec> {
            StreamSink { base: ::core::clone::Clone::clone(&self.base) }
        }
    }
    impl<T, Rust2DartCodec: ::flutter_rust_bridge::for_generated::BaseCodec>
        StreamSink<T, Rust2DartCodec> {
        pub fn deserialize(raw: String) -> Self {
            Self {
                base: ::flutter_rust_bridge::for_generated::StreamSinkBase::deserialize(raw),
            }
        }
    }
    impl<T> StreamSink<T, ::flutter_rust_bridge::for_generated::DcoCodec> {
        pub fn add<T2>(&self, value: T)
            -> Result<(), ::flutter_rust_bridge::Rust2DartSendError> where
            T: ::flutter_rust_bridge::IntoIntoDart<T2>,
            T2: ::flutter_rust_bridge::IntoDart {
            self.add_raw(::flutter_rust_bridge::for_generated::Rust2DartAction::Success,
                value)
        }
        pub fn add_error<TR, T2>(&self, value: TR)
            -> Result<(), ::flutter_rust_bridge::Rust2DartSendError> where
            TR: ::flutter_rust_bridge::IntoIntoDart<T2>,
            T2: ::flutter_rust_bridge::IntoDart {
            self.add_raw(::flutter_rust_bridge::for_generated::Rust2DartAction::Error,
                value)
        }
        fn add_raw<TR,
            T2>(&self,
            action: ::flutter_rust_bridge::for_generated::Rust2DartAction,
            value: TR)
            -> Result<(), ::flutter_rust_bridge::Rust2DartSendError> where
            TR: ::flutter_rust_bridge::IntoIntoDart<T2>,
            T2: ::flutter_rust_bridge::IntoDart {
            self.base.add_raw(::flutter_rust_bridge::for_generated::DcoCodec::encode(action,
                    value.into_into_dart()))
        }
    }
    impl<T> StreamSink<T, ::flutter_rust_bridge::for_generated::SseCodec>
        where T: SseEncode {
        pub fn add(&self, value: T)
            -> Result<(), ::flutter_rust_bridge::Rust2DartSendError> {
            self.add_raw(::flutter_rust_bridge::for_generated::Rust2DartAction::Success,
                value)
        }
        pub fn add_error<TR: SseEncode>(&self, value: TR)
            -> Result<(), ::flutter_rust_bridge::Rust2DartSendError> {
            self.add_raw(::flutter_rust_bridge::for_generated::Rust2DartAction::Error,
                value)
        }
        pub fn add_raw<TR: SseEncode>(&self,
            action: ::flutter_rust_bridge::for_generated::Rust2DartAction,
            value: TR)
            -> Result<(), ::flutter_rust_bridge::Rust2DartSendError> {
            self.base.add_raw(::flutter_rust_bridge::for_generated::SseCodec::encode(action,
                    |serializer| value.sse_encode(serializer)))
        }
    }
    impl<T, Rust2DartCodec: ::flutter_rust_bridge::for_generated::BaseCodec>
        ::flutter_rust_bridge::IntoIntoDart<StreamSink<T, Rust2DartCodec>> for
        StreamSink<T, Rust2DartCodec> {
        fn into_into_dart(self) -> StreamSink<T, Rust2DartCodec> {
            ::core::panicking::panic("internal error: entered unreachable code")
        }
    }
    impl<T, Rust2DartCodec: ::flutter_rust_bridge::for_generated::BaseCodec>
        ::flutter_rust_bridge::IntoDart for StreamSink<T, Rust2DartCodec> {
        fn into_dart(self) -> ::flutter_rust_bridge::for_generated::DartAbi {
            ::core::panicking::panic("internal error: entered unreachable code")
        }
    }
    pub(crate) const FLUTTER_RUST_BRIDGE_CODEGEN_VERSION: &str = "2.11.1";
    pub(crate) const FLUTTER_RUST_BRIDGE_CODEGEN_CONTENT_HASH: i32 =
        -1116567001;
    #[allow(missing_copy_implementations)]
    #[allow(non_camel_case_types)]
    #[allow(dead_code)]
    pub struct FLUTTER_RUST_BRIDGE_HANDLER {
        __private_field: (),
    }
    #[doc(hidden)]
    #[allow(non_upper_case_globals)]
    pub static FLUTTER_RUST_BRIDGE_HANDLER: FLUTTER_RUST_BRIDGE_HANDLER =
        FLUTTER_RUST_BRIDGE_HANDLER { __private_field: () };
    impl ::lazy_static::__Deref for FLUTTER_RUST_BRIDGE_HANDLER {
        type Target =
            ::flutter_rust_bridge::DefaultHandler<::flutter_rust_bridge::for_generated::SimpleThreadPool>;
        fn deref(&self)
            ->
                &::flutter_rust_bridge::DefaultHandler<::flutter_rust_bridge::for_generated::SimpleThreadPool> {
            #[inline(always)]
            fn __static_ref_initialize()
                ->
                    ::flutter_rust_bridge::DefaultHandler<::flutter_rust_bridge::for_generated::SimpleThreadPool> {
                {
                    match (&FLUTTER_RUST_BRIDGE_CODEGEN_VERSION,
                            &flutter_rust_bridge::for_generated::FLUTTER_RUST_BRIDGE_RUNTIME_VERSION)
                        {
                        (left_val, right_val) => {
                            if !(*left_val == *right_val) {
                                let kind = ::core::panicking::AssertKind::Eq;
                                ::core::panicking::assert_failed(kind, &*left_val,
                                    &*right_val,
                                    ::core::option::Option::Some(format_args!("Please ensure flutter_rust_bridge\'s codegen ({0}) and runtime ({1}) versions are the same",
                                            FLUTTER_RUST_BRIDGE_CODEGEN_VERSION,
                                            flutter_rust_bridge::for_generated::FLUTTER_RUST_BRIDGE_RUNTIME_VERSION)));
                            }
                        }
                    };
                    ::flutter_rust_bridge::DefaultHandler::new_simple(Default::default())
                }
            }
            #[inline(always)]
            fn __stability()
                ->
                    &'static ::flutter_rust_bridge::DefaultHandler<::flutter_rust_bridge::for_generated::SimpleThreadPool> {
                static LAZY:
                    ::lazy_static::lazy::Lazy<::flutter_rust_bridge::DefaultHandler<::flutter_rust_bridge::for_generated::SimpleThreadPool>>
                    =
                    ::lazy_static::lazy::Lazy::INIT;
                LAZY.get(__static_ref_initialize)
            }
            __stability()
        }
    }
    impl ::lazy_static::LazyStatic for FLUTTER_RUST_BRIDGE_HANDLER {
        fn initialize(lazy: &Self) { let _ = &**lazy; }
    }
    fn wire__crate__api__CastContext_create_device_from_info_impl(port_:
            flutter_rust_bridge::for_generated::MessagePort,
        ptr_:
            flutter_rust_bridge::for_generated::PlatformGeneralizedUint8ListPtr,
        rust_vec_len_: i32, data_len_: i32) {
        FLUTTER_RUST_BRIDGE_HANDLER.wrap_normal::<flutter_rust_bridge::for_generated::SseCodec,
            _,
            _>(flutter_rust_bridge::for_generated::TaskInfo {
                debug_name: "CastContext_create_device_from_info",
                port: Some(port_),
                mode: flutter_rust_bridge::for_generated::FfiCallMode::Normal,
            },
            move ||
                {
                    let message =
                        unsafe {
                            flutter_rust_bridge::for_generated::Dart2RustMessageSse::from_wire(ptr_,
                                rust_vec_len_, data_len_)
                        };
                    let mut deserializer =
                        flutter_rust_bridge::for_generated::SseDeserializer::new(message);
                    let api_that =
                        <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>>>::sse_decode(&mut deserializer);
                    let api_info =
                        <crate::api::DeviceInfo>::sse_decode(&mut deserializer);
                    deserializer.end();
                    move |context|
                        {
                            transform_result_sse::<_,
                                    ()>((move ||
                                            {
                                                let mut api_that_guard = None;
                                                let decode_indices_ =
                                                    flutter_rust_bridge::for_generated::lockable_compute_decode_order(<[_]>::into_vec(::alloc::boxed::box_new([flutter_rust_bridge::for_generated::LockableOrderInfo::new(&api_that,
                                                                            0, false)])));
                                                for i in decode_indices_ {
                                                    match i {
                                                        0 =>
                                                            api_that_guard = Some(api_that.lockable_decode_sync_ref()),
                                                        _ =>
                                                            ::core::panicking::panic("internal error: entered unreachable code"),
                                                    }
                                                }
                                                let api_that_guard = api_that_guard.unwrap();
                                                let output_ok =
                                                    Result::<_,
                                                                ()>::Ok(crate::api::CastContext::create_device_from_info(&*api_that_guard,
                                                                api_info))?;
                                                Ok(output_ok)
                                            })())
                        }
                })
    }
    fn wire__crate__api__CastContext_new_impl(port_:
            flutter_rust_bridge::for_generated::MessagePort,
        ptr_:
            flutter_rust_bridge::for_generated::PlatformGeneralizedUint8ListPtr,
        rust_vec_len_: i32, data_len_: i32) {
        FLUTTER_RUST_BRIDGE_HANDLER.wrap_normal::<flutter_rust_bridge::for_generated::SseCodec,
            _,
            _>(flutter_rust_bridge::for_generated::TaskInfo {
                debug_name: "CastContext_new",
                port: Some(port_),
                mode: flutter_rust_bridge::for_generated::FfiCallMode::Normal,
            },
            move ||
                {
                    let message =
                        unsafe {
                            flutter_rust_bridge::for_generated::Dart2RustMessageSse::from_wire(ptr_,
                                rust_vec_len_, data_len_)
                        };
                    let mut deserializer =
                        flutter_rust_bridge::for_generated::SseDeserializer::new(message);
                    deserializer.end();
                    move |context|
                        {
                            transform_result_sse::<_,
                                    crate::api::ErrorMessage>((move ||
                                            {
                                                let output_ok = crate::api::CastContext::new()?;
                                                Ok(output_ok)
                                            })())
                        }
                })
    }
    fn wire__crate__api__device_info_from_url_impl(port_:
            flutter_rust_bridge::for_generated::MessagePort,
        ptr_:
            flutter_rust_bridge::for_generated::PlatformGeneralizedUint8ListPtr,
        rust_vec_len_: i32, data_len_: i32) {
        FLUTTER_RUST_BRIDGE_HANDLER.wrap_normal::<flutter_rust_bridge::for_generated::SseCodec,
            _,
            _>(flutter_rust_bridge::for_generated::TaskInfo {
                debug_name: "device_info_from_url",
                port: Some(port_),
                mode: flutter_rust_bridge::for_generated::FfiCallMode::Normal,
            },
            move ||
                {
                    let message =
                        unsafe {
                            flutter_rust_bridge::for_generated::Dart2RustMessageSse::from_wire(ptr_,
                                rust_vec_len_, data_len_)
                        };
                    let mut deserializer =
                        flutter_rust_bridge::for_generated::SseDeserializer::new(message);
                    let api_url = <String>::sse_decode(&mut deserializer);
                    deserializer.end();
                    move |context|
                        {
                            transform_result_sse::<_,
                                    ()>((move ||
                                            {
                                                let output_ok =
                                                    Result::<_,
                                                                ()>::Ok(crate::api::device_info_from_url(api_url))?;
                                                Ok(output_ok)
                                            })())
                        }
                })
    }
    #[allow(clippy::unnecessary_literal_unwrap)]
    const _: fn() =
        ||
            {
                {
                    let DeviceInfo = None::<crate::api::DeviceInfo>.unwrap();
                    let _: String = DeviceInfo.name;
                    let _: crate::api::ProtocolType = DeviceInfo.protocol;
                    let _: Vec<crate::api::IpAddr> = DeviceInfo.addresses;
                    let _: u16 = DeviceInfo.port;
                }
                match None::<crate::api::IpAddr>.unwrap() {
                    crate::api::IpAddr::V4 { o1, o2, o3, o4 } => {
                        let _: u8 = o1;
                        let _: u8 = o2;
                        let _: u8 = o3;
                        let _: u8 = o4;
                    }
                    crate::api::IpAddr::V6 {
                        o1,
                        o2,
                        o3,
                        o4,
                        o5,
                        o6,
                        o7,
                        o8,
                        o9,
                        o10,
                        o11,
                        o12,
                        o13,
                        o14,
                        o15,
                        o16,
                        scope_id } => {
                        let _: u8 = o1;
                        let _: u8 = o2;
                        let _: u8 = o3;
                        let _: u8 = o4;
                        let _: u8 = o5;
                        let _: u8 = o6;
                        let _: u8 = o7;
                        let _: u8 = o8;
                        let _: u8 = o9;
                        let _: u8 = o10;
                        let _: u8 = o11;
                        let _: u8 = o12;
                        let _: u8 = o13;
                        let _: u8 = o14;
                        let _: u8 = o15;
                        let _: u8 = o16;
                        let _: u32 = scope_id;
                    }
                }
            };
    impl MoiArcValue for
        flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext> {
        fn get_pool() -> &'static MoiArcPool<Self> {
            #[allow(missing_copy_implementations)]
            #[allow(non_camel_case_types)]
            #[allow(dead_code)]
            struct POOL {
                __private_field: (),
            }
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            static POOL: POOL = POOL { __private_field: () };
            impl ::lazy_static::__Deref for POOL {
                type Target =
                    MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>>;
                fn deref(&self)
                    ->
                        &MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>> {
                    #[inline(always)]
                    fn __static_ref_initialize()
                        ->
                            MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>> {
                        MoiArcPool::new(Default::default())
                    }
                    #[inline(always)]
                    fn __stability()
                        ->
                            &'static MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>> {
                        static LAZY:
                            ::lazy_static::lazy::Lazy<MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>>>
                            =
                            ::lazy_static::lazy::Lazy::INIT;
                        LAZY.get(__static_ref_initialize)
                    }
                    __stability()
                }
            }
            impl ::lazy_static::LazyStatic for POOL {
                fn initialize(lazy: &Self) { let _ = &**lazy; }
            }
            ;
            &POOL
        }
    }
    impl MoiArcValue for
        flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>
        {
        fn get_pool() -> &'static MoiArcPool<Self> {
            #[allow(missing_copy_implementations)]
            #[allow(non_camel_case_types)]
            #[allow(dead_code)]
            struct POOL {
                __private_field: (),
            }
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            static POOL: POOL = POOL { __private_field: () };
            impl ::lazy_static::__Deref for POOL {
                type Target =
                    MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>>;
                fn deref(&self)
                    ->
                        &MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>> {
                    #[inline(always)]
                    fn __static_ref_initialize()
                        ->
                            MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>> {
                        MoiArcPool::new(Default::default())
                    }
                    #[inline(always)]
                    fn __stability()
                        ->
                            &'static MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>> {
                        static LAZY:
                            ::lazy_static::lazy::Lazy<MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>>>
                            =
                            ::lazy_static::lazy::Lazy::INIT;
                        LAZY.get(__static_ref_initialize)
                    }
                    __stability()
                }
            }
            impl ::lazy_static::LazyStatic for POOL {
                fn initialize(lazy: &Self) { let _ = &**lazy; }
            }
            ;
            &POOL
        }
    }
    impl MoiArcValue for
        flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>
        {
        fn get_pool() -> &'static MoiArcPool<Self> {
            #[allow(missing_copy_implementations)]
            #[allow(non_camel_case_types)]
            #[allow(dead_code)]
            struct POOL {
                __private_field: (),
            }
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            static POOL: POOL = POOL { __private_field: () };
            impl ::lazy_static::__Deref for POOL {
                type Target =
                    MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>>;
                fn deref(&self)
                    ->
                        &MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>> {
                    #[inline(always)]
                    fn __static_ref_initialize()
                        ->
                            MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>> {
                        MoiArcPool::new(Default::default())
                    }
                    #[inline(always)]
                    fn __stability()
                        ->
                            &'static MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>> {
                        static LAZY:
                            ::lazy_static::lazy::Lazy<MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>>>
                            =
                            ::lazy_static::lazy::Lazy::INIT;
                        LAZY.get(__static_ref_initialize)
                    }
                    __stability()
                }
            }
            impl ::lazy_static::LazyStatic for POOL {
                fn initialize(lazy: &Self) { let _ = &**lazy; }
            }
            ;
            &POOL
        }
    }
    impl MoiArcValue for
        flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>
        {
        fn get_pool() -> &'static MoiArcPool<Self> {
            #[allow(missing_copy_implementations)]
            #[allow(non_camel_case_types)]
            #[allow(dead_code)]
            struct POOL {
                __private_field: (),
            }
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            static POOL: POOL = POOL { __private_field: () };
            impl ::lazy_static::__Deref for POOL {
                type Target =
                    MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>>;
                fn deref(&self)
                    ->
                        &MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>> {
                    #[inline(always)]
                    fn __static_ref_initialize()
                        ->
                            MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>> {
                        MoiArcPool::new(Default::default())
                    }
                    #[inline(always)]
                    fn __stability()
                        ->
                            &'static MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>> {
                        static LAZY:
                            ::lazy_static::lazy::Lazy<MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>>>
                            =
                            ::lazy_static::lazy::Lazy::INIT;
                        LAZY.get(__static_ref_initialize)
                    }
                    __stability()
                }
            }
            impl ::lazy_static::LazyStatic for POOL {
                fn initialize(lazy: &Self) { let _ = &**lazy; }
            }
            ;
            &POOL
        }
    }
    impl MoiArcValue for
        flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>
        {
        fn get_pool() -> &'static MoiArcPool<Self> {
            #[allow(missing_copy_implementations)]
            #[allow(non_camel_case_types)]
            #[allow(dead_code)]
            struct POOL {
                __private_field: (),
            }
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            static POOL: POOL = POOL { __private_field: () };
            impl ::lazy_static::__Deref for POOL {
                type Target =
                    MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>>;
                fn deref(&self)
                    ->
                        &MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>> {
                    #[inline(always)]
                    fn __static_ref_initialize()
                        ->
                            MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>> {
                        MoiArcPool::new(Default::default())
                    }
                    #[inline(always)]
                    fn __stability()
                        ->
                            &'static MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>> {
                        static LAZY:
                            ::lazy_static::lazy::Lazy<MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>>>
                            =
                            ::lazy_static::lazy::Lazy::INIT;
                        LAZY.get(__static_ref_initialize)
                    }
                    __stability()
                }
            }
            impl ::lazy_static::LazyStatic for POOL {
                fn initialize(lazy: &Self) { let _ = &**lazy; }
            }
            ;
            &POOL
        }
    }
    impl MoiArcValue for
        flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>
        {
        fn get_pool() -> &'static MoiArcPool<Self> {
            #[allow(missing_copy_implementations)]
            #[allow(non_camel_case_types)]
            #[allow(dead_code)]
            struct POOL {
                __private_field: (),
            }
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            static POOL: POOL = POOL { __private_field: () };
            impl ::lazy_static::__Deref for POOL {
                type Target =
                    MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>>;
                fn deref(&self)
                    ->
                        &MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>> {
                    #[inline(always)]
                    fn __static_ref_initialize()
                        ->
                            MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>> {
                        MoiArcPool::new(Default::default())
                    }
                    #[inline(always)]
                    fn __stability()
                        ->
                            &'static MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>> {
                        static LAZY:
                            ::lazy_static::lazy::Lazy<MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>>>
                            =
                            ::lazy_static::lazy::Lazy::INIT;
                        LAZY.get(__static_ref_initialize)
                    }
                    __stability()
                }
            }
            impl ::lazy_static::LazyStatic for POOL {
                fn initialize(lazy: &Self) { let _ = &**lazy; }
            }
            ;
            &POOL
        }
    }
    impl MoiArcValue for
        flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source> {
        fn get_pool() -> &'static MoiArcPool<Self> {
            #[allow(missing_copy_implementations)]
            #[allow(non_camel_case_types)]
            #[allow(dead_code)]
            struct POOL {
                __private_field: (),
            }
            #[doc(hidden)]
            #[allow(non_upper_case_globals)]
            static POOL: POOL = POOL { __private_field: () };
            impl ::lazy_static::__Deref for POOL {
                type Target =
                    MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>>;
                fn deref(&self)
                    ->
                        &MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>> {
                    #[inline(always)]
                    fn __static_ref_initialize()
                        ->
                            MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>> {
                        MoiArcPool::new(Default::default())
                    }
                    #[inline(always)]
                    fn __stability()
                        ->
                            &'static MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>> {
                        static LAZY:
                            ::lazy_static::lazy::Lazy<MoiArcPool<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>>>
                            =
                            ::lazy_static::lazy::Lazy::INIT;
                        LAZY.get(__static_ref_initialize)
                    }
                    __stability()
                }
            }
            impl ::lazy_static::LazyStatic for POOL {
                fn initialize(lazy: &Self) { let _ = &**lazy; }
            }
            ;
            &POOL
        }
    }
    impl SseDecode for CastContext {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner =
                <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>>>::sse_decode(deserializer);
            return flutter_rust_bridge::for_generated::rust_auto_opaque_decode_owned(inner);
        }
    }
    impl SseDecode for CastingDevice {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner =
                <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>>>::sse_decode(deserializer);
            return flutter_rust_bridge::for_generated::rust_auto_opaque_decode_owned(inner);
        }
    }
    impl SseDecode for _DeviceConnectionState {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner =
                <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>>>::sse_decode(deserializer);
            return flutter_rust_bridge::for_generated::rust_auto_opaque_decode_owned(inner);
        }
    }
    impl SseDecode for _GenericKeyEvent {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner =
                <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>>>::sse_decode(deserializer);
            return flutter_rust_bridge::for_generated::rust_auto_opaque_decode_owned(inner);
        }
    }
    impl SseDecode for _GenericMediaEvent {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner =
                <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>>>::sse_decode(deserializer);
            return flutter_rust_bridge::for_generated::rust_auto_opaque_decode_owned(inner);
        }
    }
    impl SseDecode for _PlaybackState {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner =
                <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>>>::sse_decode(deserializer);
            return flutter_rust_bridge::for_generated::rust_auto_opaque_decode_owned(inner);
        }
    }
    impl SseDecode for _Source {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner =
                <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>>>::sse_decode(deserializer);
            return flutter_rust_bridge::for_generated::rust_auto_opaque_decode_owned(inner);
        }
    }
    impl SseDecode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>>
        {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner = <usize>::sse_decode(deserializer);
            return decode_rust_opaque_moi(inner);
        }
    }
    impl SseDecode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>>
        {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner = <usize>::sse_decode(deserializer);
            return decode_rust_opaque_moi(inner);
        }
    }
    impl SseDecode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>>
        {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner = <usize>::sse_decode(deserializer);
            return decode_rust_opaque_moi(inner);
        }
    }
    impl SseDecode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>>
        {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner = <usize>::sse_decode(deserializer);
            return decode_rust_opaque_moi(inner);
        }
    }
    impl SseDecode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>>
        {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner = <usize>::sse_decode(deserializer);
            return decode_rust_opaque_moi(inner);
        }
    }
    impl SseDecode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>>
        {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner = <usize>::sse_decode(deserializer);
            return decode_rust_opaque_moi(inner);
        }
    }
    impl SseDecode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>>
        {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner = <usize>::sse_decode(deserializer);
            return decode_rust_opaque_moi(inner);
        }
    }
    impl SseDecode for String {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner = <Vec<u8>>::sse_decode(deserializer);
            return String::from_utf8(inner).unwrap();
        }
    }
    impl SseDecode for crate::api::DeviceInfo {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut var_name = <String>::sse_decode(deserializer);
            let mut var_protocol =
                <crate::api::ProtocolType>::sse_decode(deserializer);
            let mut var_addresses =
                <Vec<crate::api::IpAddr>>::sse_decode(deserializer);
            let mut var_port = <u16>::sse_decode(deserializer);
            return crate::api::DeviceInfo {
                    name: var_name,
                    protocol: var_protocol,
                    addresses: var_addresses,
                    port: var_port,
                };
        }
    }
    impl SseDecode for crate::api::ErrorMessage {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut tag_ = <i32>::sse_decode(deserializer);
            match tag_ {
                0 => {
                    let mut var_field0 = <String>::sse_decode(deserializer);
                    return crate::api::ErrorMessage::Error(var_field0);
                }
                _ => {
                    {
                        ::core::panicking::panic_fmt(format_args!("not implemented: {0}",
                                format_args!("")));
                    };
                }
            }
        }
    }
    impl SseDecode for f64 {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            deserializer.cursor.read_f64::<NativeEndian>().unwrap()
        }
    }
    impl SseDecode for i32 {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            deserializer.cursor.read_i32::<NativeEndian>().unwrap()
        }
    }
    impl SseDecode for crate::api::IpAddr {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut tag_ = <i32>::sse_decode(deserializer);
            match tag_ {
                0 => {
                    let mut var_o1 = <u8>::sse_decode(deserializer);
                    let mut var_o2 = <u8>::sse_decode(deserializer);
                    let mut var_o3 = <u8>::sse_decode(deserializer);
                    let mut var_o4 = <u8>::sse_decode(deserializer);
                    return crate::api::IpAddr::V4 {
                            o1: var_o1,
                            o2: var_o2,
                            o3: var_o3,
                            o4: var_o4,
                        };
                }
                1 => {
                    let mut var_o1 = <u8>::sse_decode(deserializer);
                    let mut var_o2 = <u8>::sse_decode(deserializer);
                    let mut var_o3 = <u8>::sse_decode(deserializer);
                    let mut var_o4 = <u8>::sse_decode(deserializer);
                    let mut var_o5 = <u8>::sse_decode(deserializer);
                    let mut var_o6 = <u8>::sse_decode(deserializer);
                    let mut var_o7 = <u8>::sse_decode(deserializer);
                    let mut var_o8 = <u8>::sse_decode(deserializer);
                    let mut var_o9 = <u8>::sse_decode(deserializer);
                    let mut var_o10 = <u8>::sse_decode(deserializer);
                    let mut var_o11 = <u8>::sse_decode(deserializer);
                    let mut var_o12 = <u8>::sse_decode(deserializer);
                    let mut var_o13 = <u8>::sse_decode(deserializer);
                    let mut var_o14 = <u8>::sse_decode(deserializer);
                    let mut var_o15 = <u8>::sse_decode(deserializer);
                    let mut var_o16 = <u8>::sse_decode(deserializer);
                    let mut var_scopeId = <u32>::sse_decode(deserializer);
                    return crate::api::IpAddr::V6 {
                            o1: var_o1,
                            o2: var_o2,
                            o3: var_o3,
                            o4: var_o4,
                            o5: var_o5,
                            o6: var_o6,
                            o7: var_o7,
                            o8: var_o8,
                            o9: var_o9,
                            o10: var_o10,
                            o11: var_o11,
                            o12: var_o12,
                            o13: var_o13,
                            o14: var_o14,
                            o15: var_o15,
                            o16: var_o16,
                            scope_id: var_scopeId,
                        };
                }
                _ => {
                    {
                        ::core::panicking::panic_fmt(format_args!("not implemented: {0}",
                                format_args!("")));
                    };
                }
            }
        }
    }
    impl SseDecode for Vec<crate::api::IpAddr> {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut len_ = <i32>::sse_decode(deserializer);
            let mut ans_ = ::alloc::vec::Vec::new();
            for idx_ in 0..len_ {
                ans_.push(<crate::api::IpAddr>::sse_decode(deserializer));
            }
            return ans_;
        }
    }
    impl SseDecode for Vec<u8> {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut len_ = <i32>::sse_decode(deserializer);
            let mut ans_ = ::alloc::vec::Vec::new();
            for idx_ in 0..len_ { ans_.push(<u8>::sse_decode(deserializer)); }
            return ans_;
        }
    }
    impl SseDecode for Option<crate::api::DeviceInfo> {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            if (<bool>::sse_decode(deserializer)) {
                return Some(<crate::api::DeviceInfo>::sse_decode(deserializer));
            } else { return None; }
        }
    }
    impl SseDecode for crate::api::ProtocolType {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            let mut inner = <i32>::sse_decode(deserializer);
            return match inner {
                    0 => crate::api::ProtocolType::Chromecast,
                    1 => crate::api::ProtocolType::FCast,
                    _ => {
                        ::core::panicking::panic_fmt(format_args!("internal error: entered unreachable code: {0}",
                                format_args!("Invalid variant for ProtocolType: {0}",
                                    inner)));
                    }
                };
        }
    }
    impl SseDecode for u16 {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            deserializer.cursor.read_u16::<NativeEndian>().unwrap()
        }
    }
    impl SseDecode for u32 {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            deserializer.cursor.read_u32::<NativeEndian>().unwrap()
        }
    }
    impl SseDecode for u8 {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            deserializer.cursor.read_u8().unwrap()
        }
    }
    impl SseDecode for () {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {}
    }
    impl SseDecode for usize {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            deserializer.cursor.read_u64::<NativeEndian>().unwrap() as _
        }
    }
    impl SseDecode for bool {
        fn sse_decode(deserializer:
                &mut flutter_rust_bridge::for_generated::SseDeserializer)
            -> Self {
            deserializer.cursor.read_u8().unwrap() != 0
        }
    }
    fn pde_ffi_dispatcher_primary_impl(func_id: i32,
        port: flutter_rust_bridge::for_generated::MessagePort,
        ptr:
            flutter_rust_bridge::for_generated::PlatformGeneralizedUint8ListPtr,
        rust_vec_len: i32, data_len: i32) {
        match func_id {
            1 =>
                wire__crate__api__CastContext_create_device_from_info_impl(port,
                    ptr, rust_vec_len, data_len),
            2 =>
                wire__crate__api__CastContext_new_impl(port, ptr,
                    rust_vec_len, data_len),
            13 =>
                wire__crate__api__device_info_from_url_impl(port, ptr,
                    rust_vec_len, data_len),
            _ =>
                ::core::panicking::panic("internal error: entered unreachable code"),
        }
    }
    fn pde_ffi_dispatcher_sync_impl(func_id: i32,
        ptr:
            flutter_rust_bridge::for_generated::PlatformGeneralizedUint8ListPtr,
        rust_vec_len: i32, data_len: i32)
        -> flutter_rust_bridge::for_generated::WireSyncRust2DartSse {
        match func_id {
            _ =>
                ::core::panicking::panic("internal error: entered unreachable code"),
        }
    }
    impl flutter_rust_bridge::IntoDart for FrbWrapper<CastContext> {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self.0).into_dart()
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        FrbWrapper<CastContext> {}
    impl flutter_rust_bridge::IntoIntoDart<FrbWrapper<CastContext>> for
        CastContext {
        fn into_into_dart(self) -> FrbWrapper<CastContext> { self.into() }
    }
    impl flutter_rust_bridge::IntoDart for FrbWrapper<CastingDevice> {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self.0).into_dart()
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        FrbWrapper<CastingDevice> {}
    impl flutter_rust_bridge::IntoIntoDart<FrbWrapper<CastingDevice>> for
        CastingDevice {
        fn into_into_dart(self) -> FrbWrapper<CastingDevice> { self.into() }
    }
    impl flutter_rust_bridge::IntoDart for FrbWrapper<_DeviceConnectionState>
        {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self.0).into_dart()
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        FrbWrapper<_DeviceConnectionState> {}
    impl flutter_rust_bridge::IntoIntoDart<FrbWrapper<_DeviceConnectionState>>
        for _DeviceConnectionState {
        fn into_into_dart(self) -> FrbWrapper<_DeviceConnectionState> {
            self.into()
        }
    }
    impl flutter_rust_bridge::IntoDart for FrbWrapper<_GenericKeyEvent> {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self.0).into_dart()
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        FrbWrapper<_GenericKeyEvent> {}
    impl flutter_rust_bridge::IntoIntoDart<FrbWrapper<_GenericKeyEvent>> for
        _GenericKeyEvent {
        fn into_into_dart(self) -> FrbWrapper<_GenericKeyEvent> {
            self.into()
        }
    }
    impl flutter_rust_bridge::IntoDart for FrbWrapper<_GenericMediaEvent> {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self.0).into_dart()
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        FrbWrapper<_GenericMediaEvent> {}
    impl flutter_rust_bridge::IntoIntoDart<FrbWrapper<_GenericMediaEvent>> for
        _GenericMediaEvent {
        fn into_into_dart(self) -> FrbWrapper<_GenericMediaEvent> {
            self.into()
        }
    }
    impl flutter_rust_bridge::IntoDart for FrbWrapper<_PlaybackState> {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self.0).into_dart()
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        FrbWrapper<_PlaybackState> {}
    impl flutter_rust_bridge::IntoIntoDart<FrbWrapper<_PlaybackState>> for
        _PlaybackState {
        fn into_into_dart(self) -> FrbWrapper<_PlaybackState> { self.into() }
    }
    impl flutter_rust_bridge::IntoDart for FrbWrapper<_Source> {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self.0).into_dart()
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        FrbWrapper<_Source> {}
    impl flutter_rust_bridge::IntoIntoDart<FrbWrapper<_Source>> for _Source {
        fn into_into_dart(self) -> FrbWrapper<_Source> { self.into() }
    }
    impl flutter_rust_bridge::IntoDart for FrbWrapper<crate::api::DeviceInfo>
        {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            [self.0.name.into_into_dart().into_dart(),
                        self.0.protocol.into_into_dart().into_dart(),
                        self.0.addresses.into_into_dart().into_dart(),
                        self.0.port.into_into_dart().into_dart()].into_dart()
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        FrbWrapper<crate::api::DeviceInfo> {}
    impl flutter_rust_bridge::IntoIntoDart<FrbWrapper<crate::api::DeviceInfo>>
        for crate::api::DeviceInfo {
        fn into_into_dart(self) -> FrbWrapper<crate::api::DeviceInfo> {
            self.into()
        }
    }
    impl flutter_rust_bridge::IntoDart for crate::api::ErrorMessage {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            match self {
                crate::api::ErrorMessage::Error(field0) => {
                    [0.into_dart(),
                                field0.into_into_dart().into_dart()].into_dart()
                }
                _ => {
                    {
                        ::core::panicking::panic_fmt(format_args!("not implemented: {0}",
                                format_args!("")));
                    };
                }
            }
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        crate::api::ErrorMessage {}
    impl flutter_rust_bridge::IntoIntoDart<crate::api::ErrorMessage> for
        crate::api::ErrorMessage {
        fn into_into_dart(self) -> crate::api::ErrorMessage { self }
    }
    impl flutter_rust_bridge::IntoDart for FrbWrapper<crate::api::IpAddr> {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            match self.0 {
                crate::api::IpAddr::V4 { o1, o2, o3, o4 } =>
                    [0.into_dart(), o1.into_into_dart().into_dart(),
                                o2.into_into_dart().into_dart(),
                                o3.into_into_dart().into_dart(),
                                o4.into_into_dart().into_dart()].into_dart(),
                crate::api::IpAddr::V6 {
                    o1,
                    o2,
                    o3,
                    o4,
                    o5,
                    o6,
                    o7,
                    o8,
                    o9,
                    o10,
                    o11,
                    o12,
                    o13,
                    o14,
                    o15,
                    o16,
                    scope_id } =>
                    [1.into_dart(), o1.into_into_dart().into_dart(),
                                o2.into_into_dart().into_dart(),
                                o3.into_into_dart().into_dart(),
                                o4.into_into_dart().into_dart(),
                                o5.into_into_dart().into_dart(),
                                o6.into_into_dart().into_dart(),
                                o7.into_into_dart().into_dart(),
                                o8.into_into_dart().into_dart(),
                                o9.into_into_dart().into_dart(),
                                o10.into_into_dart().into_dart(),
                                o11.into_into_dart().into_dart(),
                                o12.into_into_dart().into_dart(),
                                o13.into_into_dart().into_dart(),
                                o14.into_into_dart().into_dart(),
                                o15.into_into_dart().into_dart(),
                                o16.into_into_dart().into_dart(),
                                scope_id.into_into_dart().into_dart()].into_dart(),
                _ => {
                    {
                        ::core::panicking::panic_fmt(format_args!("not implemented: {0}",
                                format_args!("")));
                    };
                }
            }
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        FrbWrapper<crate::api::IpAddr> {}
    impl flutter_rust_bridge::IntoIntoDart<FrbWrapper<crate::api::IpAddr>> for
        crate::api::IpAddr {
        fn into_into_dart(self) -> FrbWrapper<crate::api::IpAddr> {
            self.into()
        }
    }
    impl flutter_rust_bridge::IntoDart for
        FrbWrapper<crate::api::ProtocolType> {
        fn into_dart(self) -> flutter_rust_bridge::for_generated::DartAbi {
            match self.0 {
                crate::api::ProtocolType::Chromecast => 0.into_dart(),
                crate::api::ProtocolType::FCast => 1.into_dart(),
                _ =>
                    ::core::panicking::panic("internal error: entered unreachable code"),
            }
        }
    }
    impl flutter_rust_bridge::for_generated::IntoDartExceptPrimitive for
        FrbWrapper<crate::api::ProtocolType> {}
    impl flutter_rust_bridge::IntoIntoDart<FrbWrapper<crate::api::ProtocolType>>
        for crate::api::ProtocolType {
        fn into_into_dart(self) -> FrbWrapper<crate::api::ProtocolType> {
            self.into()
        }
    }
    impl SseEncode for CastContext {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>>>::sse_encode(flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self), serializer);
        }
    }
    impl SseEncode for CastingDevice {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>>>::sse_encode(flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self), serializer);
        }
    }
    impl SseEncode for _DeviceConnectionState {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>>>::sse_encode(flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self), serializer);
        }
    }
    impl SseEncode for _GenericKeyEvent {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>>>::sse_encode(flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self), serializer);
        }
    }
    impl SseEncode for _GenericMediaEvent {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>>>::sse_encode(flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self), serializer);
        }
    }
    impl SseEncode for _PlaybackState {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>>>::sse_encode(flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self), serializer);
        }
    }
    impl SseEncode for _Source {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>>>::sse_encode(flutter_rust_bridge::for_generated::rust_auto_opaque_encode::<_,
                        MoiArc<_>>(self), serializer);
        }
    }
    impl SseEncode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>>
        {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            let (ptr, size) = self.sse_encode_raw();
            <usize>::sse_encode(ptr, serializer);
            <i32>::sse_encode(size, serializer);
        }
    }
    impl SseEncode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>>
        {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            let (ptr, size) = self.sse_encode_raw();
            <usize>::sse_encode(ptr, serializer);
            <i32>::sse_encode(size, serializer);
        }
    }
    impl SseEncode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>>
        {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            let (ptr, size) = self.sse_encode_raw();
            <usize>::sse_encode(ptr, serializer);
            <i32>::sse_encode(size, serializer);
        }
    }
    impl SseEncode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>>
        {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            let (ptr, size) = self.sse_encode_raw();
            <usize>::sse_encode(ptr, serializer);
            <i32>::sse_encode(size, serializer);
        }
    }
    impl SseEncode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>>
        {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            let (ptr, size) = self.sse_encode_raw();
            <usize>::sse_encode(ptr, serializer);
            <i32>::sse_encode(size, serializer);
        }
    }
    impl SseEncode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>>
        {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            let (ptr, size) = self.sse_encode_raw();
            <usize>::sse_encode(ptr, serializer);
            <i32>::sse_encode(size, serializer);
        }
    }
    impl SseEncode for
        RustOpaqueMoi<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>>
        {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            let (ptr, size) = self.sse_encode_raw();
            <usize>::sse_encode(ptr, serializer);
            <i32>::sse_encode(size, serializer);
        }
    }
    impl SseEncode for String {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <Vec<u8>>::sse_encode(self.into_bytes(), serializer);
        }
    }
    impl SseEncode for crate::api::DeviceInfo {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <String>::sse_encode(self.name, serializer);
            <crate::api::ProtocolType>::sse_encode(self.protocol, serializer);
            <Vec<crate::api::IpAddr>>::sse_encode(self.addresses, serializer);
            <u16>::sse_encode(self.port, serializer);
        }
    }
    impl SseEncode for crate::api::ErrorMessage {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            match self {
                crate::api::ErrorMessage::Error(field0) => {
                    <i32>::sse_encode(0, serializer);
                    <String>::sse_encode(field0, serializer);
                }
                _ => {
                    {
                        ::core::panicking::panic_fmt(format_args!("not implemented: {0}",
                                format_args!("")));
                    };
                }
            }
        }
    }
    impl SseEncode for f64 {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            serializer.cursor.write_f64::<NativeEndian>(self).unwrap();
        }
    }
    impl SseEncode for i32 {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            serializer.cursor.write_i32::<NativeEndian>(self).unwrap();
        }
    }
    impl SseEncode for crate::api::IpAddr {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            match self {
                crate::api::IpAddr::V4 { o1, o2, o3, o4 } => {
                    <i32>::sse_encode(0, serializer);
                    <u8>::sse_encode(o1, serializer);
                    <u8>::sse_encode(o2, serializer);
                    <u8>::sse_encode(o3, serializer);
                    <u8>::sse_encode(o4, serializer);
                }
                crate::api::IpAddr::V6 {
                    o1,
                    o2,
                    o3,
                    o4,
                    o5,
                    o6,
                    o7,
                    o8,
                    o9,
                    o10,
                    o11,
                    o12,
                    o13,
                    o14,
                    o15,
                    o16,
                    scope_id } => {
                    <i32>::sse_encode(1, serializer);
                    <u8>::sse_encode(o1, serializer);
                    <u8>::sse_encode(o2, serializer);
                    <u8>::sse_encode(o3, serializer);
                    <u8>::sse_encode(o4, serializer);
                    <u8>::sse_encode(o5, serializer);
                    <u8>::sse_encode(o6, serializer);
                    <u8>::sse_encode(o7, serializer);
                    <u8>::sse_encode(o8, serializer);
                    <u8>::sse_encode(o9, serializer);
                    <u8>::sse_encode(o10, serializer);
                    <u8>::sse_encode(o11, serializer);
                    <u8>::sse_encode(o12, serializer);
                    <u8>::sse_encode(o13, serializer);
                    <u8>::sse_encode(o14, serializer);
                    <u8>::sse_encode(o15, serializer);
                    <u8>::sse_encode(o16, serializer);
                    <u32>::sse_encode(scope_id, serializer);
                }
                _ => {
                    {
                        ::core::panicking::panic_fmt(format_args!("not implemented: {0}",
                                format_args!("")));
                    };
                }
            }
        }
    }
    impl SseEncode for Vec<crate::api::IpAddr> {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <i32>::sse_encode(self.len() as _, serializer);
            for item in self {
                <crate::api::IpAddr>::sse_encode(item, serializer);
            }
        }
    }
    impl SseEncode for Vec<u8> {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <i32>::sse_encode(self.len() as _, serializer);
            for item in self { <u8>::sse_encode(item, serializer); }
        }
    }
    impl SseEncode for Option<crate::api::DeviceInfo> {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <bool>::sse_encode(self.is_some(), serializer);
            if let Some(value) = self {
                <crate::api::DeviceInfo>::sse_encode(value, serializer);
            }
        }
    }
    impl SseEncode for crate::api::ProtocolType {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            <i32>::sse_encode(match self {
                    crate::api::ProtocolType::Chromecast => 0,
                    crate::api::ProtocolType::FCast => 1,
                    _ => {
                        {
                            ::core::panicking::panic_fmt(format_args!("not implemented: {0}",
                                    format_args!("")));
                        };
                    }
                }, serializer);
        }
    }
    impl SseEncode for u16 {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            serializer.cursor.write_u16::<NativeEndian>(self).unwrap();
        }
    }
    impl SseEncode for u32 {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            serializer.cursor.write_u32::<NativeEndian>(self).unwrap();
        }
    }
    impl SseEncode for u8 {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            serializer.cursor.write_u8(self).unwrap();
        }
    }
    impl SseEncode for () {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {}
    }
    impl SseEncode for usize {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            serializer.cursor.write_u64::<NativeEndian>(self as _).unwrap();
        }
    }
    impl SseEncode for bool {
        fn sse_encode(self,
            serializer:
                &mut flutter_rust_bridge::for_generated::SseSerializer) {
            serializer.cursor.write_u8(self as _).unwrap();
        }
    }
    mod io {
        use super::*;
        use crate::api::*;
        use crate::*;
        use flutter_rust_bridge::for_generated::byteorder::{
            NativeEndian, ReadBytesExt, WriteBytesExt,
        };
        use flutter_rust_bridge::for_generated::{
            transform_result_dco, Lifetimeable, Lockable,
        };
        use flutter_rust_bridge::{Handler, IntoIntoDart};
        pub trait NewWithNullPtr {
            fn new_with_null_ptr()
            -> Self;
        }
        impl<T> NewWithNullPtr for *mut T {
            fn new_with_null_ptr() -> Self { std::ptr::null_mut() }
        }
        #[no_mangle]
        pub extern "C" fn frb_get_rust_content_hash() -> i32 {
            FLUTTER_RUST_BRIDGE_CODEGEN_CONTENT_HASH
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frb_pde_ffi_dispatcher_primary(func_id: i32,
            port_: i64, ptr_: *mut u8, rust_vec_len_: i32, data_len_: i32) {
            pde_ffi_dispatcher_primary_impl(func_id, port_, ptr_,
                rust_vec_len_, data_len_)
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frb_pde_ffi_dispatcher_sync(func_id: i32,
            ptr_: *mut u8, rust_vec_len_: i32, data_len_: i32)
            -> ::flutter_rust_bridge::for_generated::WireSyncRust2DartSse {
            pde_ffi_dispatcher_sync_impl(func_id, ptr_, rust_vec_len_,
                data_len_)
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frb_dart_fn_deliver_output(call_id: i32,
            ptr_: *mut u8, rust_vec_len_: i32, data_len_: i32) {
            let message =
                unsafe {
                    ::flutter_rust_bridge::for_generated::Dart2RustMessageSse::from_wire(ptr_,
                        rust_vec_len_, data_len_)
                };
            FLUTTER_RUST_BRIDGE_HANDLER.dart_fn_handle_output(call_id,
                message)
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_increment_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInnerCastContext(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>>::increment_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_decrement_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInnerCastContext(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastContext>>::decrement_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_increment_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInnerCastingDevice(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>>::increment_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_decrement_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInnerCastingDevice(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<CastingDevice>>::decrement_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_increment_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInner_DeviceConnectionState(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>>::increment_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_decrement_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInner_DeviceConnectionState(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_DeviceConnectionState>>::decrement_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_increment_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInner_GenericKeyEvent(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>>::increment_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_decrement_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInner_GenericKeyEvent(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericKeyEvent>>::decrement_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_increment_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInner_GenericMediaEvent(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>>::increment_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_decrement_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInner_GenericMediaEvent(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_GenericMediaEvent>>::decrement_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_increment_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInner_PlaybackState(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>>::increment_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_decrement_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInner_PlaybackState(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_PlaybackState>>::decrement_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_increment_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInner_Source(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>>::increment_strong_count(ptr
                    as _);
        }
        #[unsafe(no_mangle)]
        pub extern "C" fn frbgen_flutter_plugin_rust_arc_decrement_strong_count_RustOpaque_flutter_rust_bridgefor_generatedRustAutoOpaqueInner_Source(ptr:
                *const std::ffi::c_void) {
            MoiArc::<flutter_rust_bridge::for_generated::RustAutoOpaqueInner<_Source>>::decrement_strong_count(ptr
                    as _);
        }
    }
    pub use io::*;
}