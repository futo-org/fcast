use anyhow::{Result, bail};
use base64::Engine;
use fcast::{SessionDriver, SessionId};
use fcast_protocol::{
    Opcode, PlaybackErrorMessage, PlaybackState, SetVolumeMessage, v2::VolumeUpdateMessage, v3,
};
use gst::prelude::*;
use gst_gl::prelude::*;
use parking_lot::Mutex;
#[cfg(target_os = "android")]
use slint::android::android_activity::WindowManagerFlags;
use slint::{ToSharedString, VecModel};
use smallvec::SmallVec;
use tokio::{
    io::AsyncReadExt,
    net::TcpListener,
    sync::{
        broadcast,
        mpsc::{self, UnboundedReceiver, UnboundedSender},
    },
};
#[cfg(not(target_os = "android"))]
use tracing::level_filters::LevelFilter;
use tracing::{Instrument, debug, debug_span, error, info, warn};

use std::{
    collections::{HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    rc::Rc,
    sync::{
        Arc,
        atomic::{self, AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

#[cfg(not(target_os = "android"))]
pub use clap;
pub use slint;
pub use tracing;
mod fcast;
mod fcasttextoverlay;
mod fcastwhepsrcbin;
mod gcast;
mod graphics;
mod gui;
mod image;
#[cfg(all(target_os = "linux", feature = "systray"))]
mod linux_tray;
#[cfg(all(
    not(any(target_os = "android", target_os = "linux")),
    feature = "systray"
))]
mod mac_win_tray;
mod player;
mod raop;
mod user_agent;
mod video;

use crate::{
    fcast::{Operation, ReceiverToSenderMessage, TranslatableMessage},
    gui::{GuiController, ToastType},
    player::PlayerState,
};

use graphics::GraphicsContext;
pub use raop::{Configuration, device_name_hash, hash_to_string, txt_properties};

type SlintRgba8Pixbuf = slint::SharedPixelBuffer<slint::Rgba8Pixel>;

#[derive(Debug)]
pub enum MdnsEvent {
    NameSet(String),
    IpAdded(IpAddr),
    IpRemoved(IpAddr),
    SetIps(Vec<IpAddr>),
}

type MediaItemId = u64;
pub type EventSender = UnboundedSender<Event>;

#[cfg(feature = "systray")]
#[derive(Debug)]
pub enum TrayEvent {
    Quit,
    Toggle,
}

#[derive(Debug)]
pub enum RaopEvent {
    ConfigAvailable(raop::Configuration),
    SenderConnected(tokio::net::TcpStream),
    SenderDisconnected,
    CoverArtSet(Vec<u8>),
    CoverArtRemoved,
    MetadataSet(raop::RaopMetadata),
    ProgressUpdate {
        position_sec: u64,
        duration_sec: u64,
    },
}

#[cfg(any(target_os = "macos", target_os = "windows"))]
#[derive(Debug)]
pub enum AppUpdateEvent {
    UpdateAvailable(app_updater::Release),
    UpdateApplication,
    RestartApp,
}

#[derive(Debug)]
pub enum Event {
    Quit,
    SessionFinished,
    ResumeOrPause,
    SeekPercent(f32),
    ToggleDebug,
    NewPlayerEvent(player::PlayerEvent),
    Op {
        /// The UI also sends operations with session_id == 0
        session_id: SessionId,
        op: Operation,
    },
    Image(image::Event),
    Mdns(MdnsEvent),
    PlaylistDataResult {
        play_message: Option<v3::PlayMessage>,
    },
    MediaItemFinish(MediaItemId),
    SelectTrack {
        id: i32,
        variant: UiMediaTrackType,
    },
    #[cfg(feature = "systray")]
    Tray(TrayEvent),
    ShouldSetLoadingStatus(MediaItemId),
    Raop(RaopEvent),
    DumpPipeline,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    AppUpdate(AppUpdateEvent),
    GuiWindowClosed(oneshot::Sender<()>),
}

#[macro_export]
macro_rules! log_if_err {
    ($res:expr) => {
        if let Err(err) = $res {
            tracing::error!("{err}");
        }
    };
}

const FCAST_TCP_PORT: u16 = 46899;
const GCAST_TCP_PORT: u16 = 8009;
const SENDER_UPDATE_INTERVAL: Duration = Duration::from_millis(700);
#[cfg(any(target_os = "macos", target_os = "windows"))]
const UPDATER_BASE_URL: &str = "http://dl.fcast.org/receiver/desktop";

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
    Seek { position: f64, rate: f64 },
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

#[derive(PartialEq, Eq)]
enum PreservePlaylist {
    Yes,
    No,
}

#[derive(PartialEq, Eq)]
enum ContinueToPlay {
    Yes,
    No,
}

struct RaopServer {
    config: raop::Configuration,
}

struct GCastUpdateSender(Option<UnboundedSender<gcast::StatusUpdate>>);

impl GCastUpdateSender {
    fn send(&self, update: gcast::StatusUpdate) {
        if let Some(tx) = self.0.as_ref()
            && let Err(err) = tx.send(update)
        {
            error!(?err, "Failed to send GCast update");
        }
    }
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

struct Application {
    #[cfg(target_os = "android")]
    android_app: slint::android::AndroidApp,
    event_tx: EventSender,
    updates_tx: broadcast::Sender<Arc<ReceiverToSenderMessage>>,
    #[cfg(not(target_os = "android"))]
    mdns: mdns_sd::ServiceDaemon,
    last_sent_update: Instant,
    debug_mode: bool,
    player: player::Player,
    current_duration: Option<gst::ClockTime>,
    on_uri_loaded_command_queue: smallvec::SmallVec<[OnUriLoadedCommand; 1]>,
    on_playing_command_queue: smallvec::SmallVec<[OnFirstPlayingStateChangedCommand; 2]>,
    current_image_id: image::ImageId,
    current_image_download_id: image::ImageDownloadId,
    have_audio_track_cover: bool,
    video_sink_is_eos: Arc<AtomicBool>,
    current_play_data: Option<v3::PlayMessage>,
    have_media_info: bool,
    current_thumbnail_id: image::ImageId,
    pending_thumbnail: Option<image::ImageId>,
    pending_thumbnail_download: Option<image::ImageDownloadId>,
    current_addresses: HashSet<IpAddr>,
    have_media_title: bool,
    last_position_updated: f64,
    http_client: reqwest::Client,
    current_request_headers: Arc<Mutex<Option<HashMap<String, String>>>>,
    current_playlist: Option<v3::PlaylistContent>,
    current_playlist_item_idx: Option<usize>,
    device_name: Option<String>,
    current_media_item_id: MediaItemId,
    is_loading_media: bool,
    active_raop_session: bool,
    raop_server: Option<RaopServer>,
    gui: GuiController,
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    update: Option<app_updater::Release>,
    gcast_tx: GCastUpdateSender,
    #[cfg(not(target_os = "android"))]
    cli_args: CliArgs,
    window_visible_before_playing: Option<bool>,
    window_fullscreen_before_playing: Option<bool>,
    gl_context: graphics::GlContext,
    image_downloader: image::Downloader,
    image_decoder: image::Decoder,
}

impl Application {
    pub async fn new(
        gui: GuiController,
        appsink: gst::Element,
        event_tx: EventSender,
        video_sink_is_eos: Arc<AtomicBool>,
        gl_context: graphics::GlContext,
        #[cfg(not(target_os = "android"))] cli_args: CliArgs,
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

        let player = player::Player::new(appsink, event_tx.clone(), gl_context.clone())?;

        let headers = Arc::new(Mutex::new(None::<HashMap<String, String>>));

        player.playbin.connect("element-setup", false, {
            let headers = Arc::clone(&headers);
            move |vals| {
                let Ok(elem) = vals[1].get::<gst::Element>() else {
                    return None;
                };

                let name = elem.factory()?.name();
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

        let (updates_tx, _) = broadcast::channel(10);

        #[cfg(not(target_os = "android"))]
        let mdns = {
            use if_addrs::get_if_addrs;

            let host_name = gethostname::gethostname();
            let host_name = host_name.to_string_lossy();
            let device_name = format!("FCast-{host_name}");
            // Avoid naming confusion
            let gcast_device_name = format!("Chromecast-{host_name}");
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

            if !cli_args.no_google_cast {
                let gcast_props = HashMap::from([
                    ("fn".to_owned(), gcast_device_name.clone()),
                    ("ca".to_owned(), "1".to_owned()), // Has display
                ]);

                let gcast_service = mdns_sd::ServiceInfo::new(
                    "_googlecast._tcp.local.",
                    &gcast::get_host_name(&gcast_device_name),
                    &format!("{}.local.", uuid::Uuid::new_v4()),
                    (), // Auto
                    GCAST_TCP_PORT,
                    gcast_props,
                )?
                .enable_addr_auto();

                daemon.register(gcast_service)?;
            }

            if !cli_args.no_raop {
                let (raop_service, raop_config) = raop::service_info(device_name).unwrap();
                daemon.register(raop_service).unwrap();

                event_tx.send(Event::Raop(RaopEvent::ConfigAvailable(raop_config)))?;
            }

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

        let run_gcast = if cfg!(not(target_os = "android")) {
            !cli_args.no_google_cast
        } else {
            true
        };

        let gcast_tx = if run_gcast {
            let (gcast_tx, gcast_rx) = mpsc::unbounded_channel::<gcast::StatusUpdate>();
            tokio::spawn(gcast::run_server(event_tx.clone(), gcast_rx));
            GCastUpdateSender(Some(gcast_tx))
        } else {
            GCastUpdateSender(None)
        };

        tokio::spawn({
            let event_tx = event_tx.clone();
            async move {
                let listener = tokio::net::TcpListener::bind("[::]:46897").await.unwrap();
                loop {
                    let (mut stream, addr) = listener.accept().await.unwrap();
                    debug!(?addr, "Got connection");

                    let mut buf = [0u8; 1];
                    if let Ok(_) = stream.read_exact(&mut buf).await
                        && buf[0] == 0xFF
                    {
                        let _ = event_tx.send(Event::DumpPipeline);
                    }
                }
            }
            .instrument(debug_span!("pipeline-dbg-listener"))
        });

        #[cfg(any(target_os = "macos", target_os = "windows"))]
        tokio::spawn({
            let event_tx = event_tx.clone();
            async move {
                match app_updater::check_for_update(UPDATER_BASE_URL, env!("CARGO_PKG_VERSION"))
                    .instrument(tracing::debug_span!("check_for_updates"))
                    .await
                {
                    Ok(release) => {
                        if let Some(release) = release {
                            let _ = event_tx
                                .send(Event::AppUpdate(AppUpdateEvent::UpdateAvailable(release)));
                        }
                    }
                    Err(err) => {
                        error!(?err, "Failed to check for update");
                    }
                }
            }
        });

        let image_decoder = image::Decoder::new(event_tx.clone())?;
        let http_client = reqwest::Client::new();
        let image_downloader = image::Downloader::new(event_tx.clone(), http_client.clone());

        Ok(Self {
            #[cfg(target_os = "android")]
            android_app,
            event_tx,
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
            on_uri_loaded_command_queue: SmallVec::new(),
            on_playing_command_queue: SmallVec::new(),
            current_image_id: 0,
            have_audio_track_cover: false,
            video_sink_is_eos,
            current_play_data: None,
            have_media_info: false,
            pending_thumbnail: None,
            current_thumbnail_id: 0,
            current_image_download_id: 0,
            pending_thumbnail_download: None,
            current_addresses: HashSet::new(),
            have_media_title: false,
            last_position_updated: -1.0,
            http_client,
            current_request_headers: headers,
            current_playlist: None,
            current_playlist_item_idx: None,
            device_name: None,
            current_media_item_id: 0,
            is_loading_media: false,
            active_raop_session: false,
            raop_server: None,
            gui,
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            update: None,
            gcast_tx,
            #[cfg(not(target_os = "android"))]
            cli_args,
            window_visible_before_playing: None,
            window_fullscreen_before_playing: None,
            gl_context,
            image_downloader,
            image_decoder,
        })
    }

    #[cfg_attr(not(target_os = "android"), tracing::instrument(skip_all))]
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

        self.gcast_tx.send(gcast::StatusUpdate::Duration(duration));
        self.gcast_tx.send(gcast::StatusUpdate::Position(position));

        let is_live = self.player.is_live();
        let playback_state = {
            match self.player.player_state() {
                PlayerState::Stopped | PlayerState::Buffering => GuiPlaybackState::Loading,
                PlayerState::Playing => GuiPlaybackState::Playing,
                PlayerState::Paused => GuiPlaybackState::Paused,
            }
        };
        let playback_rate = self.player.rate();

        self.gui.set_playback_state(playback_state);
        self.gui.set_is_live(is_live);
        self.gui.set_playback_rate(playback_rate as f32);
        self.gui
            .update_playback_progress(position as f32, duration as f32);

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

    fn cleanup_playback_data(
        &mut self,
        continue_to_play: ContinueToPlay,
        preserve_playlist: PreservePlaylist,
    ) -> Result<()> {
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
        if preserve_playlist == PreservePlaylist::No {
            self.current_playlist = None;
            self.current_playlist_item_idx = None;
        }
        self.player.stop();
        self.is_loading_media = false;

        self.current_thumbnail_id += 1;
        self.current_image_id += 1;
        self.current_image_download_id += 1;

        if continue_to_play == ContinueToPlay::No {
            self.gui.set_media_title("".to_owned());
            self.gui.set_artist_name("".to_owned());
            self.gui.clear_images();
            self.gui.update_playback_progress(0.0, 0.0);
            self.gui.set_app_state(AppState::Idle);
            self.gui.set_playback_state(GuiPlaybackState::Idle);
            self.gui.clear_tracks();
            self.gui.set_track_ids(-1, -1, -1);

            if preserve_playlist == PreservePlaylist::No {
                self.gui.update_playlist(0, 0);
            }

            if let Some(fullscreen) = self.window_fullscreen_before_playing.take() {
                self.gui.set_fullscreen(fullscreen);
                // https://github.com/slint-ui/slint/issues/11267
                std::thread::sleep(std::time::Duration::from_millis(75));
            }

            if let Some(visible) = self.window_visible_before_playing.take() {
                self.gui.set_window_visibility(visible);
            }
        }

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
        self.is_loading_media = false;

        if !self.is_playing() {
            debug!("Ignoring old media loaded succesfully event");
            return;
        };

        // TODO: needs debouncing since seeks will trigger this too, or maybe not?
        info!("Media loaded successfully");

        #[cfg(target_os = "android")]
        {
            let android_app = self.android_app.clone();
            tokio::task::spawn_blocking(move || {
                android_app.set_window_flags(
                    WindowManagerFlags::KEEP_SCREEN_ON,
                    WindowManagerFlags::empty(),
                );
            });
        }

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

        self.cleanup_playback_data(ContinueToPlay::No, PreservePlaylist::No)?;

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

        self.gui.show_toast(ToastType::Error, message);

        Ok(())
    }

    fn media_warning(&mut self, message: String) -> Result<()> {
        // Ignore false positives because of the video sink not being ready until it has GL contexts set
        if !self.is_playing() {
            return Ok(());
        }

        warn!(msg = message, "Media warning");

        self.gui.show_toast(ToastType::Warning, message);

        Ok(())
    }

    fn media_ended(&mut self) -> Result<()> {
        info!("Media finished");

        #[cfg(target_os = "android")]
        {
            let android_app = self.android_app.clone();
            tokio::task::spawn_blocking(move || {
                android_app.set_window_flags(
                    WindowManagerFlags::empty(),
                    WindowManagerFlags::KEEP_SCREEN_ON,
                );
            });
        }

        // Special case for when there's a google cast sender connected
        if self.updates_tx.receiver_count() == 0 {
            self.cleanup_playback_data(ContinueToPlay::No, PreservePlaylist::Yes)?;
        }

        Ok(())
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

        let container = media_item.container.as_str();
        let player_variant = if container.starts_with("image/") {
            UiPlayerVariant::Image
        } else if container.starts_with("audio/")
            // Video streams are audio only until proven otherwise
            || container.starts_with("video/")
            || container == "application/x-whep"
            || container == "application/dash+xml"
            || container == "application/vnd.apple.mpegurl"
        {
            UiPlayerVariant::Audio
        } else {
            UiPlayerVariant::Unknown
        };

        match player_variant {
            UiPlayerVariant::Image => {
                self.cleanup_playback_data(ContinueToPlay::Yes, PreservePlaylist::Yes)?
            }
            UiPlayerVariant::Unknown | UiPlayerVariant::Audio | UiPlayerVariant::Video => {
                self.cleanup_playback_data(ContinueToPlay::No, PreservePlaylist::Yes)?
            }
            UiPlayerVariant::Raop => (),
        }

        self.window_visible_before_playing = Some(self.gui.set_window_visibility(true));
        #[cfg(not(target_os = "android"))]
        if !self.cli_args.no_fullscreen_player {
            // If the window was hidden, it takes some time before it can be fullscreened.
            if self.window_visible_before_playing == Some(false) {
                debug!(
                    "Waiting for GL contexts to become available before setting window fullscreen"
                );
                let available = self
                    .gl_context
                    .try_wait_available(Duration::from_millis(200));
                debug!(available, "Finished waiting");
            }
            self.window_fullscreen_before_playing = Some(self.gui.set_fullscreen(true));
        }

        let mut media_title = None;
        if let Some(v3::MetadataObject::Generic {
            thumbnail_url: Some(thumbnail_url),
            title,
            ..
        }) = media_item.metadata.as_ref()
        {
            media_title = title.clone();
            self.have_audio_track_cover = true;
            self.current_image_download_id += 1;
            let this_id = self.current_image_download_id;
            let url = thumbnail_url.clone();
            self.pending_thumbnail_download = Some(this_id);
            let headers = self.current_request_headers.lock().clone();
            self.image_downloader.queue_download(this_id, url, headers);
        }

        *self.current_request_headers.lock() = media_item.headers.clone();

        let mut is_image = false;
        if container.starts_with("image/") {
            self.current_image_download_id += 1;
            let id = self.current_image_download_id;
            let headers = self.current_request_headers.lock().clone();
            is_image = true;
            self.image_downloader.queue_download(id, url, headers);
        } else {
            self.player.set_uri(&url);
            if let Some(volume) = media_item.volume {
                self.on_uri_loaded_command_queue
                    .push(OnUriLoadedCommand::Volume(volume));
            }

            if !is_for_sure_live {
                let position = media_item.time.unwrap_or(0.0);
                let rate = media_item.speed.unwrap_or(1.0);
                self.on_playing_command_queue
                    .push(OnFirstPlayingStateChangedCommand::Seek { position, rate });
            }
        }

        self.have_media_title = media_title.is_some();

        self.gui.set_player_type(player_variant);
        if !is_image {
            self.gui.set_app_state(AppState::LoadingMedia);
        }
        if let Some(title) = media_title {
            self.gui.set_media_title(title);
        }

        self.video_sink_is_eos
            .store(true, atomic::Ordering::Relaxed);

        self.current_media_item_id += 1;

        if is_image {
            tokio::spawn({
                let id = self.current_media_item_id;
                let event_tx = self.event_tx.clone();
                async move {
                    tokio::time::sleep(Duration::from_millis(500)).await;
                    let _ = event_tx.send(Event::ShouldSetLoadingStatus(id));
                }
            });
        }
        self.is_loading_media = true;

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
                    request = request.headers(map_to_header_map(headers));
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

        self.gui.set_player_type(UiPlayerVariant::Video);

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
                    self.gui.set_app_state(AppState::Idle);
                    self.cleanup_playback_data(ContinueToPlay::No, PreservePlaylist::No)?;
                }
                // TODO: notify update? or wait for async state change from player
            }
            Operation::Play(play_message) => {
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
                    self.player.seek(seconds);
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
                    self.gui.set_playlist_index(new_index as i32);
                }
                {
                    error!("Cannot set playlist item when no playlist is loaded");
                }
            }
            Operation::SetVolume(msg) => {
                let volume = msg.volume;
                self.player.set_volume(volume);
                self.gui.set_volume(volume as f32);
            }
        }

        Ok(false)
    }

    fn handle_mdns_event(&mut self, event: MdnsEvent) -> Result<()> {
        match event {
            MdnsEvent::NameSet(device_name) => {
                self.device_name = Some(device_name.clone());
                self.gui.set_local_device_name(device_name);
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
            let dims = qrcode.size as u32;
            let mut pixbuf: gui::QrCodeImage = slint::SharedPixelBuffer::new(dims, dims);
            let pixbuf_pixels = pixbuf.make_mut_slice();
            for (idx, module) in qrcode.data[0..pixbuf_pixels.len()].iter().enumerate() {
                if *module == fast_qr::Module::LIGHT {
                    pixbuf_pixels[idx] = slint::Rgb8Pixel::new(0xFF, 0xFF, 0xFF);
                } else {
                    pixbuf_pixels[idx] = slint::Rgb8Pixel::new(0x00, 0x00, 0x00);
                }
            }

            self.gui.set_connection_details(pixbuf, ips_string);
        }

        Ok(())
    }

    fn handle_new_player_event(&mut self, event: player::PlayerEvent) -> Result<()> {
        match event {
            player::PlayerEvent::EndOfStream => {
                self.player.end_of_stream_reached();

                debug!("Player reached EOS");

                self.media_ended()?;

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
            player::PlayerEvent::Tags(tags) => {
                if !self.have_audio_track_cover
                    && let Some(cover) = tags.get::<gst::tags::Image>()
                    && let Some(buffer) = cover.get().buffer()
                    && let Ok(buffer) = buffer.map_readable()
                    && self.pending_thumbnail.is_none()
                {
                    self.current_thumbnail_id += 1;
                    let this_id = self.current_thumbnail_id;
                    self.image_decoder.queue_job(
                        this_id,
                        image::ImageDecodeJob::new_no_format(
                            buffer.to_vec(),
                            image::ImageDecodeJobType::AudioThumbnail,
                        ),
                    );
                    self.pending_thumbnail = Some(this_id);
                }

                if !self.have_media_title
                    && let Some(title) = tags.get::<gst::tags::Title>()
                {
                    self.have_media_title = true;
                    self.gui.set_media_title(title.get().to_owned());
                }

                if let Some(artist) = tags.get::<gst::tags::Artist>() {
                    self.gui.set_artist_name(artist.get().to_owned());
                }
            }
            player::PlayerEvent::VolumeChanged(volume) => {
                self.player.volume_changed();

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

                self.gcast_tx.send(gcast::StatusUpdate::Volume(volume));
            }
            player::PlayerEvent::StreamCollection(collection) => {
                self.player.handle_stream_collection(collection);
                // self.media_loaded_successfully();

                self.gui.set_app_state(AppState::Playing);

                // self.current_duration = info.duration();
                // if info.number_of_video_streams() > 0 {
                //     self.video_stream_available()?;
                // }

                debug!("Commands: {:?}", self.on_playing_command_queue);
                while let Some(command) = self.on_playing_command_queue.pop() {
                    #[allow(irrefutable_let_patterns)]
                    if let OnFirstPlayingStateChangedCommand::Seek { position, rate } = command {
                        self.player.seek_and_set_rate(position, rate);
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

                self.gui.set_tracks(
                    trackify(&self.player.video_streams),
                    trackify(&self.player.audio_streams),
                    trackify(&self.player.subtitle_streams),
                );

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

                self.gcast_tx
                    .send(gcast::StatusUpdate::PlayerState(self.player.player_state()));
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
                self.gui.set_track_ids(video_sid, audio_sid, subtitle_sid);
                if video.is_some() {
                    self.video_stream_available()?;
                }
            }
            player::PlayerEvent::RateChanged(new_rate) => {
                self.player.set_rate_changed(new_rate);
                self.notify_updates(true)?;
            }
            player::PlayerEvent::Error(msg) => {
                self.player.dump_graph(remote_pipeline_dbg::Trigger::Error);
                if let Some(player_uri) = self.player.current_uri()
                    && let Some(current_uri) = self.current_item_uri()
                    && current_uri == player_uri
                {
                    self.player.stop();
                    self.media_error(msg)?;
                }
            }
            player::PlayerEvent::Warning(msg) => {
                self.player
                    .dump_graph(remote_pipeline_dbg::Trigger::Warning);
                self.media_warning(msg)?;
            }
        }

        Ok(())
    }

    #[cfg(feature = "systray")]
    fn handle_tray_event(&mut self, event: TrayEvent) -> Result<bool> {
        debug!(?event, "Handling tray event");

        match event {
            TrayEvent::Quit => return Ok(true),
            TrayEvent::Toggle => self.gui.toggle_window(),
        }

        Ok(false)
    }

    #[tracing::instrument(skip_all)]
    fn handle_raop_event(&mut self, event: RaopEvent) -> Result<bool> {
        match event {
            RaopEvent::ConfigAvailable(config) => {
                let run_raop = if cfg!(not(target_os = "android")) {
                    !self.cli_args.no_raop
                } else {
                    true
                };

                if run_raop && self.raop_server.is_none() {
                    info!(?config, "Starting raop server");

                    let event_tx = self.event_tx.clone();
                    tokio::spawn(async move {
                        // IpV4 only
                        let listener = tokio::net::TcpListener::bind("0.0.0.0:33505")
                            .await
                            .unwrap();

                        loop {
                            let (stream, _) = listener.accept().await.unwrap();
                            let _ = event_tx.send(Event::Raop(RaopEvent::SenderConnected(stream)));
                        }
                    });
                    self.raop_server = Some(RaopServer { config });
                }
            }
            RaopEvent::SenderConnected(stream) => {
                if self.active_raop_session {
                    debug!("Rejecting sender");
                    return Ok(false);
                }

                let Some(server) = self.raop_server.as_ref() else {
                    error!("No server is running");
                    return Ok(false);
                };

                let config = server.config.clone();
                let event_tx = self.event_tx.clone();
                tokio::spawn(async move {
                    raop::handle_sender(stream, config, event_tx.clone()).await;
                    let _ = event_tx.send(Event::Raop(RaopEvent::SenderDisconnected));
                });

                debug!("Session started");
                self.active_raop_session = true;

                self.gui.set_app_state(AppState::Playing);
                self.gui.set_player_type(UiPlayerVariant::Raop);
            }
            RaopEvent::SenderDisconnected => {
                debug!("Session ended");
                self.active_raop_session = false;
                self.gui.set_app_state(AppState::Idle);
                self.gui.set_player_type(UiPlayerVariant::Unknown);
                self.gui.clear_common_playback_state();
            }
            RaopEvent::CoverArtSet(data) => {
                self.current_thumbnail_id += 1;
                let this_id = self.current_thumbnail_id;
                self.image_decoder.queue_job(
                    this_id,
                    image::ImageDecodeJob::new_no_format(
                        data,
                        image::ImageDecodeJobType::AudioThumbnail,
                    ),
                );
                self.pending_thumbnail = Some(this_id);
            }
            RaopEvent::CoverArtRemoved => self.gui.clear_audio_covers(),
            RaopEvent::MetadataSet(metadata) => {
                if let Some(title) = metadata.title {
                    self.gui.set_media_title(title);
                }
                if let Some(name) = metadata.artist {
                    self.gui.set_artist_name(name);
                }
            }
            RaopEvent::ProgressUpdate {
                position_sec,
                duration_sec,
            } => self
                .gui
                .update_playback_progress(position_sec as f32, duration_sec as f32),
        }

        Ok(false)
    }

    #[cfg(any(target_os = "macos", target_os = "windows"))]
    fn handle_app_update_event(&mut self, event: AppUpdateEvent) -> Result<bool> {
        match event {
            AppUpdateEvent::UpdateAvailable(release) => {
                self.update = Some(release);
                self.gui.set_updater_state(UiUpdaterState::ShowingDialog);
            }
            AppUpdateEvent::UpdateApplication => {
                let Some(update) = self.update.take() else {
                    error!("User want's to update but no updates available");
                    return Ok(false);
                };

                let gui_tx = self.gui.tx.clone();
                tokio::spawn(async move {
                    let res = app_updater::download_update(UPDATER_BASE_URL, &update, {
                        let gui_tx = gui_tx.clone();
                        move |progress, total| {
                            let progress_percent = if total == 0 {
                                0.0
                            } else {
                                progress as f64 / total as f64
                            } * 100.0;

                            let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdateDownloadProgress(
                                progress_percent as i32,
                            ));
                        }
                    })
                    .await;

                    let update_file = match res {
                        Ok(update) => update,
                        Err(err) => {
                            let error_msg = err.to_shared_string();
                            let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdateState(
                                UiUpdaterState::DownloadFailed,
                            ));
                            let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdaterError(error_msg));
                            return;
                        }
                    };

                    if let Err(err) = app_updater::install_update(
                        #[cfg(target_os = "macos")]
                        "FCast Receiver.app",
                        update_file,
                        Box::new(|closure| {
                            slint::invoke_from_event_loop(move || {
                                (closure)();
                            })
                            .is_err()
                        }),
                    )
                    .await
                    {
                        error!(?err, "Failed to install update");
                        let error_msg = err.to_shared_string();
                        let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdateState(
                            UiUpdaterState::InstallFailed,
                        ));
                        let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdaterError(error_msg));
                        return;
                    }

                    debug!(?update, "Successfully updated");

                    let _ = gui_tx.send(gui::UpdateGuiCommand::SetUpdateState(
                        UiUpdaterState::InstallSuccessful,
                    ));
                });
            }
            AppUpdateEvent::RestartApp => {
                debug!("Restarting app...");
                app_updater::restart_application();
            }
        }

        Ok(false)
    }

    fn handle_image_event(&mut self, event: image::Event) -> Result<bool> {
        match event {
            image::Event::DownloadResult { id, res } => {
                debug!(id, "Got image download result");

                if Some(id) == self.pending_thumbnail_download {
                    match res {
                        Ok((encoded_image, format)) => {
                            self.pending_thumbnail_download = None;
                            self.current_thumbnail_id += 1;
                            let this_id = self.current_thumbnail_id;
                            self.pending_thumbnail = Some(this_id);
                            self.image_decoder.queue_job(
                                this_id,
                                image::ImageDecodeJob::new(
                                    encoded_image,
                                    format,
                                    image::ImageDecodeJobType::AudioThumbnail,
                                ),
                            );
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
                        self.image_decoder.queue_job(
                            this_id,
                            image::ImageDecodeJob::new(
                                encoded_image,
                                format,
                                image::ImageDecodeJobType::Regular,
                            ),
                        );
                    }
                    Err(err) => {
                        self.media_error(format!("Image download failed: {err:?}"))?;
                    }
                }
            }
            image::Event::AudioThumbnailAvailable(img) => {
                if let Some(pending_thumbnail) = self.pending_thumbnail
                    && pending_thumbnail == img.id
                {
                    self.gui.set_audio_track_cover(img);
                }
            }
            image::Event::AudioThumbnailBlurAvailable(img) => {
                if let Some(pending_thumbnail) = self.pending_thumbnail
                    && pending_thumbnail == img.id
                {
                    // NOTE: `AudioThumbnailBlurAvailable` is assumed to *always* be received after `AudioThumbnailAvailable`
                    //       and no other thumbnail results in between.
                    self.pending_thumbnail = None;
                    self.gui.set_blured_audio_track_cover(img);
                }
            }
            image::Event::Decoded(img) => {
                if img.id != self.current_image_id {
                    warn!(img.id, "Ignoring old image decode result");
                    return Ok(false);
                }

                self.gui.set_image_preview(img);
                self.gui.set_app_state(AppState::Playing);

                self.media_loaded_successfully();
            }
            image::Event::DecodedAnimation { id, frames } => {
                if id != self.current_image_id {
                    warn!(id, "Ignoring old image decode result");
                    return Ok(false);
                }

                self.gui.set_animation(frames);
                self.gui.set_app_state(AppState::Playing);

                self.media_loaded_successfully();
            }
        }

        Ok(false)
    }

    /// Returns `true` if the event loop should exit
    async fn handle_event(&mut self, event: Event) -> Result<bool> {
        match event {
            Event::SessionFinished => {
                self.gui.device_disconnected();
            }
            Event::ResumeOrPause => {
                let op = match self.player.player_state() {
                    PlayerState::Paused => Operation::Resume,
                    PlayerState::Playing => Operation::Pause,
                    _ => {
                        error!(
                            "Cannot resume or pause in player current state: {:?}",
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
            Event::Op { session_id: id, op } => {
                debug!(id, ?op, "Operation from sender");
                return self.handle_operation(op);
            }
            Event::Image(event) => return self.handle_image_event(event),
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
                let length = playlist.items.len();

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

                self.gui.update_playlist(start_idx as i32, length as i32);
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
            #[cfg(feature = "systray")]
            Event::Tray(event) => {
                return self.handle_tray_event(event);
            }
            Event::ShouldSetLoadingStatus(id) => {
                if id == self.current_media_item_id && self.is_loading_media {
                    self.gui.set_app_state(AppState::LoadingMedia);
                }
            }
            Event::Raop(event) => return self.handle_raop_event(event),
            Event::DumpPipeline => {
                self.player.dump_graph(remote_pipeline_dbg::Trigger::Manual);
            }
            #[cfg(any(target_os = "macos", target_os = "windows"))]
            Event::AppUpdate(event) => return self.handle_app_update_event(event),
            Event::GuiWindowClosed(feedback) => {
                let _ = self.handle_operation(Operation::Stop);
                self.player.shutdown(feedback);
            }
        }

        Ok(false)
    }

    pub async fn run_event_loop(
        mut self,
        mut event_rx: UnboundedReceiver<Event>,
        fin_tx: tokio::sync::oneshot::Sender<()>,
    ) -> Result<()> {
        macro_rules! listener_stream {
            ($addr:expr) => {
                futures::stream::unfold(
                    TcpListener::bind(SocketAddr::new($addr, FCAST_TCP_PORT)).await?,
                    |listener| async move { Some((listener.accept().await, listener)) },
                )
            };
        }

        #[cfg(target_os = "windows")]
        let ipv4_stream = listener_stream!(IpAddr::V4(std::net::Ipv4Addr::UNSPECIFIED));
        let ipv6_stream = listener_stream!(IpAddr::V6(std::net::Ipv6Addr::UNSPECIFIED));

        #[cfg(target_os = "windows")]
        tokio::pin!(ipv4_stream);
        tokio::pin!(ipv6_stream);

        #[cfg(target_os = "windows")]
        let listener_stream = futures::stream::select(ipv4_stream, ipv6_stream);
        #[cfg(not(target_os = "windows"))]
        let mut listener_stream = ipv6_stream;

        #[cfg(target_os = "windows")]
        tokio::pin!(listener_stream);

        #[cfg(all(target_os = "linux", feature = "systray"))]
        let _tray = if self.cli_args.no_systray {
            None
        } else {
            use ksni::TrayMethods;

            let tray = linux_tray::LinuxSysTray {
                event_tx: self.event_tx.clone(),
            };

            Some(tray.disable_dbus_name(true).spawn().await)
        };

        #[cfg(not(target_os = "android"))]
        if self.cli_args.fullscreen {
            self.gui.set_fullscreen(true);
        }

        let mut update_interval = tokio::time::interval(Duration::from_millis(200));

        use futures::stream::StreamExt;

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
                session = listener_stream.select_next_some() => {
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
                                .await
                            {
                                error!("Session exited with error: {err}");
                            }

                            if let Err(err) = event_tx.send(Event::SessionFinished) {
                                error!("Failed to send SessionFinished: {err}");
                            }
                        }
                    });

                    self.gui.device_connected();

                    session_id += 1;
                }
            }
        }

        debug!("Quitting");

        self.player.stop();
        self.gui.quit_loop();

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

#[cfg(not(target_os = "android"))]
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
    /// Disable the system tray icon
    #[arg(long, default_value_t = false)]
    no_systray: bool,
    /// Disable the RAOP receiver
    #[arg(long, default_value_t = false)]
    no_raop: bool,
    /// Disable the Google Cast receiver
    #[arg(long, default_value_t = false)]
    no_google_cast: bool,
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

    #[cfg(target_os = "windows")]
    {
        let mut plugin_dir = std::env::current_exe()?;
        plugin_dir.pop();
        unsafe { std::env::set_var("GST_PLUGIN_PATH", plugin_dir) };
    }

    #[cfg(target_os = "macos")]
    {
        let mut plugin_dir = std::env::current_exe()?;
        plugin_dir.pop();
        plugin_dir.push("lib");
        unsafe { std::env::set_var("GST_PLUGIN_PATH", plugin_dir) };
    }

    #[cfg(not(target_os = "android"))]
    {
        use tracing_subscriber::{layer::SubscriberExt, util::SubscriberInitExt};
        let log_level = cli_args.loglevel.unwrap_or(log_level());
        let filter = tracing_subscriber::filter::Targets::new()
            .with_target("tracing_gstreamer::callsite", LevelFilter::OFF)
            .with_target("mdns_sd", LevelFilter::INFO)
            .with_target("hyper_util", LevelFilter::INFO)
            .with_target("h2", LevelFilter::INFO)
            .with_target("winit", LevelFilter::INFO)
            .with_default(log_level);
        let fmt_layer = tracing_subscriber::fmt::layer();
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        let registry = tracing_subscriber::registry().with(fmt_layer).with(filter);
        #[cfg(feature = "tracy")]
        let registry = registry.with(tracing_tracy::TracyLayer::default());
        registry.init();
    }

    #[cfg(target_os = "android")]
    {
        gst::log::set_default_threshold(gst::DebugLevel::Warning);
        gst::log::set_threshold_for_name("gldebug", gst::DebugLevel::None);
        gst::log::set_threshold_for_name("video-info", gst::DebugLevel::None);
    }

    let start = std::time::Instant::now();

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

    #[cfg(any(target_os = "windows", target_os = "macos"))]
    if let Err(err) = tokio_rustls::rustls::crypto::aws_lc_rs::default_provider().install_default()
    {
        error!(
            ?err,
            "Failed to register aws_lc_rs as rustls default crypto provider"
        );
    }

    let gst_gl_contexts = graphics::GlContext::new();

    let (event_tx, event_rx) = mpsc::unbounded_channel::<Event>();
    let (fin_tx, fin_rx) = tokio::sync::oneshot::channel::<()>();

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

    let slint_sink_mutex = Arc::new(parking_lot::Mutex::new(None::<video::SlintOpenGLSink>));

    let ui = MainWindow::new()?;

    let bridge = ui.global::<Bridge>();

    #[cfg(debug_assertions)]
    bridge.set_is_debugging(true);

    ui.window().set_rendering_notifier({
        let ui_weak = ui.as_weak();
        let gst_gl_contexts = gst_gl_contexts.clone();
        #[cfg(not(target_os = "android"))]
        let mut start_fullscreen = Some(cli_args.fullscreen);
        let mut prev_size = (0, 0);
        let mut slint_sink = None;
        let slint_sink_mutex = Arc::clone(&slint_sink_mutex);
        let mut graphics_context = GraphicsContext::None;
        let event_tx = event_tx.clone();
        move |state, graphics_api| match state {
            slint::RenderingState::RenderingSetup => {
                debug!("Got graphics API: {graphics_api:?}");
                let ui_weak = ui_weak.clone();

                graphics_context = GraphicsContext::from_slint(graphics_api).unwrap();

                #[cfg(not(target_os = "android"))]
                if let Some(fullscreen) = start_fullscreen.take() {
                    ui_weak
                        .upgrade()
                        .unwrap()
                        .window()
                        .set_fullscreen(fullscreen);
                }
            }
            slint::RenderingState::BeforeRendering => {
                let Some(slint_sink) = slint_sink.as_mut() else {
                    slint_sink = slint_sink_mutex.lock().take();
                    return;
                };

                if let Some((gst_gl_context, gst_gl_display)) = graphics_context.get_gst_contexts()
                {
                    gst_gl_context
                        .activate(true)
                        .expect("could not activate GStreamer GL context");
                    gst_gl_context
                        .fill_info()
                        .expect("failed to fill GL info for wrapped context");

                    slint_sink.gst_gl_context = Some(gst_gl_context.clone());

                    gst_gl_contexts.set_contexts(gst_gl_display, gst_gl_context);
                }

                graphics_context = GraphicsContext::Initialized;

                let Some(ui) = ui_weak.upgrade() else {
                    error!("Failed to upgrade ui");
                    return;
                };

                let new_size = ui.window().size();
                let new_size = (new_size.width, new_size.height);
                if new_size != prev_size {
                    slint_sink.window_width.store(new_size.0, Ordering::Relaxed);
                    slint_sink
                        .window_height
                        .store(new_size.1, Ordering::Relaxed);
                    prev_size = new_size;
                    if let Some(sink_pad) = slint_sink.appsink.static_pad("sink") {
                        sink_pad.push_event(gst::event::Reconfigure::builder().build());
                    }
                }

                let bridge = ui.global::<Bridge>();
                if bridge.invoke_is_playing() {
                    let frame = if let Some(frame) = slint_sink.fetch_next_frame() {
                        match frame {
                            Some(frame) => unsafe {
                                slint::BorrowedOpenGLTextureBuilder::new_gl_2d_rgba_texture(
                                    frame.tex_id,
                                    (frame.width, frame.height).into(),
                                )
                                .build()
                            },
                            None => return,
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

                let subtitles = slint_sink.fetch_next_subtitles();
                if let Some(subtitles) = subtitles {
                    let subtitles: Option<VecModel<slint::SharedString>> = subtitles
                        .map(|subs| subs.into_iter().map(|s| s.to_shared_string()).collect());
                    if let Some(subs) = subtitles {
                        bridge.set_subtitles(Rc::new(subs).into());
                    }
                } else {
                    bridge.set_subtitles(slint::ModelRc::default());
                }
            }
            slint::RenderingState::RenderingTeardown => {
                let (feedback_tx, feedback_rx) = oneshot::channel::<()>();

                match event_tx.send(Event::GuiWindowClosed(feedback_tx)) {
                    Ok(_) => match feedback_rx.recv_timeout(Duration::from_millis(2500)) {
                        Ok(_) => debug!("Player shutdown successfully"),
                        Err(err) => {
                            error!(?err, "Failed to receive feedback of player shutdown")
                        }
                    },
                    Err(err) => error!(?err, "Failed to send window closed event"),
                }

                gst_gl_contexts.deactivate_and_clear();

                if let Some(sink) = slint_sink.as_mut() {
                    sink.release_state();
                }
            }
            _ => (),
        }
    })?;

    #[cfg(all(
        not(any(target_os = "android", target_os = "linux")),
        feature = "systray"
    ))]
    let _tray_icon = if !cli_args.no_systray {
        let (tray, ids) = mac_win_tray::create_tray_icon();
        mac_win_tray::set_event_handler(event_tx.clone(), ids);
        Some(tray)
    } else {
        None
    };

    let (gui_tx, gui_rx) = mpsc::unbounded_channel::<gui::UpdateGuiCommand>();

    gui::spawn_command_handler(ui.as_weak(), gui_rx);

    let gui = GuiController::new(gui_tx);

    #[allow(unused_variables)]
    #[cfg(not(target_os = "android"))]
    let (no_main_window, no_systray) = (cli_args.no_main_window, cli_args.no_systray);
    runtime.spawn({
        let ui_weak = ui.as_weak();
        let event_tx = event_tx.clone();
        let slint_sink_mutex = Arc::clone(&slint_sink_mutex);
        async move {
            gst::init().unwrap();
            debug!(gstreamer_version = %gst::version_string());

            // TODO: investigate why certain files leads to crashes when this is added
            // gst::rust_allocator().clone().set_default();

            let mut slint_sink = video::SlintOpenGLSink::new().unwrap();
            let slint_appsink = slint_sink.video_sink();
            let video_sink_is_eos = Arc::clone(&slint_sink.is_eos);

            slint_sink
                .connect({
                    let ui_weak = ui_weak.clone();
                    move || {
                        ui_weak
                            .upgrade_in_event_loop(move |ui| {
                                ui.window().request_redraw();
                            })
                            .unwrap();
                    }
                })
                .unwrap();

            *slint_sink_mutex.lock() = Some(slint_sink);

            fcastwhepsrcbin::plugin_init().unwrap();
            fcasttextoverlay::plugin_init().unwrap();
            gstreqwest::plugin_register_static().unwrap();

            #[cfg(feature = "static-gst-plugins")]
            {
                gstwebrtchttp::plugin_register_static().unwrap();
                gstrswebrtc::plugin_register_static().unwrap();
                #[cfg(not(target_os = "android"))]
                gstrsrtp::plugin_register_static().unwrap();
                gstdav1d::plugin_register_static().unwrap();
            }

            Application::new(
                gui,
                slint_appsink,
                event_tx,
                video_sink_is_eos,
                #[cfg(target_os = "android")]
                android_app,
                gst_gl_contexts,
                #[cfg(not(target_os = "android"))]
                cli_args,
            )
            .await
            .unwrap()
            .run_event_loop(event_rx, fin_tx)
            .await
            .unwrap();
        }
    });

    gui::register_callbacks(&ui, &bridge, event_tx.clone());

    #[cfg(not(target_os = "android"))]
    let _awake = keepawake::Builder::default()
        .display(true)
        .reason("Media playback")
        .app_name("FCast Receiver")
        .app_reverse_domain("org.fcast.receiver")
        .create();

    info!(initialized_in = ?start.elapsed());

    #[cfg(not(target_os = "android"))]
    let _ = ctrlc::set_handler(|| {
        debug!("Got Ctrl+C");
        let _ = slint::quit_event_loop();
    });

    #[cfg(any(target_os = "android", not(feature = "systray")))]
    ui.run()?;

    #[cfg(feature = "systray")]
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
