use std::{collections::HashMap, time::Duration};

use flatbuffers::FlatBufferBuilder;
use serde_json::Value;

pub mod fcast_flatbuffers {
    #![allow(dead_code)]
    #![allow(unused_imports)]
    #![allow(clippy::extra_unused_lifetimes)]
    #![allow(clippy::missing_safety_doc)]
    #![allow(clippy::derivable_impls)]

    include!(concat!(env!("OUT_DIR"), "/flatbuffers/fcast_generated.rs"));
}

pub use flatbuffers;

pub use fcast_flatbuffers::fcast::{v4 as flat, v4::PlaybackState};

pub const MAX_PACKET_SIZE: usize = 512 * 1024;

pub struct ConstructedMessage<'a> {
    builder: flatbuffers::FlatBufferBuilder<'a>,
}

impl std::fmt::Debug for ConstructedMessage<'_> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ConstructedMessage { ... }")
    }
}

impl PartialEq for ConstructedMessage<'_> {
    fn eq(&self, _other: &Self) -> bool {
        false
    }
}

impl std::ops::Deref for ConstructedMessage<'_> {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        self.builder.finished_data()
    }
}

pub struct MessageBuilder<'a> {
    builder: flatbuffers::FlatBufferBuilder<'a>,
}

use paste::paste;
use smol_str::SmolStr;

macro_rules! create_msg {
    ($self:expr, $name:expr, $($field:ident $(: $value:expr)? ),* $(,)?) => {{
        paste! {
            let value = flat::[<$name>]::create(
                &mut $self.builder,
                &flat:: [<$name Args>] {
                    $($field: create_msg!(@value $field $(: $value)?)),*
                }

            ).as_union_value();
            $self.create_and_finish_envelope(flat::Message::[<$name>], value)
        }
    }};
    (@value $field:ident : $value:expr) => { $value };
    (@value $field:ident) => { $field };
}

macro_rules! create_str {
    ($self:expr, $str:expr) => {
        Some($self.builder.create_string(&$str))
    };
}

macro_rules! maybe_create_str {
    ($self:expr, $str:expr) => {
        if let Some(s) = $str {
            create_str!($self, s)
        } else {
            None
        }
    };
}

macro_rules! create_device_info {
    ($self:expr, $device_info:expr) => {{
        let args = flat::DeviceInfoArgs {
            display_name: maybe_create_str!($self, &$device_info.display_name),
            app_name: maybe_create_str!($self, &$device_info.app_name),
            app_version: maybe_create_str!($self, &$device_info.app_version),
        };

        flat::DeviceInfo::create(&mut $self.builder, &args)
    }};
}

impl<'a> MessageBuilder<'a> {
    #[allow(clippy::new_without_default)]
    pub fn new() -> Self {
        Self {
            builder: FlatBufferBuilder::new(),
        }
    }

    fn create_and_finish_envelope(
        mut self,
        payload_type: flat::Message,
        payload: flatbuffers::WIPOffset<flatbuffers::UnionWIPOffset>,
    ) -> ConstructedMessage<'a> {
        let envelope = flat::Packet::create(
            &mut self.builder,
            &flat::PacketArgs {
                payload_type,
                payload: Some(payload),
            },
        );

        self.builder.finish(envelope, None);

