#![feature(ip)]

use anyhow::{Result, bail};
use base64::Engine;
use bytes::Bytes;
use fcast_protocol::{
    Opcode, PlaybackErrorMessage, PlaybackState, SetVolumeMessage, v2::VolumeUpdateMessage, v3,
};
use gst::prelude::*;
use image::ImageFormat;
use parking_lot::Mutex;
use session::{SessionDriver, SessionId};
#[cfg(target_os = "android")]
use slint::android::android_activity::WindowManagerFlags;
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

#[cfg(not(target_os = "android"))]
pub use clap;
pub use slint;
pub use tracing;
mod fcastwhepsrcbin;
mod player;
mod session;
// mod small_vec_model; // For later
#[cfg(target_os = "linux")]
mod linux_tray;
#[cfg(not(any(target_os = "android", target_os = "linux")))]
mod mac_win_tray;
mod user_agent;
mod video;

use crate::{
    player::PlayerState,
    session::{Operation, ReceiverToSenderMessage, TranslatableMessage},
};

#[derive(Debug, thiserror::Error)]
pub enum DownloadImageError {
    #[error("request failed: {0:?}")]
    RequestFailed(#[from] reqwest::Error),
    #[error("response is missing content type")]
    MissingContentType,
    #[error("response has invalid content type")]
    InvalidContentType,
    #[error("content type is not a string")]
    ContentTypeIsNotString,
    #[error("content type ({0}) is unsupported")]
    UnsupportedContentType(String),
    #[error("failed to decode image: {0:?}")]
    DecodeImage(#[from] image::ImageError),
    #[error("failed to parse URL: {0:?}")]
    InvalidUrl(#[from] url::ParseError),
    #[error("unsuccessful status={0}")]
    Unsuccessful(reqwest::StatusCode),
}

type SlintRgba8Pixbuf = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

#[derive(Debug)]
pub enum MdnsEvent {
    NameSet(String),
    IpAdded(IpAddr),
    IpRemoved(IpAddr),
    SetIps(Vec<IpAddr>),
}

type MediaItemId = u64;

#[cfg(not(target_os = "android"))]
#[derive(Debug)]
pub enum TrayEvent {
    Quit,
    Toggle,
}

#[derive(Debug)]
pub enum Event {
    Quit,
    SessionFinished,
    ResumeOrPause,
    SeekPercent(f32),
    ToggleDebug,
    // Player(gst::Message),
    NewPlayerEvent(player::PlayerEvent),
    Op {
        /// The UI also sends operations with session_id == 0
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
    PlaylistDataResult {
        play_message: Option<v3::PlayMessage>,
    },
    MediaItemFinish(MediaItemId),
    SelectTrack {
        // id: usize,
        id: i32,
        variant: UiMediaTrackType,
    },
    #[cfg(not(target_os = "android"))]
    Tray(TrayEvent),
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
// TODO: rename and merge with OnUriLoadedCommand
enum OnFirstPlayingStateChangedCommand {
    Seek(f64),
    Rate(f64),
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
            Self::Bytes(bytes) => bytes,
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
    let span = debug_span!("image-decoder");
    let _entered = span.enter();

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

fn sec_to_string(sec: f64) -> String {
    let time_secs = sec % 60.0;
    let time_mins = (sec / 60.0) % 60.0;
    let time_hours = sec / 60.0 / 60.0;

    format!(
        "{:02}:{:02}:{:02}",
        time_hours as u32, time_mins as u32, time_secs as u32,
    )
}

// struct PlaylistPlaybackState {
//     playlist: v3::PlaylistContent,
//     current_item_idx: usize,
// }

// TODO: store either single item or playlist etc.

struct Application {
    #[cfg(target_os = "android")]
    android_app: slint::android::AndroidApp,
    event_tx: UnboundedSender<Event>,
    ui_weak: slint::Weak<MainWindow>,
    updates_tx: broadcast::Sender<Arc<ReceiverToSenderMessage>>,
    #[cfg(not(target_os = "android"))]
    mdns: mdns_sd::ServiceDaemon,
    last_sent_update: Instant,
    debug_mode: bool,
    player: player::Player,
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
    last_position_updated: f64,
    http_client: reqwest::Client,
    current_request_headers: Arc<Mutex<Option<HashMap<String, String>>>>,
    current_playlist: Option<v3::PlaylistContent>,
    current_playlist_item_idx: Option<usize>,
    device_name: Option<String>,
    current_media_item_id: MediaItemId,
}

impl Application {
    pub async fn new(
        appsink: gst::Element,
        event_tx: UnboundedSender<Event>,
        ui_weak: slint::Weak<MainWindow>,
        video_sink_is_eos: Arc<AtomicBool>,
        #[cfg(target_os = "android")] android_app: slint::android::AndroidApp,
        // contexts: std::sync::Arc<std::sync::Mutex<Option<(gst_gl::GLDisplay, gst_gl::GLContext)>>>,
    ) -> Result<Self> {
        let registry = gst::Registry::get();
        // Seems better than souphttpsrc
        if let Some(reqwest_src) = registry.lookup_feature("reqwesthttpsrc") {
            reqwest_src.set_rank(gst::Rank::PRIMARY + 1);
        }

        #[cfg(target_os = "android")]
        if let Some(amcaudiodec) = registry.lookup_feature("amcaudiodec") {
            // https://gitlab.freedesktop.org/gstreamer/gstreamer/-/issues/4883
            amcaudiodec.set_rank(gst::Rank::NONE);
        }

        // let player = player::Player::new(appsink, event_tx.clone(), contexts)?;
        let player = player::Player::new(appsink, event_tx.clone())?;

        let headers = Arc::new(Mutex::new(None::<HashMap<String, String>>));

        player.playbin.connect("element-setup", false, {
            let headers = Arc::clone(&headers);
            move |vals| {
                let Ok(elem) = vals[1].get::<gst::Element>() else {
                    return None;
                };

                let name = elem.factory()?.name();
                // TODO: should check for http clients and include headers
                match name.as_str() {
                    "rtspsrc" => elem.set_property("latency", 25u32),
                    "webrtcbin" => elem.set_property("latency", 25u32),
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
                        let mut did_set_user_agent = false;
                        if let Some(ref headers) = *headers.lock() {
                            let mut extra_headers_builder =
                                gst::Structure::builder("reqwesthttpsrc-extra-headers");
                            for (k, v) in headers {
                                if k == "User-Agent" || k == "user-agent" {
                                    elem.set_property("user-agent", v);
                                    did_set_user_agent = true;
                                } else {
                                    extra_headers_builder = extra_headers_builder.field(k, v);
                                }
                            }
                            elem.set_property("extra-headers", extra_headers_builder.build());
                        }
                        if !did_set_user_agent {
                            elem.set_property(
                                "user-agent",
                                user_agent::random_browser_user_agent(None),
                            );
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

        // tokio::spawn({
        //     let player_bus = player.message_bus();
        //     let event_tx = event_tx.clone();
        //     async move {
        //         let mut messages = player_bus.stream();

        //         while let Some(msg) = messages.next().await {
        //             let _ = event_tx.send(Event::Player(msg));
        //         }
        //     }
        // });

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
        std::thread::Builder::new()
            .name("image-decoder".to_owned())
            .spawn({
                let event_tx = event_tx.clone();
                move || {
                    if let Err(err) = image_decode_worker(image_decode_rx, event_tx) {
                        error!(?err, "Image decode worker failed");
                    }
                }
            })?;

        Ok(Self {
            #[cfg(target_os = "android")]
            android_app,
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
            last_position_updated: -1.0,
            http_client: reqwest::Client::new(),
            current_request_headers: headers,
            current_playlist: None,
            current_playlist_item_idx: None,
            device_name: None,
            current_media_item_id: 0,
        })
    }

    fn map_to_header_map(headers: &HashMap<String, String>) -> reqwest::header::HeaderMap {
        let mut header_map = reqwest::header::HeaderMap::new();
        for (k, v) in headers {
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

        header_map
    }

    async fn download_image(
        client: &reqwest::Client,
        url: &str,
        headers: Option<HashMap<String, String>>,
    ) -> std::result::Result<(Bytes, ImageFormat), DownloadImageError> {
        let url = url::Url::parse(url)?;
        debug!(%url, "Starting image download");
        let random_user_agent = user_agent::random_browser_user_agent(url.domain());
        let mut request = client.get(url);
        let mut did_set_user_agent = false;
        if let Some(headers) = headers {
            let header_map = Self::map_to_header_map(&headers);
            did_set_user_agent = header_map.contains_key(reqwest::header::USER_AGENT);
            request = request.headers(header_map);
        }
        if !did_set_user_agent {
            request = request.header(reqwest::header::USER_AGENT, random_user_agent);
        }

        let resp = request.send().await?;
        if !resp.status().is_success() {
            return Err(DownloadImageError::Unsuccessful(resp.status()));
        }

        let headers = resp.headers();
        let content_type = headers
            .get(reqwest::header::CONTENT_TYPE)
            .ok_or(DownloadImageError::MissingContentType)?
            .to_str()
            .map_err(|_| DownloadImageError::ContentTypeIsNotString)?;
        let format = ImageFormat::from_mime_type(content_type).ok_or(
            DownloadImageError::UnsupportedContentType(content_type.to_string()),
        )?;

        let body = resp.bytes().await?;
        Ok((body, format))
    }

    #[tracing::instrument(skip_all)]
    fn notify_updates(&mut self, force: bool) -> Result<()> {
        if !self.player.have_media_info() {
            return Ok(());
        }

        let Some(position) = self.player.get_position() else {
            error!("player does not have a playback position");
            return Ok(());
        };
        let position = position.seconds_f64();
        self.last_position_updated = position;
        let duration = match self.current_duration {
            Some(dur) => dur,
            None => {
                let dur = self.player.get_duration().unwrap_or_default();
                self.current_duration = Some(dur);
                dur
            }
        };
        let duration = duration.seconds_f64();
        let progress_str = sec_to_string(position);
        let duration_str = sec_to_string(duration);
        let progress_percent = (position / duration) as f32;
        let is_live = self.player.is_live();
        let playback_state = {
            match self.player.player_state() {
                PlayerState::Stopped | PlayerState::Buffering => GuiPlaybackState::Loading,
                PlayerState::Playing => GuiPlaybackState::Playing,
                PlayerState::Paused => GuiPlaybackState::Paused,
            }
        };

        let playback_rate = self.player.rate();
        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_progress_label(progress_str.into());
            bridge.set_duration_label(duration_str.into());
            if !bridge.get_is_scrubbing_position() {
                bridge.set_playback_position(progress_percent);
            }
            bridge.set_playback_state(playback_state);
            bridge.set_is_live(is_live);
            bridge.set_playback_rate(playback_rate as f32);
            bridge.set_duration_seconds(duration as i32);
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
                speed: Some(playback_rate),
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
        self.current_duration = None;
        self.on_uri_loaded_command_queue.clear();
        self.on_playing_command_queue.clear();
        self.have_audio_track_cover = false;
        self.current_play_data = None;
        self.have_media_info = false;
        self.pending_thumbnail = None;
        self.video_sink_is_eos
            .store(true, atomic::Ordering::Relaxed);
        self.have_media_title = false;
        self.last_position_updated = -1.0;
        *self.current_request_headers.lock() = None;
        self.current_playlist = None;
        self.current_playlist_item_idx = None;
        self.player.stop();

        self.current_thumbnail_id += 1;
        self.current_image_id += 1;
        self.current_image_download_id += 1;

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
            bridge.set_duration_seconds(0);
            bridge.set_app_state(AppState::Idle);
            bridge.set_playback_state(GuiPlaybackState::Idle);
            bridge.set_media_title("".to_shared_string());
            bridge.set_artist_name("".to_shared_string());

            bridge.set_video_tracks(slint::ModelRc::default());
            bridge.set_audio_tracks(slint::ModelRc::default());
            bridge.set_subtitle_tracks(slint::ModelRc::default());

            bridge.set_current_video_track(-1);
            bridge.set_current_audio_track(-1);
            bridge.set_current_subtitle_track(-1);
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

    fn is_playing(&self) -> bool {
        self.current_play_data.is_some() || self.current_playlist.is_some()
    }

    fn media_loaded_successfully(&mut self) {
        if !self.is_playing() {
            debug!("Ignoring old media loaded succesfully event");
            return;
        };

        // TODO: needs debouncing since seeks will trigger this too, or maybe not?
        info!("Media loaded successfully");

        #[cfg(target_os = "android")]
        self.android_app.set_window_flags(
            WindowManagerFlags::KEEP_SCREEN_ON,
            WindowManagerFlags::empty(),
        );

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
            && let Some(playlist_item_idx) = self.current_playlist_item_idx
        {
            let Some(item) = playlist.items.get(playlist_item_idx).cloned() else {
                return;
            };

            if let Some(show_duration) = item.show_duration {
                let event_tx = self.event_tx.clone();
                let id = self.current_media_item_id;
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_secs_f64(show_duration)).await;
                    let _ = event_tx.send(Event::MediaItemFinish(id));
                });
            }

            if self.updates_tx.receiver_count() > 0 {
                let event = v3::EventObject::MediaItem {
                    variant: v3::EventType::MediaItemChange,
                    item,
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
    }

    fn current_item_uri(&self) -> Option<&str> {
        if let Some(play_msg) = self.current_play_data.as_ref() {
            play_msg.url.as_deref()
        } else if let Some(playlist) = self.current_playlist.as_ref()
            && let Some(idx) = self.current_playlist_item_idx
            && let Some(item) = playlist.items.get(idx)
        {
            item.url.as_deref()
        } else {
            None
        }
    }

    fn media_error(&mut self, message: String) -> Result<()> {
        if !self.is_playing() {
            return Ok(());
        }

        error!(msg = message, "Media error");

        self.cleanup_playback_data()?;

        if self.updates_tx.receiver_count() > 0 {
            let update = v3::PlaybackUpdateMessage {
                generation_time: current_time_millis(),
                time: None,
                duration: None,
                state: PlaybackState::Idle,
                speed: None,
                item_index: None,
            };
            let msg = ReceiverToSenderMessage::Translatable {
                op: Opcode::PlaybackUpdate,
                msg: TranslatableMessage::PlaybackUpdate(update),
            };
            let _ = self.updates_tx.send(Arc::new(msg));
            let _ = self
                .updates_tx
                .send(Arc::new(ReceiverToSenderMessage::Error(
                    PlaybackErrorMessage {
                        message: message.clone(),
                    },
                )));
        }

        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_error_message(message.to_shared_string());
            bridge.set_is_showing_error_message(true);
        })?;

        Ok(())
    }

    fn media_warning(&mut self, message: String) -> Result<()> {
        // Ignore false positives because of the video sink not being ready until it has GL contexts set
        if !self.is_playing() {
            return Ok(());
        }

        warn!(msg = message, "Media warning");

        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_warning_message(message.to_shared_string());
            bridge.set_is_showing_warning_message(true);
        })?;

        Ok(())
    }

    fn media_ended(&mut self) {
        info!("Media finished");

        #[cfg(target_os = "android")]
        self.android_app.set_window_flags(
            WindowManagerFlags::empty(),
            WindowManagerFlags::KEEP_SCREEN_ON,
        );
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
        if let Some(v3::MetadataObject::Generic {
            thumbnail_url: Some(thumbnail_url),
            title,
            ..
        }) = media_item.metadata.as_ref()
        {
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

        *self.current_request_headers.lock() = media_item.headers.clone();

        if container.starts_with("image/") {
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
            // self.player.set_uri(Some(&url));
            self.player.set_uri(&url);
            if let Some(volume) = media_item.volume {
                self.on_uri_loaded_command_queue
                    .push(OnUriLoadedCommand::Volume(volume));
            }

            if let Some(rate) = media_item.speed {
                self.on_playing_command_queue
                    .push(OnFirstPlayingStateChangedCommand::Rate(rate));
            }
            if !is_for_sure_live {
                self.on_playing_command_queue
                    .push(OnFirstPlayingStateChangedCommand::Seek(
                        media_item.time.unwrap_or(0.0),
                    ));
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

        self.current_media_item_id += 1;

        Ok(())
    }

    fn handle_playlist_play_request(&mut self, play_message: &v3::PlayMessage) -> Result<()> {
        if let Some(url) = play_message.url.as_ref() {
            let url = url.clone();
            let mut play_message = play_message.clone();
            let event_tx = self.event_tx.clone();
            let client = self.http_client.clone();
            tokio::spawn(async move {
                let mut request = client.get(url);
                if let Some(headers) = play_message.headers.as_ref() {
                    request = request.headers(Self::map_to_header_map(headers));
                }
                let mut result = None;
                match request.send().await {
                    Ok(resp) => match resp.text().await {
                        Ok(json) => {
                            play_message.content = Some(json);
                            result = Some(play_message);
                        }
                        Err(err) => {
                            error!(?err, "Failed to convert response to text");
                        }
                    },
                    Err(err) => {
                        error!(?err, "Failed to download playlist json data");
                    }
                }

                let _ = event_tx.send(Event::PlaylistDataResult {
                    play_message: result,
                });
            });
        } else if play_message.content.is_some() {
            self.event_tx.send(Event::PlaylistDataResult {
                play_message: Some(play_message.clone()),
            })?;
        } else {
            bail!("No playlist available");
        }

        Ok(())
    }

    fn handle_simple_play_request(&mut self, play_message: &v3::PlayMessage) -> Result<()> {
        self.load_media_item(&Self::play_message_to_media_item(play_message.clone()))
    }

    fn video_stream_available(&self) -> Result<()> {
        if !self.is_playing() {
            debug!("Ignoring old video stream available event");
            return Ok(());
        };

        debug!("Video stream available");

        self.ui_weak.upgrade_in_event_loop(move |ui| {
            let bridge = ui.global::<Bridge>();
            bridge.set_player_variant(UiPlayerVariant::Video);
        })?;

        Ok(())
    }

    fn handle_operation(&mut self, op: Operation) -> Result<bool> {
        match op {
            Operation::Pause => {
                if self.is_playing() {
                    self.player.pause();
                }
            }
            Operation::Resume => {
                if self.is_playing() {
                    self.player.play();
                }
            }
            Operation::Stop => {
                if self.is_playing() {
                    self.player.stop();
                    self.ui_weak.upgrade_in_event_loop(|ui| {
                        ui.global::<Bridge>().set_app_state(AppState::Idle);
                    })?;
                    self.cleanup_playback_data()?;
                }
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
                if self.is_playing() {
                    let seconds = seek_message.time;
                    // if !self.seek_lock.is_locked() && seconds >= 0.0 && seconds.is_normal() {
                    // self.seek_lock.acquire();
                    // self.player.seek(gst::ClockTime::from_seconds_f64(time));
                    // TODO: show errors as warnings in the UI
                    self.player.seek(seconds);
                    // }
                }
            }
            Operation::SetSpeed(set_speed_message) => {
                // TODO: show errors as warnings in the UI
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
                // if !self.volume_lock.is_locked() {
                //     self.volume_lock.acquire();
                self.player.set_volume(set_volume_msg.volume);
                // }
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
                    port: FCAST_TCP_PORT,
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

    fn handle_new_player_event(&mut self, event: player::PlayerEvent) -> Result<()> {
        match event {
            player::PlayerEvent::EndOfStream => {
                self.player.end_of_stream_reached();

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
            player::PlayerEvent::DurationChanged => {
                self.current_duration = self.player.get_duration();
            }
            player::PlayerEvent::Tags(_tags) => {}
            player::PlayerEvent::VolumeChanged(volume) => {
                self.player.volume_changed();

                self.ui_weak.upgrade_in_event_loop(move |ui| {
                    ui.global::<Bridge>().set_volume(volume as f32);
                })?;
                if self.updates_tx.receiver_count() > 0 {
                    let update = VolumeUpdateMessage {
                        generation_time: current_time_millis(),
                        volume,
                    };

                    let msg = ReceiverToSenderMessage::Translatable {
                        op: Opcode::VolumeUpdate,
                        msg: TranslatableMessage::VolumeUpdate(update),
                    };
                    self.updates_tx.send(Arc::new(msg))?;
                    self.last_sent_update = Instant::now();
                }
            }
            player::PlayerEvent::StreamCollection(collection) => {
                self.player.handle_stream_collection(collection);
                // self.media_loaded_successfully();

                self.ui_weak.upgrade_in_event_loop(|ui| {
                    ui.invoke_playback_started();
                    ui.global::<Bridge>().set_app_state(AppState::Playing);
                })?;

                // self.current_duration = info.duration();
                // if info.number_of_video_streams() > 0 {
                //     self.video_stream_available()?;
                // }

                debug!("Commands: {:?}", self.on_playing_command_queue);
                while let Some(command) = self.on_playing_command_queue.pop() {
                    match command {
                        OnFirstPlayingStateChangedCommand::Seek(time) => {
                            // self.player.seek(gst::ClockTime::from_seconds_f64(time))
                            self.player.seek(time);
                        }
                        OnFirstPlayingStateChangedCommand::Rate(rate) => {
                            self.player.set_rate(rate);
                        }
                    }
                }

                self.player.play();

                fn trackify(streams: &[gst::Stream]) -> Vec<UiMediaTrack> {
                    streams
                        .iter()
                        .map(|stream| UiMediaTrack {
                            name: player::stream_title(stream).to_shared_string(),
                        })
                        .collect::<Vec<UiMediaTrack>>()
                }

                let video_tracks = trackify(&self.player.video_streams);
                let audio_tracks = trackify(&self.player.audio_streams);
                let subtitle_tracks = trackify(&self.player.subtitle_streams);

                self.ui_weak.upgrade_in_event_loop(move |ui| {
                    let bridge = ui.global::<Bridge>();
                    bridge.set_video_tracks(Rc::new(VecModel::from(video_tracks)).into());
                    bridge.set_audio_tracks(Rc::new(VecModel::from(audio_tracks)).into());
                    bridge.set_subtitle_tracks(Rc::new(VecModel::from(subtitle_tracks)).into());
                })?;

                if !self.have_media_info {
                    self.media_loaded_successfully();
                    self.have_media_info = true;
                }
            }
            player::PlayerEvent::AboutToFinish => {}
            player::PlayerEvent::Buffering(percent) => {
                if self.player.buffering(percent) {
                    self.notify_updates(true)?;
                }
            }
            player::PlayerEvent::IsLive => {
                // self.player.is_live = true;
                self.player.set_is_live(true);
            }
            player::PlayerEvent::StateChanged {
                old,
                current,
                pending,
            } => {
                if self.player.state_changed(old, current, pending).is_some() {
                    self.notify_updates(true)?;
                }
            }
            player::PlayerEvent::UriSet(uri) => {
                self.player.uri_set(uri);
            }
            player::PlayerEvent::UriLoaded => {
                self.player.uri_loaded();

                for command in self.on_uri_loaded_command_queue.iter() {
                    match command {
                        OnUriLoadedCommand::Volume(volume) => {
                            self.player.set_volume(*volume);
                        }
                    }
                }
            }
            player::PlayerEvent::QueueSeek(seek) => {
                self.player.queue_seek(seek);
            }
            player::PlayerEvent::StreamsSelected {
                video,
                audio,
                subtitle,
            } => {
                let (video_sid, audio_sid, subtitle_sid) = self.player.streams_selected(
                    video.as_deref(),
                    audio.as_deref(),
                    subtitle.as_deref(),
                );

                self.ui_weak.upgrade_in_event_loop(move |ui| {
                    let bridge = ui.global::<Bridge>();
                    bridge.set_current_video_track(video_sid);
                    bridge.set_current_audio_track(audio_sid);
                    bridge.set_current_subtitle_track(subtitle_sid);
                })?;

                if video.is_some() {
                    self.video_stream_available()?;
                }

                // TODO: get title from video track if available?

                if video.is_none()
                    && let Some(audio_sid) = audio
                    && let Some(track) = self.player.get_stream_from_id(&audio_sid)
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
            }
            player::PlayerEvent::RateChanged(new_rate) => {
                self.player.set_rate_changed(new_rate);
                self.notify_updates(true)?;
            }
            player::PlayerEvent::Error(msg) => {
                self.player.dump_graph();
                if let Some(player_uri) = self.player.current_uri()
                    && let Some(current_uri) = self.current_item_uri()
                    && current_uri == player_uri
                {
                    self.player.stop();
                    self.media_error(msg)?;
                }
            }
            player::PlayerEvent::Warning(msg) => {
                self.player.dump_graph();
                self.media_warning(msg)?;
            }
        }

        Ok(())
    }

    #[cfg(not(target_os = "android"))]
    fn handle_tray_event(&mut self, event: TrayEvent) -> Result<bool> {
        debug!(?event, "Handling tray event");

        match event {
            TrayEvent::Quit => return Ok(true),
            TrayEvent::Toggle => {
                self.ui_weak.upgrade_in_event_loop(|ui| {
                    let window = ui.window();
                    if let Err(err) = if window.is_visible() {
                        window.hide()
                    } else {
                        window.show()
                    } {
                        error!(?err, "Failed to toggle window visibility");
                    }
                })?;
            }
        }

        Ok(false)
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
                // let op = match self.player_state {
                let op = match self.player.player_state() {
                    // gst_play::PlayState::Paused => Operation::Resume,
                    // gst_play::PlayState::Playing => Operation::Pause,
                    PlayerState::Paused => Operation::Resume,
                    PlayerState::Playing => Operation::Pause,
                    _ => {
                        error!(
                            "Cannot resume or pause in player current state: {:?}",
                            // self.player_state
                            self.player.player_state(),
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
            // Event::Player(event) => self.handle_player_event(event).await?,
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
                        self.media_error(format!("Image download failed: {err:?}"))?;
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
            Event::PlaylistDataResult { play_message } => {
                let Some(play_message) = play_message else {
                    error!("Playlist failed to laod");
                    return Ok(false);
                };

                let Some(content) = play_message.content else {
                    // Unreachable
                    error!("Playlist play message is missing content");
                    return Ok(false);
                };

                let playlist = serde_json::from_str::<v3::PlaylistContent>(&content)?;

                let start_idx = match playlist.offset {
                    Some(idx) => idx as usize,
                    None => 0,
                };

                let Some(start_item) = playlist.items.get(start_idx) else {
                    error!(
                        start_idx,
                        ?playlist,
                        "Playlist's start index is out of bounds"
                    );
                    return Ok(false);
                };

                self.load_media_item(start_item)?; // TODO: how should errors be handled?

                self.current_playlist = Some(playlist);
                self.current_playlist_item_idx = Some(start_idx);
            }
            Event::MediaItemFinish(id) => {
                if self.current_playlist_item_idx.is_none() || id != self.current_media_item_id {
                    debug!(id, "Ignoring media item finish event");
                    return Ok(false);
                }

                if let Some(playlist) = self.current_playlist.as_ref()
                    && let Some(item_idx) = self.current_playlist_item_idx
                {
                    let next_idx = item_idx + 1;
                    if next_idx < playlist.items.len() {
                        self.handle_operation(Operation::SetPlaylistItem(
                            v3::SetPlaylistItemMessage {
                                item_index: next_idx as u64,
                            },
                        ))?;
                    } else {
                        info!("Playlist ended");
                    }
                }
            }
            #[allow(deprecated)]
            Event::SelectTrack { id, variant } => {
                debug!(id, ?variant, "Selecting track");

                let res = match variant {
                    UiMediaTrackType::Video => self.player.select_video_stream(id),
                    UiMediaTrackType::Audio => self.player.select_audio_stream(id),
                    UiMediaTrackType::Subtitle => self.player.select_subtitle_stream(id),
                };

                if let Err(err) = res {
                    error!(?err, id, ?variant, "Failed to select track");
                }
            }
            Event::NewPlayerEvent(event) => {
                self.handle_new_player_event(event)?;
            }
            #[cfg(not(target_os = "android"))]
            Event::Tray(event) => {
                return self.handle_tray_event(event);
            }
        }

        Ok(false)
    }

    pub async fn run_event_loop(
        mut self,
        mut event_rx: UnboundedReceiver<Event>,
        fin_tx: oneshot::Sender<()>,
        #[cfg(not(target_os = "android"))] cli_args: CliArgs,
    ) -> Result<()> {
        // TODO: IPv4 on windows
        let dispatch_listener = TcpListener::bind(SocketAddr::new(
            IpAddr::V6(Ipv6Addr::UNSPECIFIED),
            FCAST_TCP_PORT,
        ))
        .await?;

        #[cfg(target_os = "linux")]
        let _tray = if cli_args.no_systray {
            None
        } else {
            use ksni::TrayMethods;

            let tray = linux_tray::LinuxSysTray {
                event_tx: self.event_tx.clone(),
            };

            Some(tray.disable_dbus_name(true).spawn().await)
        };

        #[cfg(not(target_os = "android"))]
        if cli_args.fullscreen {
            self.ui_weak.upgrade_in_event_loop(|ui| {
                ui.window().set_fullscreen(true);
            })?;
        }

        let mut update_interval = tokio::time::interval(Duration::from_millis(200));

        let mut session_id: SessionId = 0;
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
                _ = update_interval.tick() => {
                    if self.player.player_state() == player::PlayerState::Playing {
                        self.notify_updates(false)?;
                    }
                }
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

        let _ = slint::quit_event_loop();

        Ok(())
    }
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

#[cfg(not(target_os = "android"))]
#[derive(clap::Parser)]
#[command(version)]
pub struct CliArgs {
    /// Start minimized to tray
    #[arg(long, default_value_t = false)]
    no_main_window: bool,
    /// Start application in fullscreen
    #[arg(long, default_value_t = false)]
    fullscreen: bool,
    /// Defines the verbosity level of the logger
    #[arg(long, alias = "log", visible_alias = "log")]
    loglevel: Option<LevelFilter>,
    /// Start player in windowed mode
    #[arg(long, default_value_t = false)]
    no_fullscreen_player: bool,
    /// Play videos in the main application window
    #[arg(long, default_value_t = false)]
    no_player_window: bool,
    /// Disable the system tray icon
    #[arg(long, default_value_t = false)]
    no_systray: bool,
}

/// Run the main app.
///
/// Slint and friends are assumed to be initialized by the platform specific target.
pub fn run(
    #[cfg(not(target_os = "android"))] cli_args: CliArgs,
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
        use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
        let log_level = cli_args.loglevel.unwrap_or(log_level());
        let filter = tracing_subscriber::filter::Targets::new()
            .with_target("tracing_gstreamer::callsite", LevelFilter::OFF)
            .with_default(log_level);
        let fmt_layer = tracing_subscriber::fmt::layer();
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        tracing_subscriber::registry()
            .with(fmt_layer)
            .with(filter)
            .init();
    }

    #[cfg(target_os = "android")]
    {
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        gst::log::set_threshold_for_name("gldebug", gst::DebugLevel::None);
    }

    let start = std::time::Instant::now();

    gst::init()?;

    debug!(gstreamer_version = %gst::version_string());

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(num_cpus::get().min(4))
        .thread_name("main-async-worker")
        .build()
        .unwrap();

    #[cfg(target_os = "linux")]
    if let Err(err) = rustls::crypto::ring::default_provider().install_default() {
        error!(
            ?err,
            "Failed to register ring as rustls default crypto provider"
        );
    }

    // let gst_gl_contexts = std::sync::Arc::new(std::sync::Mutex::new(None));

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

    #[cfg(debug_assertions)]
    ui.global::<Bridge>().set_is_debugging(true);

    let video_sink_is_eos = Arc::clone(&slint_sink.is_eos);
    ui.window().set_rendering_notifier({
        let ui_weak = ui.as_weak();
        // let gst_gl_contexts = std::sync::Arc::clone(&gst_gl_contexts);
        #[cfg(not(target_os = "android"))]
        let mut start_fullscreen = Some(cli_args.fullscreen);
        // TODO: debug to find out why gstreamer breaks after clicking systray (window toggle) on wayland

        move |state, graphics_api| {
            if let slint::RenderingState::RenderingSetup = state {
                debug!("Got graphics API: {graphics_api:?}");
                let ui_weak = ui_weak.clone();

                #[cfg(not(target_os = "android"))]
                if let Some(fullscreen) = start_fullscreen.take() {
                    ui_weak
                        .upgrade()
                        .unwrap()
                        .window()
                        .set_fullscreen(fullscreen);
                }

                slint_sink
                    .connect(
                        graphics_api,
                        move || {
                            ui_weak
                                .upgrade_in_event_loop(move |ui| {
                                    ui.window().request_redraw();
                                })
                                .unwrap();
                        },
                        // &gst_gl_contexts,
                    )
                    .unwrap();
            } else if let slint::RenderingState::BeforeRendering = state {
                let Some(ui) = ui_weak.upgrade() else {
                    error!("Failed to upgrade ui");
                    return;
                };

                let bridge = ui.global::<Bridge>();
                if bridge.get_playing() {
                    let frame = if let Some(frame) = slint_sink.fetch_next_frame() {
                        match frame {
                            Some(frame) => slint::Image::gst_frame(frame.frame, frame.info),
                            None => {
                                return;
                            }
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

    #[cfg(not(any(target_os = "android", target_os = "linux")))]
    let _tray_icon = if !cli_args.no_systray {
        let (tray, ids) = mac_win_tray::create_tray_icon();
        mac_win_tray::set_event_handler(event_tx.clone(), ids);
        Some(tray)
    } else {
        None
    };

    #[cfg(not(target_os = "android"))]
    let (no_main_window, no_systray) = (cli_args.no_main_window, cli_args.no_systray);
    runtime.spawn({
        let ui_weak = ui.as_weak();
        let event_tx = event_tx.clone();
        async move {
            fcastwhepsrcbin::plugin_init().unwrap();
            gstreqwest::plugin_register_static().unwrap();
            gstwebrtchttp::plugin_register_static().unwrap();
            gstrswebrtc::plugin_register_static().unwrap();
            #[cfg(not(target_os = "android"))]
            gstrsrtp::plugin_register_static().unwrap();

            Application::new(
                slint_appsink,
                event_tx,
                ui_weak,
                video_sink_is_eos,
                #[cfg(target_os = "android")]
                android_app,
                // gst_gl_contexts,
            )
            .await
            .unwrap()
            .run_event_loop(
                event_rx,
                fin_tx,
                #[cfg(not(target_os = "android"))]
                cli_args,
            )
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

    ui.global::<Bridge>().on_change_playback_rate({
        let event_tx = event_tx.clone();
        move |new_rate: f32| {
            log_if_err!(event_tx.send(Event::Op {
                session_id: 0,
                op: Operation::SetSpeed(fcast_protocol::SetSpeedMessage {
                    speed: new_rate as f64
                }),
            }));
        }
    });

    ui.global::<Bridge>().on_hide_cursor_hack({
        let ui_weak = ui.as_weak();
        move || {
            let ui = ui_weak
                .upgrade()
                .expect("callbacks are always called from the event loop");
            let _ = ui
                .window()
                .try_dispatch_event(slint::platform::WindowEvent::PointerReleased {
                    position: slint::LogicalPosition::new(0.0, 0.0),
                    button: slint::platform::PointerEventButton::Other,
                });
        }
    });

    ui.global::<Bridge>().on_select_track({
        let event_tx = event_tx.clone();
        move |id: i32, variant: UiMediaTrackType| {
            log_if_err!(event_tx.send(Event::SelectTrack { id, variant }));
        }
    });

    ui.global::<Bridge>()
        .on_sec_to_string(|sec: i32| -> slint::SharedString {
            sec_to_string(sec as f64).to_shared_string()
        });

    info!(initialized_in = ?start.elapsed());

    #[cfg(target_os = "android")]
    ui.run()?;

    #[cfg(not(target_os = "android"))]
    if no_systray {
        ui.run()?;
    } else {
        if !no_main_window {
            ui.show()?;
        }
        slint::run_event_loop_until_quit()?;
    }

    info!("Shutting down...");

    runtime.block_on(async move {
        let _ = event_tx.send(Event::Quit);
        let _ = fin_rx.await;
    });

    Ok(())
}
