#![feature(ip)]

use anyhow::{Context, Result, bail};
use base64::Engine;
use bytes::Bytes;
use fcast_protocol::{Opcode, PlaybackState, SetVolumeMessage, v2::VolumeUpdateMessage, v3};
use futures::StreamExt;
use gst::prelude::*;
use gst_play::prelude::*;
use image::ImageFormat;
use parking_lot::Mutex;
use session::{SessionDriver, SessionId};
use slint::{ToSharedString, VecModel};
use smallvec::SmallVec;
use tokio::{
    net::TcpListener,
    sync::{
        broadcast,
        mpsc::{self, UnboundedReceiver, UnboundedSender},
        oneshot,
    },
};
use tracing::{Instrument, debug, debug_span, error, info, level_filters::LevelFilter, warn};

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, Ipv6Addr, SocketAddr},
    rc::Rc,
    sync::{
        Arc,
        atomic::{self, AtomicBool},
    },
    time::{Duration, Instant},
};

#[cfg(target_os = "android")]
pub use tracing;

pub use slint;
mod fcastwhepsrcbin;
mod player;
mod session;
// mod small_vec_model; // For later
mod video;

use crate::session::{Operation, ReceiverToSenderMessage, TranslatableMessage};

#[derive(Debug, thiserror::Error)]
pub enum DownloadImageError {
    #[error("failed to send request: {0}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("response is missing content type")]
    MissingContentType,
    #[error("response has invalid content type")]
    InvalidContentType,
    #[error("content type is not a string")]
    ContentTypeIsNotString,
    #[error("content type is unsupported")]
    UnsupportedContentType,
    #[error("failed to decode image: {0}")]
    DecodeImage(#[from] image::ImageError),
}

type SlintRgba8Pixbuf = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

#[derive(Debug)]
pub enum MdnsEvent {
    NameSet(String),
    IpAdded(IpAddr),
    IpRemoved(IpAddr),
    SetIps(Vec<IpAddr>),
}

#[derive(Debug)]
pub enum Event {
    Quit,
    SessionFinished,
    ResumeOrPause,
    SeekPercent(f32),
    ToggleDebug,
    Player(gst::Message),
    Op {
        session_id: SessionId,
        op: Operation,
    },
    ImageDownloadResult {
        id: ImageDownloadId,
        res: std::result::Result<(Bytes, ImageFormat), DownloadImageError>,
    },
    AudioThumbnailAvailable {
        id: ImageId,
        preview: SlintRgba8Pixbuf,
    },
    AudioThumbnailBlurAvailable {
        id: ImageId,
        blured: SlintRgba8Pixbuf,
    },
    ImageDecoded {
        id: ImageId,
        image: SlintRgba8Pixbuf,
    },
    Mdns(MdnsEvent),
}

#[macro_export]
macro_rules! log_if_err {
    ($res:expr) => {
        if let Err(err) = $res {
            error!("{err}");
        }
    };
}

const FCAST_TCP_PORT: u16 = 46899;
const SENDER_UPDATE_INTERVAL: Duration = Duration::from_millis(700);

slint::include_modules!();

fn current_time_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("UNIX_EPOCH is always earlier than now")
        .as_millis() as u64
}

#[derive(Debug)]
enum OnUriLoadedCommand {
    Volume(f64),
}

#[derive(Debug)]
enum OnFirstPlayingStateChangedCommand {
    Seek(f64),
    Rate(f64),
}

struct BoolLock(bool);

impl BoolLock {
    pub fn new() -> Self {
        Self(false)
    }

    pub fn acquire(&mut self) {
        self.0 = true;
    }

    pub fn release(&mut self) {
        self.0 = false;
    }

    pub fn is_locked(&self) -> bool {
        self.0
    }
}

enum ImageDecodeJobType {
    AudioThumbnail,
    Regular,
}

enum EncodedImageData {
    Vec(Vec<u8>),
    Bytes(Bytes),
}

impl std::ops::Deref for EncodedImageData {
    type Target = [u8];

    fn deref(&self) -> &Self::Target {
        match self {
            Self::Vec(vec) => vec.as_slice(),
            Self::Bytes(bytes) => &bytes,
        }
    }
}

struct ImageDecodeJob {
    image: EncodedImageData,
    format: Option<image::ImageFormat>,
    typ: ImageDecodeJobType,
}

pub type ImageId = u32;
pub type ImageDownloadId = u32;

fn image_decode_worker(
    job_rx: std::sync::mpsc::Receiver<(ImageId, ImageDecodeJob)>,
    event_tx: UnboundedSender<Event>,
) -> anyhow::Result<()> {
    // libheif_rs::integration::image::register_all_decoding_hooks();

    while let Ok((id, job)) = job_rx.recv() {
        debug!(?id, "Got job");

        let decode_res = match job.format {
            Some(format) => image::load_from_memory_with_format(&job.image, format),
            None => image::load_from_memory(&job.image),
        };

        let decoded = match decode_res {
            Ok(img) => img.to_rgba8(),
            Err(err) => {
                // TODO: should notify about failure
                error!(?err, "Failed to decode image");
                continue;
            }
        };

        fn to_slint_pixbuf(img: &image::RgbaImage) -> SlintRgba8Pixbuf {
            slint::SharedPixelBuffer::<slint::Rgba8Pixel>::clone_from_slice(
                img.as_raw(),
                img.width(),
                img.height(),
            )
        }

        match job.typ {
            ImageDecodeJobType::AudioThumbnail => {
                let preview = to_slint_pixbuf(&decoded);
                event_tx.send(Event::AudioThumbnailAvailable { id, preview })?;
                let blured = to_slint_pixbuf(&image::imageops::fast_blur(&decoded, 64.0));
                event_tx.send(Event::AudioThumbnailBlurAvailable { id, blured })?;
            }
            ImageDecodeJobType::Regular => {
                event_tx.send(Event::ImageDecoded {
                    id,
                    image: to_slint_pixbuf(&decoded),
                })?;
            }
        }
    }

    info!("Image decoding worker finished");

    Ok(())
}

// struct PlaylistPlaybackState {
//     playlist: v3::PlaylistContent,
//     current_item_idx: usize,
// }

// TODO: store either single item or playlist etc.

struct Application {
    event_tx: UnboundedSender<Event>,
    ui_weak: slint::Weak<MainWindow>,
    updates_tx: broadcast::Sender<Arc<ReceiverToSenderMessage>>,
    #[cfg(not(target_os = "android"))]
    mdns: mdns_sd::ServiceDaemon,
    last_sent_update: Instant,
    debug_mode: bool,
    player: gst_play::Play,
    player_state: gst_play::PlayState,
    current_media: Option<gst_play::PlayMediaInfo>,
    current_duration: Option<gst::ClockTime>,
    on_uri_loaded_command_queue: smallvec::SmallVec<[OnUriLoadedCommand; 1]>,
    on_playing_command_queue: smallvec::SmallVec<[OnFirstPlayingStateChangedCommand; 2]>,
    current_image_id: ImageId,
    current_image_download_id: ImageDownloadId,
    have_audio_track_cover: bool,
    video_sink_is_eos: Arc<AtomicBool>,
    current_play_data: Option<v3::PlayMessage>,
    have_media_info: bool,
    current_thumbnail_id: ImageId,
    pending_thumbnail: Option<ImageId>,
    pending_thumbnail_download: Option<ImageDownloadId>,
    image_decode_tx: std::sync::mpsc::Sender<(ImageId, ImageDecodeJob)>,
    current_addresses: HashSet<IpAddr>,
    have_media_title: bool,
    volume_lock: BoolLock,
    seek_lock: BoolLock,
    last_position_updated: f64,
    http_client: reqwest::Client,
    current_request_headers: Arc<Mutex<Option<HashMap<String, String>>>>,
    current_playlist: Option<v3::PlaylistContent>,
    current_playlist_item_idx: Option<usize>,
    device_name: Option<String>,
}