        ConstructedMessage {
            builder: self.builder,
        }
    }

    pub fn progress_changed_raw(
        mut self,
        position: Option<&flat::Time>,
        duration: Option<&flat::Time>,
    ) -> ConstructedMessage<'a> {
        create_msg!(self, ProgressChanged, position, duration)
    }

    pub fn progress_changed(
        self,
        position: flat::Time,
        duration: flat::Time,
    ) -> ConstructedMessage<'a> {
        self.progress_changed_raw(Some(&position), Some(&duration))
    }

    pub fn set_progress_update_interval_raw(
        mut self,
        interval: Option<&flat::Time>,
    ) -> ConstructedMessage<'a> {
        create_msg!(self, SetProgressUpdateInterval, interval)
    }

    pub fn set_progress_update_interval(self, interval: flat::Time) -> ConstructedMessage<'a> {
        self.set_progress_update_interval_raw(Some(&interval))
    }

    pub fn volume_changed(mut self, volume: f32) -> ConstructedMessage<'a> {
        create_msg!(self, VolumeChanged, volume)
    }

    pub fn speed_changed(mut self, speed: f32) -> ConstructedMessage<'a> {
        create_msg!(self, SpeedChanged, speed)
    }

    pub fn playback_state_changed(mut self, state: flat::PlaybackState) -> ConstructedMessage<'a> {
        create_msg!(self, PlaybackStateChanged, state)
    }

    pub fn change_track(
        mut self,
        id: Option<u32>,
        typ: flat::MediaTrackType,
    ) -> ConstructedMessage<'a> {
        create_msg!(self, ChangeTrack, id, track_type: typ)
    }

    pub fn add_subtitle_source(
        mut self,
        url: &str,
        select: bool,
        name: Option<&str>,
    ) -> ConstructedMessage<'a> {
        let url = self.builder.create_string(url);
        let name = name.map(|n| self.builder.create_string(n));
        create_msg!(
            self,
            AddSubtitleSource,
            url: Some(url),
            select,
            name,
            metadata: None
        )
    }

    pub fn companion_hello_request(mut self) -> ConstructedMessage<'a> {
        create_msg!(self, CompanionHelloRequest,)
    }

    pub fn companion_hello_response(mut self, provider_id: u16) -> ConstructedMessage<'a> {
        create_msg!(self, CompanionHelloResponse, provider_id)
    }

    pub fn companion_resource_info_request(
        mut self,
        request_id: u32,
        resource_id: u32,
    ) -> ConstructedMessage<'a> {
        create_msg!(self, CompanionResourceInfoRequest, request_id, resource_id)
    }

    pub fn companion_resource_info_response(
        mut self,
        request_id: u32,
        content_type: &str,
        size: Option<u64>,
    ) -> ConstructedMessage<'a> {
        let content_type = self.builder.create_string(content_type);
        let (resource_size_type, resource_size) = match size {
            Some(size) => (
                flat::CompanionResourceSize::Known,
                flat::KnownResourceSize::create(
                    &mut self.builder,
                    &flat::KnownResourceSizeArgs { size },
                )
                .as_union_value(),
            ),
            None => (
                flat::CompanionResourceSize::Unknown,
                flat::UnknownResourceSize::create(
                    &mut self.builder,
                    &flat::UnknownResourceSizeArgs {},
                )
                .as_union_value(),
            ),
        };
        create_msg!(
            self,
            CompanionResourceInfoResponse,
            request_id,
            content_type: Some(content_type),
            resource_size_type: resource_size_type,
            resource_size: Some(resource_size)
        )
    }

    pub fn sender_introduction(mut self, device_info: &DeviceInfo) -> ConstructedMessage<'a> {
        let device_info = create_device_info!(self, device_info);
        create_msg!(self, SenderIntroduction, device_info: Some(device_info))
    }

    pub fn start_mirroring_session(mut self, session_id: u16) -> ConstructedMessage<'a> {
        create_msg!(self, StartMirroringSession, session_id)
    }

    pub fn mirroring_session_description(
        mut self,
        session_id: u16,
        sdp: &str,
    ) -> ConstructedMessage<'a> {
        let sdp = self.builder.create_string(sdp);
        create_msg!(self, MirroringSessionDescription, session_id, sdp: Some(sdp))
    }

    pub fn stop_playback(mut self) -> ConstructedMessage<'a> {
        create_msg!(self, StopPlayback,)
    }

    fn strip_flat_media_item(
        &mut self,
        item: flat::MediaItem<'_>,
    ) -> flatbuffers::WIPOffset<flat::MediaItem<'a>> {
        let args = flat::MediaItemArgs {
            container: create_str!(self, item.container()),
            source_url: create_str!(self, item.source_url()),
            start_time: item.start_time(),
            volume: item.volume(),
            speed: item.speed(),
            headers: None, // Don't include potentially sensitive values
            title: item.title().map(|s| self.builder.create_string(s)),
            thumbnail_url: item.thumbnail_url().map(|s| self.builder.create_string(s)),
            metadata_type: flat::Metadata::NONE,
            metadata: None,
            extra_metadata: None,
        };
        flat::MediaItem::create(&mut self.builder, &args)
    }

    pub fn from_play_stripped(mut self, play: &flat::Load) -> Option<ConstructedMessage<'a>> {
        let (source_type, source) = match play.source_type() {
            flat::MediaSource::Single => {
                let item = self
                    .strip_flat_media_item(play.source_as_single()?)
                    .as_union_value();
                (flat::MediaSource::Single, item)
            }
            flat::MediaSource::Queue => {
                let msg = play.source_as_queue()?;
                let queue_items = msg
                    .items()
                    .iter()
                    .map(|queue_item| {
                        let item = self.strip_flat_media_item(queue_item.media_item());
                        flat::QueueItem::create(
                            &mut self.builder,
                            &flat::QueueItemArgs {
                                media_item: Some(item),
                                playback_duration: queue_item.playback_duration(),
                            },
                        )
                    })
                    .collect::<Vec<_>>();

                let items = self.builder.create_vector(&queue_items);
                let queue = flat::Queue::create(
                    &mut self.builder,
                    &flat::QueueArgs {
                        items: Some(items),
                        start_index: msg.start_index(),
                        autoplay: msg.autoplay(),
                    },
                )
                .as_union_value();

                (flat::MediaSource::Queue, queue)
            }
            _ => return None,
        };

        Some(create_msg!(self, Load, source_type, source: Some(source)))
    }

    pub fn from_queue_insert_stripped(
        mut self,
        insert: &flat::QueueInsert,
    ) -> Option<ConstructedMessage<'a>> {
        let position = match insert.position_type() {
            flat::QueuePosition::Index => QueuePosition::Index(insert.position_as_index()?.index()),
            flat::QueuePosition::Front => QueuePosition::Front,
            flat::QueuePosition::Back => QueuePosition::Back,
            _ => return None,
        };
        let (pos_type, pos) = self.queue_position(position);
        let item = self.strip_flat_media_item(insert.item().media_item());
        let q_item = flat::QueueItem::create(
            &mut self.builder,
            &flat::QueueItemArgs {
                media_item: Some(item),
                playback_duration: insert.item().playback_duration(),
            },
        );
        Some(
            create_msg!(self, QueueInsert, item: Some(q_item), position_type: pos_type, position: Some(pos)),
        )
    }

    fn construct_media_item(
        &mut self,
        item: MediaItem,
    ) -> flatbuffers::WIPOffset<flat::MediaItem<'a>> {
        let headers = if let Some(headers) = item.headers {
            let elems = headers
                .iter()
                .map(|(k, v)| {
                    let header = flat::RequestHeaderArgs {
                        key: create_str!(self, k),
                        value: create_str!(self, v),
                    };
                    flat::RequestHeader::create(&mut self.builder, &header)
                })
                .collect::<Vec<_>>();
            Some(self.builder.create_vector(&elems))
        } else {
            None
        };

        let (metadata_type, metadata) = match item.metadata {
            Some(Metadata::Video { .. }) => {
                let meta = flat::VideoMetadataArgs { chapters: None };
                let meta = flat::VideoMetadata::create(&mut self.builder, &meta).as_union_value();
                (flat::Metadata::Video, Some(meta))
            }
            Some(Metadata::Audio { artist, album }) => {
                let meta = flat::AudioMetadataArgs {
                    artist: maybe_create_str!(self, artist),
                    album: maybe_create_str!(self, album),
                    chapters: None,
                };
                let meta = flat::AudioMetadata::create(&mut self.builder, &meta).as_union_value();
                (flat::Metadata::Audio, Some(meta))
            }
            None => (flat::Metadata::NONE, None),
        };

        let start_time = item
            .start_time
            .map(|s| flat::Time::new(Duration::from_secs_f64(s).as_micros() as u64));
        let item = flat::MediaItemArgs {
            container: create_str!(self, item.container),
            source_url: create_str!(self, item.source_url),
            start_time: start_time.as_ref(),
            volume: item.volume,
            speed: item.speed,
            headers,
            title: maybe_create_str!(self, item.title),
            thumbnail_url: maybe_create_str!(self, item.thumbnail_url),
            metadata_type,
            metadata,
            extra_metadata: None, // TODO
        };

        flat::MediaItem::create(&mut self.builder, &item)
    }

    pub fn load_single(mut self, item: MediaItem) -> ConstructedMessage<'a> {
        let item = self.construct_media_item(item).as_union_value();
        create_msg!(self, Load, source_type: flat::MediaSource::Single, source: Some(item))
    }

    pub fn load_queue<'m>(
        mut self,
        items: impl Iterator<Item = MediaItem<'m>>,
        start_index: Option<u8>,
    ) -> ConstructedMessage<'a> {
        let items = items
            .map(|item| {
                let item = self.construct_media_item(item);
                flat::QueueItem::create(
                    &mut self.builder,
                    &flat::QueueItemArgs {
                        media_item: Some(item),
                        playback_duration: None,
                    },
                )
            })
            .collect::<Vec<_>>();

        let items = self.builder.create_vector(&items);
        let queue = flat::Queue::create(
            &mut self.builder,
            &flat::QueueArgs {
                items: Some(items),
                start_index,
                autoplay: false,
            },
        )
        .as_union_value();

        create_msg!(self, Load, source_type: flat::MediaSource::Queue, source: Some(queue))
    }

    fn queue_position(
        &mut self,
        position: QueuePosition,
    ) -> (
        flat::QueuePosition,
        flatbuffers::WIPOffset<flatbuffers::UnionWIPOffset>,
    ) {
        match position {
            QueuePosition::Index(index) => (
                flat::QueuePosition::Index,
                flat::QueueIndex::create(&mut self.builder, &flat::QueueIndexArgs { index })
                    .as_union_value(),
            ),
            QueuePosition::Front => (
                flat::QueuePosition::Front,
                flat::QueueMarkerFront::create(&mut self.builder, &flat::QueueMarkerFrontArgs {})
                    .as_union_value(),
            ),
            QueuePosition::Back => (
                flat::QueuePosition::Back,
                flat::QueueMarkerBack::create(&mut self.builder, &flat::QueueMarkerBackArgs {})
                    .as_union_value(),
            ),
        }
    }

    pub fn queue_remove(mut self, position: QueuePosition) -> ConstructedMessage<'a> {
        let (typ, position) = self.queue_position(position);
        create_msg!(self, QueueRemove, position_type: typ, position: Some(position))
    }

    pub fn queue_insert(
        mut self,
        item: MediaItem,
        position: QueuePosition,
    ) -> ConstructedMessage<'a> {
        let (pos_type, position) = self.queue_position(position);
        let item = self.construct_media_item(item);
        let q_item = flat::QueueItem::create(
            &mut self.builder,
            &flat::QueueItemArgs {
                media_item: Some(item),
                playback_duration: None,
            },
        );
        create_msg!(self, QueueInsert, item: Some(q_item), position_type: pos_type, position: Some(position))
    }

    pub fn queue_select(mut self, position: QueuePosition) -> ConstructedMessage<'a> {
        let (position_type, position) = self.queue_position(position);
        create_msg!(self, QueueItemSelected, position_type, position: Some(position))
    }

    pub fn tracks_available(
        mut self,
        tracks: impl Iterator<Item = MediaTrack>,
    ) -> ConstructedMessage<'a> {
        let tracks_vec = tracks
            .map(|track| {
                let title = if let Some(title) = track.title {
                    Some(self.builder.create_string(&title))
                } else {
                    None
                };
                let iso_639 = self.builder.create_string(&track.iso_639);
                let (metadata_type, metadata) = match track.metadata {
                    Some(MediaTrackMetadata::Video) => (
                        flat::MediaTrackMetadata::Video,
                        Some(
                            flat::VideoTrackMeta::create(
                                &mut self.builder,
                                &flat::VideoTrackMetaArgs { resolution: None },
                            )
                            .as_union_value(),
                        ),
                    ),
                    Some(MediaTrackMetadata::Audio) => (
                        flat::MediaTrackMetadata::Audio,
                        Some(
                            flat::AudioTrackMeta::create(
                                &mut self.builder,
                                &flat::AudioTrackMetaArgs {},
                            )
                            .as_union_value(),
                        ),
                    ),
                    Some(MediaTrackMetadata::Subtitle) => (
                        flat::MediaTrackMetadata::Subtitle,
                        Some(
                            flat::SubtitleTrackMeta::create(
                                &mut self.builder,
                                &flat::SubtitleTrackMetaArgs {},
                            )
                            .as_union_value(),
                        ),
                    ),
                    None => (flat::MediaTrackMetadata::NONE, None),
                };
                flat::MediaTrack::create(
                    &mut self.builder,
                    &flat::MediaTrackArgs {
                        id: track.id,
                        title,
                        iso_639: Some(iso_639),
                        metadata_type,
                        metadata,
                    },
                )
            })
            .collect::<Vec<_>>();
        let tracks = self.builder.create_vector(&tracks_vec);

        create_msg!(self, TracksAvailable, tracks: Some(tracks))
    }

    fn create_str_vector(
        &mut self,
        strs: impl Iterator<Item = &'static str>,
    ) -> flatbuffers::WIPOffset<flatbuffers::Vector<'a, flatbuffers::ForwardsUOffset<&'a str>>>
    {
        let strs = strs
            .map(|p| self.builder.create_string(p))
            .collect::<Vec<_>>();
        self.builder.create_vector(&strs)
    }

    pub fn receiver_introduction(
        mut self,
        device_info: &DeviceInfo,
        supported_streaming_protocols: impl Iterator<Item = &'static str>,
        supported_containers: impl Iterator<Item = &'static str>,
        supported_video_formats: impl Iterator<Item = &'static str>,
        supported_audio_formats: impl Iterator<Item = &'static str>,
        supported_subtitle_formats: impl Iterator<Item = &'static str>,
        supported_hdr_formats: impl Iterator<Item = &'static str>,
        supported_image_formats: impl Iterator<Item = &'static str>,
        supports_external_subtitles: bool,
        supports_mirroring: bool,
        volume_step_interval: f32,
    ) -> ConstructedMessage<'a> {
        let device_info = Some(create_device_info!(self, device_info));
        let protocols = self.create_str_vector(supported_streaming_protocols);
        let containers = self.create_str_vector(supported_containers);
        let videos = self.create_str_vector(supported_video_formats);
        let audios = self.create_str_vector(supported_audio_formats);
        let subtitles = self.create_str_vector(supported_subtitle_formats);
        let hdrs = self.create_str_vector(supported_hdr_formats);
        let images = self.create_str_vector(supported_image_formats);

        let media_capabilities = flat::MediaCapabilities::create(
            &mut self.builder,
            &flat::MediaCapabilitiesArgs {
                protocols: Some(protocols),
                containers: Some(containers),
                video_formats: Some(videos),
                audio_formats: Some(audios),
                subtitle_formats: Some(subtitles),
                hdr_formats: Some(hdrs),
                image_formats: Some(images),
                external_subtitles: supports_external_subtitles,
                mirroring: supports_mirroring,
            },
        );
        let display_capabilities = flat::DisplayCapabilities::create(
            &mut self.builder,
            &flat::DisplayCapabilitiesArgs { resolution: None },
        );
        let audio_capabilities = flat::AudioCapabilities::create(
            &mut self.builder,
            &flat::AudioCapabilitiesArgs {
                volume_step_interval,
            },
        );

        let capabilities = Some(flat::ReceiverCapabilities::create(
            &mut self.builder,
            &flat::ReceiverCapabilitiesArgs {
                media: Some(media_capabilities),
                display: Some(display_capabilities),
                audio: Some(audio_capabilities),
            },
        ));

        create_msg!(self, ReceiverIntroduction, device_info, capabilities)
    }

    pub fn error(
        mut self,
        packet_num: Option<u32>,
        kind: flat::ErrorKind,
    ) -> ConstructedMessage<'a> {
        create_msg!(self, Error, kind, packet_num)
    }

    pub fn companion_resource_request(
        mut self,
        request_id: u32,
        resource_id: u32,
        read_head: Option<flat::ResourceReadHead>,
    ) -> ConstructedMessage<'a> {
        create_msg!(self, CompanionResourceRequest, request_id, resource_id, read_head: read_head.as_ref())
    }
}