impl Application {
    pub async fn new(
        appsink: gst::Element,
        event_tx: UnboundedSender<Event>,
        ui_weak: slint::Weak<MainWindow>,
        video_sink_is_eos: Arc<AtomicBool>,
    ) -> Result<Self> {
        let video_renderer = gst_play::PlayVideoOverlayVideoRenderer::with_sink(&appsink);
        let player =
            gst_play::Play::new(Some(video_renderer.upcast::<gst_play::PlayVideoRenderer>()));

        let mut player_config = player.config();
        player_config.set_position_update_interval(250);
        player_config.set_seek_accurate(true);
        player
            .set_config(player_config)
            .context("Failed to set gst player config")?;

        let registry = gst::Registry::get();
        // Seems better than souphttpsrc
        if let Some(reqwest_src) = registry.lookup_feature("reqwesthttpsrc") {
            reqwest_src.set_rank(gst::Rank::PRIMARY + 1);
        }

        let headers = Arc::new(Mutex::new(None));

        let player_playbin = player.pipeline();
        player_playbin.connect("element-setup", false, {
            let headers = Arc::clone(&headers);
            move |vals| {
                let Ok(elem) = vals[1].get::<gst::Element>() else {
                    return None;
                };

                let name = elem.factory()?.name();
                // TODO: should check for http clients and include headers
                match name.as_str() {
                    "rtspsrc" => elem.set_property("latency", 25u32),
                    "webrtcbin" => elem.set_property("latency", 1u32),
                    "whepsrc" => {
                        let mut caps = gst::Caps::new_empty();
                        {
                            let caps = caps.get_mut().unwrap();

                            let accept = [("VP8", 101)];

                            for (encoding, payload) in accept {
                                let cap = gst::Caps::builder("application/x-rtp")
                                    .field("media", "video")
                                    .field("payload", payload)
                                    .field("encoding-name", encoding)
                                    .field("clock-rate", 90000)
                                    .build();
                                caps.append(cap);
                            }
                        }

                        elem.set_property("video-caps", caps);
                    }
                    "reqwesthttpsrc" => {
                        if let Some(ref headers) = *headers.lock() {
                            let mut extra_headers_builder =
                                gst::Structure::builder("reqwesthttpsrc-extra-headers");
                            for (k, v) in headers {
                                if k == "User-Agent" || k == "user-agent" {
                                    elem.set_property("user-agent", v);
                                } else {
                                    extra_headers_builder = extra_headers_builder.field(k, v);
                                }
                            }
                            elem.set_property("extra-headers", extra_headers_builder.build());
                        }
                    }
                    _ => (),
                }

                None
            }
        });

        // let audiosink = {
        //     let sinkbin = gst::Bin::new();

        //     let pitch = gst::ElementFactory::make("pitch").build()?;
        //     let convert = gst::ElementFactory::make("audioconvert").build()?;
        //     let resample = gst::ElementFactory::make("audioresample").build()?;
        //     let sink = gst::ElementFactory::make("autoaudiosink").build()?;

        //     let elems = [&pitch, &convert, &resample, &sink];
        //     sinkbin.add_many(&elems)?;
        //     gst::Element::link_many(&elems)?;

        //     let ghost = gst::GhostPad::with_target(&pitch.static_pad("sink").unwrap()).unwrap();
        //     sinkbin.add_pad(&ghost).unwrap();

        //     sinkbin
        // };
        // player_playbin.set_property("audio-sink", audiosink);

        tokio::spawn({
            let player_bus = player.message_bus();
            let event_tx = event_tx.clone();
            async move {
                let mut messages = player_bus.stream();

                while let Some(msg) = messages.next().await {
                    let _ = event_tx.send(Event::Player(msg));
                }
            }
        });

        let (updates_tx, _) = broadcast::channel(10);

        // TODO: IPv6?
        // TODO: update addresses when they change on the device, same with qr code
        #[cfg(not(target_os = "android"))]
        let mdns = {
            use if_addrs::get_if_addrs;

            let device_name = format!("FCast-{}", gethostname::gethostname().to_string_lossy());
            let _ = event_tx.send(Event::Mdns(MdnsEvent::NameSet(device_name.clone())));

            if let Ok(ifaces) = get_if_addrs() {
                let event =
                    MdnsEvent::SetIps(ifaces.into_iter().map(|iface| iface.addr.ip()).collect());
                let _ = event_tx.send(Event::Mdns(event));
            }

            let daemon = mdns_sd::ServiceDaemon::new()?;

            let service = mdns_sd::ServiceInfo::new(
                "_fcast._tcp.local.",
                &device_name,
                &format!("{device_name}.local."),
                (), // Auto
                FCAST_TCP_PORT,
                None::<std::collections::HashMap<String, String>>,
            )?
            .enable_addr_auto();

            daemon.register(service)?;

            let monitor = daemon.monitor()?;
            let event_tx = event_tx.clone();
            tokio::spawn(async move {
                while let Ok(event) = monitor.recv_async().await {
                    let event = match event {
                        mdns_sd::DaemonEvent::IpAdd(addr) => MdnsEvent::IpAdded(addr),
                        mdns_sd::DaemonEvent::IpDel(addr) => MdnsEvent::IpRemoved(addr),
                        _ => continue,
                    };
                    let _ = event_tx.send(Event::Mdns(event));
                }
            });

            daemon
        };

        let (image_decode_tx, image_decode_rx) = std::sync::mpsc::channel();
        std::thread::spawn({
            let event_tx = event_tx.clone();
            move || {
                if let Err(err) = image_decode_worker(image_decode_rx, event_tx) {
                    error!(?err, "Image decode worker failed");
                }
            }
        });

        Ok(Self {
            event_tx,
            ui_weak,
            updates_tx,
            #[cfg(not(target_os = "android"))]
            mdns,
            last_sent_update: Instant::now() - SENDER_UPDATE_INTERVAL,
            #[cfg(debug_assertions)]
            debug_mode: true,
            #[cfg(not(debug_assertions))]
            debug_mode: false,
            player,
            player_state: gst_play::PlayState::Stopped,
            current_media: None,
            current_duration: None,
            on_uri_loaded_command_queue: smallvec::SmallVec::new(),
            on_playing_command_queue: smallvec::SmallVec::new(),
            current_image_id: 0,
            have_audio_track_cover: false,
            video_sink_is_eos,
            current_play_data: None,
            have_media_info: false,
            pending_thumbnail: None,
            image_decode_tx,
            current_thumbnail_id: 0,
            current_image_download_id: 0,
            pending_thumbnail_download: None,
            current_addresses: HashSet::new(),
            have_media_title: false,
            volume_lock: BoolLock::new(),
            seek_lock: BoolLock::new(),
            last_position_updated: -1.0,
            http_client: reqwest::Client::new(),
            current_request_headers: headers,
            current_playlist: None,
            current_playlist_item_idx: None,
            device_name: None,
        })
    }

    // TODO: include headers
    async fn download_image(
        client: &reqwest::Client,
        url: &str,
        headers: Option<HashMap<String, String>>,
    ) -> std::result::Result<(Bytes, ImageFormat), DownloadImageError> {
        let mut request = client.get(url);
        if let Some(headers) = headers {
            let mut header_map = reqwest::header::HeaderMap::new();
            for (k, v) in headers.into_iter() {
                let Ok(name) = reqwest::header::HeaderName::from_bytes(k.as_bytes()) else {
                    warn!(k, "Invalid header name");
                    continue;
                };
                let Ok(value) = reqwest::header::HeaderValue::from_bytes(v.as_bytes()) else {
                    warn!(v, "Invalid header value");
                    continue;
                };
                header_map.insert(name, value);
            }
            request = request.headers(header_map);
        }

        let resp = request.send().await?;
        let headers = resp.headers();
        let content_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .ok_or(DownloadImageError::MissingContentType)?
            .to_str()
            .map_err(|_| DownloadImageError::ContentTypeIsNotString)?;
        let format = ImageFormat::from_mime_type(content_type)
            .ok_or(DownloadImageError::UnsupportedContentType)?;

        let body = resp.bytes().await?;
        Ok((body, format))
    }

    fn notify_updates(&mut self, force: bool) -> Result<()> {
        let Some(info) = self.current_media.as_ref() else {
            return Ok(());
        };

        let Some(position) = self.player.position() else {
            error!("No position");
            return Ok(());
        };
        let position = position.seconds_f64();
        self.last_position_updated = position;
        let duration = self
            .current_duration
            .as_ref()
            .unwrap_or(&gst::ClockTime::default())
            .seconds_f64();

        fn sec_to_string(sec: f64) -> String {
            let time_secs = sec % 60.0;
            let time_mins = (sec / 60.0) % 60.0;
            let time_hours = sec / 60.0 / 60.0;

            format!(
                "{:02}:{:02}:{:02}",
                time_hours as u32, time_mins as u32, time_secs as u32,
            )
        }

        let progress_str = sec_to_string(position);
        let duration_str = sec_to_string(duration);
        let progress_percent = (position / duration) as f32;
        let is_live = info.is_live();
        let playback_state = {
            match self.player_state {
                gst_play::PlayState::Stopped => GuiPlaybackState::Loading,
                gst_play::PlayState::Buffering => GuiPlaybackState::Loading,
                gst_play::PlayState::Playing => GuiPlaybackState::Playing,
                gst_play::PlayState::Paused => GuiPlaybackState::Paused,
                _ => return Ok(()),
            }
        };

        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_progress_label(progress_str.into());
            bridge.set_duration_label(duration_str.into());
            if !bridge.get_is_scrubbing_position() {
                bridge.set_playback_position(progress_percent);
            }
            bridge.set_playback_state(playback_state);
            bridge.set_is_live(is_live);
        })?;

        if self.updates_tx.receiver_count() > 0
            && (self.last_sent_update.elapsed() >= SENDER_UPDATE_INTERVAL || force)
        {
            let update = v3::PlaybackUpdateMessage {
                generation_time: current_time_millis(),
                time: Some(position),
                duration: Some(duration),
                state: match playback_state {
                    GuiPlaybackState::Idle | GuiPlaybackState::Loading => PlaybackState::Idle,
                    GuiPlaybackState::Playing => PlaybackState::Playing,
                    GuiPlaybackState::Paused => PlaybackState::Paused,
                },
                speed: Some(self.player.rate()),
                item_index: None,
            };

            debug!("Sending update ({update:?})");

            let msg = ReceiverToSenderMessage::Translatable {
                op: Opcode::PlaybackUpdate,
                msg: TranslatableMessage::PlaybackUpdate(update),
            };
            let _ = self.updates_tx.send(Arc::new(msg));
            self.last_sent_update = Instant::now();
        }

        Ok(())
    }

    fn cleanup_playback_data(&mut self) -> Result<()> {
        self.current_media = None;
        self.current_duration = None;
        self.on_uri_loaded_command_queue.clear();
        self.on_playing_command_queue.clear();
        self.have_audio_track_cover = false;
        self.current_play_data = None;
        self.have_media_info = false;
        self.pending_thumbnail = None;
        self.player_state = gst_play::PlayState::Stopped;
        self.video_sink_is_eos
            .store(true, atomic::Ordering::Relaxed);
        self.have_media_title = false;
        self.seek_lock.release();
        self.volume_lock.release();
        self.last_position_updated = -1.0;
        *self.current_request_headers.lock() = None;
        self.current_playlist = None;
        self.current_playlist_item_idx = None;

        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_video_frame(slint::Image::default());
            bridge.set_image_preview(slint::Image::default());
            bridge.set_audio_track_cover(slint::Image::default());
            bridge.set_blured_audio_track_cover(slint::Image::default());
            bridge.set_overlays(slint::ModelRc::default());

            bridge.set_playing(false);
            bridge.set_playback_position(0.0);
            bridge.set_label("".to_shared_string());
            bridge.set_progress_label("".to_shared_string());
            bridge.set_duration_label("".to_shared_string());
            bridge.set_playback_state(GuiPlaybackState::Idle);
            bridge.set_media_title("".to_shared_string());
            bridge.set_artist_name("".to_shared_string());

            bridge.set_video_dbg(slint::ModelRc::default());
            bridge.set_audio_dbg(slint::ModelRc::default());
            bridge.set_subtitle_dbg(slint::ModelRc::default());
        })?;

        Ok(())
    }

    fn play_message_to_media_item(msg: v3::PlayMessage) -> v3::MediaItem {
        v3::MediaItem {
            container: msg.container,
            url: msg.url,
            content: msg.content,
            time: msg.time,
            volume: msg.volume,
            speed: msg.speed,
            cache: None,
            show_duration: None,
            headers: msg.headers,
            metadata: msg.metadata,
        }
    }

    fn media_loaded_successfully(&mut self) {
        // TODO: v3::EventType::MediaItemStart
        // TODO: needs debouncing since seeks will trigger this too, or maybe not?
        info!("Media loaded successfully");

        if self.updates_tx.receiver_count() > 0
            && let Some(play_msg) = self.current_play_data.clone()
        {
            let event = v3::EventObject::MediaItem {
                variant: v3::EventType::MediaItemStart,
                item: Self::play_message_to_media_item(play_msg),
            };
            let msg = v3::EventMessage {
                generation_time: current_time_millis(),
                event,
            };
            let _ = self
                .updates_tx
                .send(Arc::new(ReceiverToSenderMessage::Event { msg }));
        }

        if let Some(playlist) = self.current_playlist.as_ref()
            && self.updates_tx.receiver_count() > 0
            && let Some(playlist_item_idx) = self.current_playlist_item_idx
        {
            let event = v3::EventObject::MediaItem {
                variant: v3::EventType::MediaItemChange,
                item: playlist.items.get(playlist_item_idx).unwrap(/* TODO: */).clone(),
            };
            let msg = v3::EventMessage {
                generation_time: current_time_millis(),
                event,
            };
            let _ = self
                .updates_tx
                .send(Arc::new(ReceiverToSenderMessage::Event { msg }));
        }
    }

    fn media_ended(&mut self) {
        info!("Media finished");
    }

    fn load_media_item(&mut self, media_item: &v3::MediaItem) -> Result<()> {
        let mut url = if let Some(url) = media_item.url.clone() {
            url
        } else {
            let Some(content) = media_item.content.as_ref() else {
                error!("Play message does not contain a URL or content");
                return Ok(());
            };

            let content_type = match media_item.container.as_str() {
                "application/dash+xml" => "application/dash+xml",
                "application/vnd.apple.mpegurl" | "audio/mpegurl" => "application/x-hls",
                _ => {
                    error!("Invalid content type {}", media_item.container);
                    return Ok(());
                }
            };

            let b64_content = base64::engine::general_purpose::STANDARD.encode(content);

            format!("data:{content_type};base64,{b64_content}")
        };

        self.have_audio_track_cover = false;
        let mut is_for_sure_live = false;
        if media_item.container == "application/x-whep" {
            url = url.replace("http://", "fcastwhep://");
            is_for_sure_live = true;
        }

        self.on_playing_command_queue.clear();

        // Player states
        //  * Loading media
        //  * Image
        //  * Audio only
        //  * Video+audio

        let container = media_item.container.as_str();
        let player_variant = if container.starts_with("image/") {
            // TODO: use gst-plugin-gif::gifdec for GIFs
            UiPlayerVariant::Image
        } else if container.starts_with("audio/") {
            UiPlayerVariant::Audio
        } else if container.starts_with("video/")
            || container == "application/x-whep"
            || container == "application/dash+xml"
            || container == "application/vnd.apple.mpegurl"
            || container == "audio/mpegurl"
        {
            // Video streams are audio only until proven otherwise
            UiPlayerVariant::Audio
            // UiPlayerVariant::Video
        } else {
            UiPlayerVariant::Unknown
        };

        let mut media_title = None;
        if let Some(metadata) = media_item.metadata.as_ref() {
            match metadata {
                v3::MetadataObject::Generic {
                    thumbnail_url: Some(thumbnail_url),
                    title,
                    ..
                } => {
                    media_title = title.as_ref().map(|title| title.to_shared_string());
                    self.have_audio_track_cover = true;
                    let event_tx = self.event_tx.clone();
                    self.current_image_download_id += 1;
                    let this_id = self.current_image_download_id;
                    let url = thumbnail_url.clone();
                    self.pending_thumbnail_download = Some(this_id);
                    let client = self.http_client.clone();
                    let headers = self.current_request_headers.lock().clone();
                    tokio::spawn(async move {
                        let res = Self::download_image(&client, &url, headers)
                            .instrument(debug_span!("download_image", this_id))
                            .await;
                        let _ = event_tx.send(Event::ImageDownloadResult { id: this_id, res });
                    });
                }
                _ => (),
            }
        }

        *self.current_request_headers.lock() = media_item.headers.clone();

        if container.starts_with("image/") {
            self.player.stop();

            let event_tx = self.event_tx.clone();
            self.current_image_download_id += 1;
            let id = self.current_image_download_id;
            let client = self.http_client.clone();
            let headers = self.current_request_headers.lock().clone();
            tokio::spawn(async move {
                let res = Self::download_image(&client, &url, headers)
                    .instrument(debug_span!("download_image", id))
                    .await;
                let _ = event_tx.send(Event::ImageDownloadResult { id, res });
            });
        } else {
            self.player.set_uri(Some(&url));
            if let Some(volume) = media_item.volume {
                self.on_uri_loaded_command_queue
                    .push(OnUriLoadedCommand::Volume(volume));
            }

            if let Some(rate) = media_item.speed {
                self.on_playing_command_queue
                    .push(OnFirstPlayingStateChangedCommand::Rate(rate));
            }
            if !is_for_sure_live
                && let Some(time) = media_item.time
                // Don't seek if we don't need to
                && time >= 1.0
            {
                self.on_playing_command_queue
                    // .push(OnUriLoadedCommand::Seek(time));
                    .push(OnFirstPlayingStateChangedCommand::Seek(time));
            }
        }

        self.have_media_title = media_title.is_some();

        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_player_variant(player_variant);
            // TODO: I think we should wait at least 500ms before setting this.
            //       Reasoning: if e.g. images take some time to decode we might flash the screen for no reason...
            bridge.set_app_state(AppState::LoadingMedia);
            if let Some(title) = media_title {
                bridge.set_media_title(title);
            }
        })?;

        self.video_sink_is_eos
            .store(true, atomic::Ordering::Relaxed);

        // TODO: v3::EventType::MediaItemChange?

        Ok(())
    }

    fn handle_playlist_play_request(&mut self, play_message: &v3::PlayMessage) -> Result<()> {
        // TODO: load the json from URL
        let playlist = serde_json::from_str::<v3::PlaylistContent>(
            play_message
                .content
                .as_ref()
                .ok_or(anyhow::anyhow!("No playlist found in content field"))?,
        )?;

        // TODO: play the actual playlist

        let start_idx = match playlist.offset {
            Some(idx) => idx as usize,
            None => 0,
        };

        let start_item = playlist.items.get(start_idx).unwrap(/* TODO: */);

        self.load_media_item(start_item)?; // TODO: how should errors be handled?

        // TODO: handle show duration for the item

        self.current_playlist = Some(playlist);
        self.current_playlist_item_idx = Some(start_idx);

        Ok(())
    }

    fn handle_simple_play_request(&mut self, play_message: &v3::PlayMessage) -> Result<()> {
        self.load_media_item(&Self::play_message_to_media_item(play_message.clone()))
    }

    fn video_stream_available(&self) -> Result<()> {
        debug!("Video stream available");

        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_player_variant(UiPlayerVariant::Video);
        })?;

        Ok(())
    }

    async fn handle_player_event(&mut self, event: gst::Message) -> Result<()> {
        let Ok(play_message) = gst_play::PlayMessage::parse(&event) else {
            return Ok(());
        };

        match play_message {
            gst_play::PlayMessage::UriLoaded(loaded) => {
                debug!(uri = %loaded.uri(), "URI loaded");
                debug!("Commands: {:?}", self.on_playing_command_queue);
                // TODO: Some streams are not happy about setting theese things so early, should we wait for first playing state change?
                // for command in self.on_playing_command_queue.iter() {
                for command in self.on_uri_loaded_command_queue.iter() {
                    match command {
                        OnUriLoadedCommand::Volume(volume) => {
                            self.player.set_volume(*volume);
                        }
                    }
                }

                // TODO: can this be done here 100% of the time?
                while let Some(command) = self.on_playing_command_queue.pop() {
                    match command {
                        OnFirstPlayingStateChangedCommand::Seek(time) => {
                            self.player.seek(gst::ClockTime::from_seconds_f64(time))
                        }
                        OnFirstPlayingStateChangedCommand::Rate(rate) => self.player.set_rate(rate),
                    }
                }

                self.player.play();
            }
            gst_play::PlayMessage::PositionUpdated(position_updated) => {
                if self.player_state != gst_play::PlayState::Stopped
                    && self.player_state != gst_play::PlayState::Paused
                    && self.player_state != gst_play::PlayState::Buffering
                    && let Some(new_position) =
                        position_updated.position().map(|pos| pos.seconds_f64())
                    && (self.last_position_updated - new_position).abs() >= 0.75
                {
                    self.notify_updates(false)?;
                }
                // TODO: check if it's been a second since last update (stream time, not wall clock)?
            }
            gst_play::PlayMessage::DurationChanged(duration_changed) => {
                self.current_duration = duration_changed.duration();
            }
            gst_play::PlayMessage::StateChanged(state_change) => {
                // TODO: should start events be sent when the first Playing state change has happened?

                // TODO: is this robust enough? or should we wait fro the Stopped state?
                if self.current_media.is_none() {
                    debug!(?state_change, "Ignoring old player state change");
                    return Ok(());
                }

                self.player_state = state_change.state();
                match self.player_state {
                    // gst_play::PlayState::Stopped => todo!(),
                    // gst_play::PlayState::Buffering => todo!(),
                    gst_play::PlayState::Paused | gst_play::PlayState::Playing => {
                        self.ui_weak.upgrade_in_event_loop(|ui| {
                            ui.invoke_playback_started();
                            ui.global::<Bridge>().set_app_state(AppState::Playing);
                        })?;
                        self.notify_updates(true)
                            .context("Failed to notify about updates")?;

                        if self.player_state == gst_play::PlayState::Playing
                            && !self.on_playing_command_queue.is_empty()
                        {
                            debug!(?self.on_playing_command_queue, "Updating player");
                            // while let Some(command) = self.on_playing_command_queue.pop() {
                            //     match command {
                            //         OnFirstPlayingStateChangedCommand::Seek(time) => self
                            //             .player
                            //             .seek(gst::ClockTime::from_seconds_f64(time)),
                            //         OnFirstPlayingStateChangedCommand::Rate(rate) => {
                            //             self.player.set_rate(rate)
                            //         }
                            //     }
                            // }
                            // self.on_playing_command_queue.clear();
                        }

                        // if state == gst_play::PlayState::Playing {
                        //     while let Some(command) = self.on_playing_command_queue.pop() {
                        //         match command {
                        //         }
                        //     }
                        // }
                    }
                    gst_play::PlayState::Stopped => {
                        // TODO: reset playback info, time, duration, etc.
                        self.notify_updates(true)
                            .context("Failed to notify about updates")?;
                    }
                    _ => (),
                }
            }
            gst_play::PlayMessage::Buffering(_buffering) => (),
            gst_play::PlayMessage::EndOfStream(_) => {
                debug!("Player reached EOS");

                self.media_ended();

                // TODO: this should be the last message sent regarding the media currently being played
                if self.updates_tx.receiver_count() > 0
                    && let Some(play_data) = self.current_play_data.as_ref()
                {
                    let event = v3::EventMessage {
                        generation_time: current_time_millis(),
                        event: v3::EventObject::MediaItem {
                            variant: v3::EventType::MediaItemEnd,
                            item: v3::MediaItem {
                                container: play_data.container.clone(),
                                url: play_data.url.clone(),
                                content: play_data.content.clone(),
                                time: play_data.time,
                                volume: play_data.volume,
                                speed: play_data.speed,
                                cache: None,
                                show_duration: None,
                                headers: None,
                                metadata: play_data.metadata.clone(),
                            },
                        },
                    };
                    self.updates_tx
                        .send(Arc::new(ReceiverToSenderMessage::Event { msg: event }))?;
                }
            }
            gst_play::PlayMessage::Error(_error) => (),
            gst_play::PlayMessage::Warning(_warning) => (),
            gst_play::PlayMessage::MediaInfoUpdated(media_info_updated) => {
                let info = media_info_updated.media_info();

                // TODO: we should read these always because they might be updated
                {
                    // for stream in info.video_streams() {
                    // if let Some(tags) = stream.tags() {
                    // debug!("Video stream: {}x{}, {:?} {:?}", stream.width(), stream.height(), tags.get::<gst::tags::VideoCodec>(), tags.get::<gst::tags::Codec>());
                    // if let Some(title) = tags.get::<gst::tags::Title>() {
                    //     debug!(?title, "Video stream");
                    // }
                    // }
                    // }
                }

                if !self.have_media_info && info.number_of_streams() > 0 {
                    self.media_loaded_successfully(); // TODO: is this the best place to put this?

                    // TODO: MediaItemChange

                    self.current_duration = info.duration();
                    if info.number_of_video_streams() > 0 {
                        self.video_stream_available()?;
                    }

                    if info.number_of_video_streams() == 0
                        && let Some(track) = self.player.current_audio_track()
                        && let Some(tags) = track.tags()
                    {
                        if !self.have_audio_track_cover
                            && let Some(cover) = tags.get::<gst::tags::Image>()
                            && let Some(buffer) = cover.get().buffer()
                            && let Ok(buffer) = buffer.map_readable()
                        {
                            self.current_thumbnail_id += 1;
                            let this_id = self.current_thumbnail_id;
                            self.image_decode_tx.send((
                                this_id,
                                ImageDecodeJob {
                                    image: EncodedImageData::Vec(buffer.to_vec()),
                                    format: None,
                                    typ: ImageDecodeJobType::AudioThumbnail,
                                },
                            ))?;
                            self.pending_thumbnail = Some(this_id);
                        }

                        if !self.have_media_title
                            && let Some(title) = tags.get::<gst::tags::Title>()
                        {
                            let title = title.get().to_shared_string();
                            self.ui_weak.upgrade_in_event_loop(move |ui| {
                                ui.global::<Bridge>().set_media_title(title);
                            })?;
                            self.have_media_title = true;
                        }

                        if let Some(artist) = tags.get::<gst::tags::Artist>() {
                            let artist = artist.get().to_shared_string();
                            self.ui_weak.upgrade_in_event_loop(move |ui| {
                                ui.global::<Bridge>().set_artist_name(artist);
                            })?;
                        }
                    }

                    self.have_media_info = true;
                }

                fn bitrate_to_string(bitrate: i32) -> String {
                    if bitrate > 1_000_000 {
                        format!("{:.2} Mbps", bitrate as f32 / 1_000_000.0)
                    } else if bitrate > 1_000 {
                        format!("{:.2} Kbps", bitrate as f32 / 1_000.0)
                    } else {
                        format!("{} bps", bitrate)
                    }
                }

                fn play_info_to_stream_dbg(play_info: &impl PlayStreamInfoExt) -> UiStreamDbg {
                    UiStreamDbg {
                        id: play_info.stream_id().to_shared_string(),
                        codec: play_info.codec().unwrap_or("n/a".into()).to_shared_string(),
                    }
                }

                // if self.debug_mode && !self.has_media_info  {
                if !self.have_media_info {
                    let audio_streams: Vec<UiAudioStreamDbg> = info
                        .audio_streams()
                        .into_iter()
                        .map(|stream| UiAudioStreamDbg {
                            info: play_info_to_stream_dbg(&stream),
                            bitrate: bitrate_to_string(stream.bitrate()).to_shared_string(),
                            max_bitrate: bitrate_to_string(stream.max_bitrate()).to_shared_string(),
                            channels: stream.channels(),
                            language: stream.language().unwrap_or("n/a".into()).to_shared_string(),
                            sample_rate: stream.sample_rate(),
                        })
                        .collect();
                    let video_streams: Vec<UiVideoStreamDbg> = info
                        .video_streams()
                        .into_iter()
                        .map(|stream| UiVideoStreamDbg {
                            info: play_info_to_stream_dbg(&stream),
                            bitrate: bitrate_to_string(stream.bitrate()).to_shared_string(),
                            max_bitrate: bitrate_to_string(stream.max_bitrate()).to_shared_string(),
                            width: stream.width(),
                            height: stream.height(),
                            framerate_n: stream.framerate().numer(),
                            framerate_d: stream.framerate().denom(),
                        })
                        .collect();
                    let subtitle_streams: Vec<UiSubtitleStreamDbg> = info
                        .subtitle_streams()
                        .into_iter()
                        .map(|stream| UiSubtitleStreamDbg {
                            info: play_info_to_stream_dbg(&stream),
                            language: stream.language().unwrap_or("n/a".into()).to_shared_string(),
                        })
                        .collect();

                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        let bridge = ui.global::<Bridge>();
                        bridge.set_audio_dbg(Rc::new(VecModel::from(audio_streams)).into());
                        bridge.set_video_dbg(Rc::new(VecModel::from(video_streams)).into());
                        bridge.set_subtitle_dbg(Rc::new(VecModel::from(subtitle_streams)).into());
                    })?;
                }

                self.current_media = Some(info.to_owned());
            }
            gst_play::PlayMessage::VolumeChanged(volume_changed) => {
                let volume = volume_changed.volume();
                // info!(volume, "Volume changed");

                self.ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.global::<Bridge>().set_volume(volume as f32);
                })?;
                if self.updates_tx.receiver_count() > 0 {
                    let update = VolumeUpdateMessage {
                        generation_time: current_time_millis(),
                        volume,
                    };

                    // debug!("Sending update ({update:?})");

                    let msg = ReceiverToSenderMessage::Translatable {
                        op: Opcode::VolumeUpdate,
                        msg: TranslatableMessage::VolumeUpdate(update),
                    };
                    self.updates_tx
                        // .send(Arc::new(Packet::from(update).encode()?))?;
                        .send(Arc::new(msg))?;
                    self.last_sent_update = Instant::now();
                }

                self.volume_lock.release();
            }
            gst_play::PlayMessage::SeekDone(_) => {
                self.seek_lock.release();
                self.notify_updates(true)
                    .context("Failed to notify about updates")?;
            }
            _ => (),
        }

        Ok(())
    }

    fn handle_operation(&mut self, op: Operation) -> Result<bool> {
        match op {
            Operation::Pause => {
                self.player.pause();
            }
            Operation::Resume => {
                self.player.play();
            }
            Operation::Stop => {
                // TODO: handle this case correctly:
                // DEBUG rcore: Operation from sender id=0 op=Stop
                // DEBUG rcore: Player event event=StateChanged(Playing)
                // DEBUG rcore: Player event event=StateChanged(Stopped)
                //              * gui is in playing mode and screen is black (eos) *

                self.player.stop();
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>().set_app_state(AppState::Idle);
                })?;
                self.cleanup_playback_data()?;
                // TODO: notify update? or wait for async state change from player
            }
            Operation::Play(play_message) => {
                self.cleanup_playback_data()?;

                if play_message.container == "application/json" {
                    self.handle_playlist_play_request(&play_message)?;
                } else {
                    self.handle_simple_play_request(&play_message)?;
                }

                if self.updates_tx.receiver_count() > 0 {
                    let play_update = v3::PlayUpdateMessage {
                        generation_time: Some(current_time_millis()),
                        play_data: Some(play_message.clone()),
                    };
                    let msg = ReceiverToSenderMessage::PlayUpdate { msg: play_update };
                    let _ = self.updates_tx.send(Arc::new(msg));
                }

                self.current_play_data = Some(play_message);
            }
            Operation::Seek(seek_message) => {
                let time = seek_message.time;
                if !self.seek_lock.is_locked() && time >= 0.0 && time.is_normal() {
                    self.seek_lock.acquire();
                    self.player.seek(gst::ClockTime::from_seconds_f64(time));
                }
            }
            Operation::SetSpeed(set_speed_message) => {
                self.player.set_rate(set_speed_message.speed);
            }
            Operation::SetPlaylistItem(msg) => {
                debug!(?msg, "Set playlist item");
                if let Some(ref playlist) = self.current_playlist {
                    let new_index = msg.item_index as usize;
                    if let Some(item) = playlist.items.get(new_index) {
                        self.load_media_item(&item.clone())?;
                    } else {
                        todo!("Playlist item not found");
                    }

                    self.current_playlist_item_idx = Some(new_index);
                }
                {
                    error!("Cannot set playlist item when no playlist is loaded");
                }
            }
            Operation::SetVolume(set_volume_msg) => {
                if !self.volume_lock.is_locked() {
                    self.volume_lock.acquire();
                    self.player.set_volume(set_volume_msg.volume);
                }
            }
        }

        Ok(false)
    }

    fn handle_mdns_event(&mut self, event: MdnsEvent) -> Result<()> {
        match event {
            MdnsEvent::NameSet(device_name) => {
                let device_name_shared = device_name.to_shared_string();
                self.device_name = Some(device_name);
                self.ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.global::<Bridge>().set_device_name(device_name_shared);
                })?;
            }
            MdnsEvent::IpAdded(addr) => {
                let _ = self.current_addresses.insert(addr);
            }
            MdnsEvent::IpRemoved(addr) => {
                let _ = self.current_addresses.remove(&addr);
            }
            MdnsEvent::SetIps(addrs) => {
                self.current_addresses.clear();
                for addr in addrs {
                    let _ = self.current_addresses.insert(addr);
                }
            }
        }

        let addrs = self
            .current_addresses
            .iter()
            .filter(|addr| {
                !addr.is_loopback() && {
                    match *addr {
                        IpAddr::V4(_) => true,
                        IpAddr::V6(v6) => !v6.is_unicast_link_local(),
                    }
                }
            })
            .map(|addr| addr.to_string())
            .collect::<SmallVec<[String; 5]>>();

        if addrs.is_empty() {
            // TODO: Reset QR
        } else if let Some(device_name) = self.device_name.clone() {
            let ips_string = addrs.join(", ");
            let net_config = fcast_protocol::FCastNetworkConfig {
                name: device_name,
                addresses: addrs.to_vec(),
                services: vec![fcast_protocol::FCastService {
                    port: 46899,
                    r#type: 0,
                }],
            };
            debug!(?net_config, "Network config for QR code created");
            let net_config = serde_json::to_string(&net_config)?;
            let device_url = format!(
                "fcast://r/{}",
                base64::engine::general_purpose::URL_SAFE
                    .encode(net_config)
                    .as_str(),
            );

            let qrcode = fast_qr::QRBuilder::new(device_url.as_bytes()).build()?;
            // use fast_qr::convert::Builder;
            // let qr_svg = fast_qr::convert::svg::SvgBuilder::default()
            //     .shape(fast_qr::convert::Shape::Circle)
            //     .module_color(fast_qr::convert::Color::from([0x00, 0x00, 0x00, 0xFF]))
            //     .background_color(fast_qr::convert::Color::from([0x00, 0x00, 0x00, 0x00]))
            //     .margin(1)
            //     .to_str(&qrcode);

            let dims = qrcode.size as u32;
            let mut pixbuf: slint::SharedPixelBuffer<slint::Rgb8Pixel> =
                slint::SharedPixelBuffer::new(dims, dims);
            let pixbuf_pixels = pixbuf.make_mut_slice();
            for (idx, module) in qrcode.data[0..pixbuf_pixels.len()].iter().enumerate() {
                if *module == fast_qr::Module::LIGHT {
                    pixbuf_pixels[idx] = slint::Rgb8Pixel::new(0xFF, 0xFF, 0xFF);
                } else {
                    pixbuf_pixels[idx] = slint::Rgb8Pixel::new(0x00, 0x00, 0x00);
                }
            }

            self.ui_weak.upgrade_in_event_loop(move |ui| {
                let bridge = ui.global::<Bridge>();
                bridge.set_qr_code(slint::Image::from_rgb8(pixbuf));
                bridge.set_local_ip_addrs(ips_string.to_shared_string());

                // if let Ok(qr) = slint::Image::load_from_svg_data(qr_svg.as_bytes()) {
                //     ui.global::<Bridge>().set_qr_code(qr);
                // }
            })?;
        }

        Ok(())
    }

    /// Returns `true` if the event loop should exit
    async fn handle_event(&mut self, event: Event) -> Result<bool> {
        // NOTE: all player actions are async (right?)
        match event {
            Event::SessionFinished => {
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.global::<Bridge>().invoke_device_disconnected();
                })?;
            }
            Event::ResumeOrPause => {
                let op = match self.player_state {
                    gst_play::PlayState::Paused => Operation::Resume,
                    gst_play::PlayState::Playing => Operation::Pause,
                    _ => {
                        error!(
                            "Cannot resume or pause in player current state: {:?}",
                            self.player_state
                        );
                        return Ok(false);
                    }
                };

                return self.handle_operation(op);
            }
            Event::SeekPercent(percent) => {
                debug!("SeekPercent({percent})");
                if let Some(duration) = self.current_duration {
                    let seconds = percent as f64 * duration.seconds_f64();
                    return self.handle_operation(Operation::Seek(fcast_protocol::SeekMessage {
                        time: seconds,
                    }));
                }
            }
            Event::Quit => return Ok(true),
            Event::ToggleDebug => self.debug_mode = !self.debug_mode,
            Event::Player(event) => self.handle_player_event(event).await?,
            Event::Op { session_id: id, op } => {
                debug!(id, ?op, "Operation from sender");
                return self.handle_operation(op);
            }
            Event::ImageDownloadResult { id, res } => {
                debug!(id, "Got image download result");

                if Some(id) == self.pending_thumbnail_download {
                    // Somewhere it goes wrong decoding?
                    match res {
                        Ok((encoded_image, format)) => {
                            self.pending_thumbnail_download = None;
                            self.current_thumbnail_id += 1;
                            let this_id = self.current_thumbnail_id;
                            self.pending_thumbnail = Some(this_id);
                            self.image_decode_tx.send((
                                this_id,
                                ImageDecodeJob {
                                    image: EncodedImageData::Bytes(encoded_image),
                                    format: Some(format),
                                    typ: ImageDecodeJobType::AudioThumbnail,
                                },
                            ))?;
                        }
                        Err(err) => {
                            error!(%err, "Thumbnail image download failed");
                        }
                    }
                    return Ok(false);
                }

                if id != self.current_image_download_id {
                    warn!(id, "Ignoring old image download result");
                    return Ok(false);
                }

                match res {
                    Ok((encoded_image, format)) => {
                        self.current_image_id += 1;
                        let this_id = self.current_image_id;
                        self.image_decode_tx.send((
                            this_id,
                            ImageDecodeJob {
                                image: EncodedImageData::Bytes(encoded_image),
                                format: Some(format),
                                typ: ImageDecodeJobType::Regular,
                            },
                        ))?;
                    }
                    Err(err) => {
                        error!(%err, "Image download failed");
                    }
                }
            }
            Event::AudioThumbnailAvailable { id, preview } => {
                if let Some(pending_thumbnail) = self.pending_thumbnail
                    && pending_thumbnail == id
                {
                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        let bridge = ui.global::<Bridge>();
                        bridge.set_audio_track_cover(slint::Image::from_rgba8(preview));
                    })?;
                }
            }
            Event::AudioThumbnailBlurAvailable { id, blured } => {
                if let Some(pending_thumbnail) = self.pending_thumbnail
                    && pending_thumbnail == id
                {
                    // NOTE: `AudioThumbnailBlurAvailable` is assumed to *always* be received after `AudioThumbnailAvailable`
                    //       and no other thumbnail results in between.
                    self.pending_thumbnail = None;
                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        let bridge = ui.global::<Bridge>();
                        bridge.set_blured_audio_track_cover(slint::Image::from_rgba8(blured));
                    })?;
                }
            }
            Event::ImageDecoded { id, image } => {
                if id != self.current_image_id {
                    warn!(id, "Ignoring old image decode result");
                    return Ok(false);
                }

                self.ui_weak.upgrade_in_event_loop(move |ui| {
                    let image = slint::Image::from_rgba8(image);
                    let bridge = ui.global::<Bridge>();
                    bridge.set_image_preview(image);
                    bridge.set_app_state(AppState::Playing)
                })?;

                self.media_loaded_successfully();
            }
            Event::Mdns(event) => {
                debug!(?event, "mDNS event");
                self.handle_mdns_event(event)?;
            }
        }

        Ok(false)
    }

    pub async fn run_event_loop(
        mut self,
        mut event_rx: UnboundedReceiver<Event>,
        fin_tx: oneshot::Sender<()>,
    ) -> Result<()> {
        // TODO: IPv4 on windows
        let dispatch_listener =
            TcpListener::bind(SocketAddr::new(IpAddr::V6(Ipv6Addr::UNSPECIFIED), 46899)).await?;

        let mut session_id: SessionId = 0;
        // let mut update_interval = tokio::time::interval(Duration::from_millis(250));

        // let event_tx_cl = self.event_tx.clone();

        loop {
            tokio::select! {
                event = event_rx.recv() => {
                    if let Some(event) = event {
                        match self.handle_event(event).await {
                            Ok(true) => break,
                            Err(err) => error!("Handle event error: {err}"),
                            _ => (),
                        }
                    } else {
                        break;
                    }
                }
                // _ = update_interval.tick() => {
                    // TODO: in what states can we omit updates?
                    // if self.player_state != gst_play::PlayState::Stopped
                    //     && self.player_state != gst_play::PlayState::Paused
                    //     && self.player_state != gst_play::PlayState::Buffering {
                    //     self.notify_updates(false)?;
                    // }
                // }
                session = dispatch_listener.accept() => {
                    let (stream, _) = session?;

                    debug!("New connection id={session_id}");

                    tokio::spawn({
                        let id = session_id;
                        let event_tx = self.event_tx.clone();
                        let updates_rx = self.updates_tx.subscribe();
                        async move {
                            if let Err(err) =
                                SessionDriver::new(stream, id)
                                .run(updates_rx, &event_tx)
                                .instrument(tracing::debug_span!("session", id))
                                .await
                            {
                                error!("Session exited with error: {err}");
                            }

                            if let Err(err) = event_tx.send(Event::SessionFinished) {
                                error!("Failed to send SessionFinished: {err}");
                            }
                        }
                    });

                    self.ui_weak.upgrade_in_event_loop(move |ui| {
                        ui.invoke_device_connected();
                    })?;

                    session_id += 1;
                }
            }
        }

        self.player.stop();

        debug!("Quitting");

        if fin_tx.send(()).is_err() {
            bail!("Failed to send fin");
        }

        #[cfg(not(target_os = "android"))]
        {
            'outer: loop {
                let shutdown_rx = self.mdns.shutdown();
                match shutdown_rx {
                    Ok(rx) => loop {
                        match rx.recv_async().await {
                            Ok(status) => {
                                if status == mdns_sd::DaemonStatus::Shutdown {
                                    debug!("mDNS daemon shutdown");
                                    break 'outer;
                                }
                            }
                            Err(err) => {
                                error!(?err, "Failed to shutdown mDNS daemon");
                                break 'outer;
                            }
                        }
                    },
                    Err(mdns_sd::Error::Again) => continue,
                    Err(_) => break,
                }
            }
        }

        Ok(())
    }
}