pub enum Metadata {
    Video {
        subtitle_url: Option<String>,
    },
    Audio {
        artist: Option<String>,
        album: Option<String>,
    },
}

pub struct MediaItem<'a> {
    /// The MIME type
    pub container: String,
    pub source_url: String,
    /// The time to start playing in seconds
    pub start_time: Option<f64>,
    /// The desired volume (0-1)
    pub volume: Option<f32>,
    /// Initial playback speed
    pub speed: Option<f32>,
    /// HTTP request headers to add to the play request
    pub headers: Option<HashMap<String, String>>,
    pub title: Option<&'a str>,
    pub thumbnail_url: Option<&'a str>,
    pub metadata: Option<Metadata>,
    pub extra_metadata: Option<HashMap<String, Value>>,
}

#[derive(Debug)]
pub struct DeviceInfo {
    pub display_name: Option<String>,
    pub app_name: Option<String>,
    pub app_version: Option<String>,
}

pub enum MediaTrackMetadata {
    Video,
    Audio,
    Subtitle,
}

pub struct MediaTrack {
    pub id: u32,
    pub title: Option<SmolStr>,
    pub iso_639: SmolStr,
    pub metadata: Option<MediaTrackMetadata>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
pub enum QueuePosition {
    Index(u8),
    Front,
    Back,
}