#[derive(clap::Parser)]
#[command(version)]
struct CliArgs {
    // Disable animated background. Reduces resource usage
    // #[arg(short = 'b', long, default_value_t = false)]
    // no_background: bool,
}

fn log_level() -> LevelFilter {
    match std::env::var("FCAST_LOG") {
        Ok(level) => match level.to_ascii_lowercase().as_str() {
            "error" => LevelFilter::ERROR,
            "warn" => LevelFilter::WARN,
            "info" => LevelFilter::INFO,
            "debug" => LevelFilter::DEBUG,
            "trace" => LevelFilter::TRACE,
            _ => LevelFilter::OFF,
        },
        #[cfg(debug_assertions)]
        Err(_) => LevelFilter::DEBUG,
        #[cfg(not(debug_assertions))]
        Err(_) => LevelFilter::OFF,
    }
}

/// Run the main app.
///
/// Slint and friends are assumed to be initialized by the platform specific target.
pub fn run(
    #[cfg(target_os = "android")] android_app: slint::android::AndroidApp,
    #[cfg(target_os = "android")] mut platform_event_rx: UnboundedReceiver<Event>,
) -> Result<()> {
    let prev_panic_hook = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |panic_info| {
        tracing_panic::panic_hook(panic_info);
        prev_panic_hook(panic_info);
    }));
    tracing_gstreamer::integrate_events();
    gst::log::remove_default_log_function();

    #[cfg(not(target_os = "android"))]
    {
        use tracing_subscriber::{Layer, layer::SubscriberExt, util::SubscriberInitExt};
        let fmt_layer = tracing_subscriber::fmt::layer().with_filter(log_level());
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        tracing_subscriber::registry().with(fmt_layer).init();
    }

    #[cfg(target_os = "android")]
    {
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        gst::log::set_threshold_for_name("gldebug", gst::DebugLevel::None);
    }

    let start = std::time::Instant::now();

    gst::init()?;

    let runtime = tokio::runtime::Runtime::new().unwrap();

    #[cfg(any(target_os = "linux", target_os = "android"))]
    if let Err(err) = rustls::crypto::ring::default_provider().install_default() {
        error!(
            ?err,
            "Failed to register ring as rustls default crypto provider"
        );
    }

    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
    let (fin_tx, fin_rx) = oneshot::channel::<()>();

    #[cfg(target_os = "android")]
    runtime.spawn({
        let event_tx = event_tx.clone();
        async move {
            while let Some(event) = platform_event_rx.recv().await {
                if event_tx.send(event).is_err() {
                    break;
                }
            }

            debug!("Platform event proxy finished");
        }
    });

    let mut slint_sink = video::SlintOpenGLSink::new()?;
    let slint_appsink = slint_sink.video_sink();

    let ui = MainWindow::new()?;

    // TODO: use AndroidApp set window flag with KEEP_SCREEN_ON when viewing media

    #[cfg(debug_assertions)]
    ui.global::<Bridge>().set_is_debugging(true);

    let video_sink_is_eos = Arc::clone(&slint_sink.is_eos);
    ui.window().set_rendering_notifier({
        let ui_weak = ui.as_weak();

        move |state, graphics_api| {
            if let slint::RenderingState::RenderingSetup = state {
                debug!("Got graphics API: {graphics_api:?}");
                let ui_weak = ui_weak.clone();

                slint_sink
                    .connect(graphics_api, move || {
                        ui_weak
                            .upgrade_in_event_loop(move |ui| {
                                ui.window().request_redraw();
                            })
                            .unwrap();
                    })
                    .unwrap();
            } else if let slint::RenderingState::BeforeRendering = state {
                let Some(ui) = ui_weak.upgrade() else {
                    error!("Failed to upgrade ui");
                    return;
                };

                let bridge = ui.global::<Bridge>();
                if bridge.get_playing() {
                    let frame = if let Some((texture_id, size)) =
                        slint_sink.fetch_next_frame_as_texture()
                    {
                        unsafe {
                            slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                                texture_id,
                                size.into(),
                            )
                            .build()
                        }
                    } else {
                        slint::Image::default()
                    };

                    bridge.set_video_frame(frame);

                    let overlays = slint_sink.fetch_next_overlays();
                    if let Some(overlays) = overlays {
                        let overlays: Option<VecModel<UiSubOverlay>> = overlays.map(|overlays| {
                            overlays
                                .into_iter()
                                .map(|overlay| UiSubOverlay {
                                    img: slint::Image::from_rgba8(overlay.pix_buffer),
                                    x: overlay.x as f32,
                                    y: overlay.y as f32,
                                })
                                .collect()
                        });
                        if let Some(overlays) = overlays {
                            bridge.set_overlays(Rc::new(overlays).into());
                        }
                    } else {
                        bridge.set_overlays(slint::ModelRc::default());
                    }
                }
            }
        }
    })?;

    runtime.spawn({
        let ui_weak = ui.as_weak();
        let event_tx = event_tx.clone();
        async move {
            fcastwhepsrcbin::plugin_init().unwrap();
            gstreqwest::plugin_register_static().unwrap();

            Application::new(slint_appsink, event_tx, ui_weak, video_sink_is_eos)
                .await
                .unwrap()
                .run_event_loop(event_rx, fin_tx)
                .await
                .unwrap();
        }
    });

    ui.global::<Bridge>().on_resume_or_pause({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.send(Event::ResumeOrPause));
        }
    });

    ui.global::<Bridge>().on_seek_to_percent({
        let event_tx = event_tx.clone();
        move |percent| {
            log_if_err!(event_tx.send(Event::SeekPercent(percent)));
        }
    });

    ui.global::<Bridge>().on_toggle_fullscreen({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak
                .upgrade()
                .expect("callbacks always get called from the event loop");
            let is_fullscreen = !ui.window().is_fullscreen();
            ui.window().set_fullscreen(is_fullscreen);
            ui.global::<Bridge>().set_is_fullscreen(is_fullscreen);
        }
    });

    ui.global::<Bridge>().on_set_volume({
        let event_tx = event_tx.clone();
        move |volume| {
            log_if_err!(event_tx.send(Event::Op {
                session_id: 0,
                op: Operation::SetVolume(SetVolumeMessage {
                    volume: volume as f64,
                })
            }));
        }
    });

    ui.global::<Bridge>().on_force_quit(move || {
        log_if_err!(slint::quit_event_loop());
    });

    ui.global::<Bridge>().on_debug_toggled({
        let event_tx = event_tx.clone();
        move || {
            log_if_err!(event_tx.send(Event::ToggleDebug));
        }
    });

    // ui.global::<Bridge>().set_label(format!("{ips:?}").into());

    info!(finished_in = ?start.elapsed());

    ui.run()?;

    debug!("Shutting down...");

    runtime.block_on(async move {
        event_tx.send(Event::Quit).unwrap();
        fin_rx.await.unwrap();
    });

    Ok(())
}
