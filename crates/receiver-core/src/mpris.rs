#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PlaybackStatus {
    Playing,
    Paused,
    Stopped,
}

impl PlaybackStatus {
    fn as_str(self) -> &'static str {
        match self {
            PlaybackStatus::Playing => "Playing",
            PlaybackStatus::Paused => "Paused",
            PlaybackStatus::Stopped => "Stopped",
        }
    }
}

/// State deltas pushed from the application event loop to the MPRIS task.
#[derive(Debug, Clone)]
pub enum MprisUpdate {
    /// Playback status changed (play/pause/buffering/idle transition).
    Status(PlaybackStatus),
    /// Volume changed (linear `0.0..=1.0`).
    Volume(f64),
    /// Playback rate changed.
    Rate(f64),
    /// Position resync from a discontinuity / state edge (no `Seeked` signal).
    Position { position_us: i64, length_us: i64 },
    /// An explicit seek: resync position and emit the MPRIS `Seeked` signal.
    Seeked { position_us: i64, length_us: i64 },
    /// New title/artist tags arrived for the current item.
    Metadata {
        title: Option<String>,
        artist: Option<String>,
    },
    /// A new media item prerolled: bump the track id, clear previous title/artist, mark playable
    /// and set seekability.
    Loaded { length_us: i64, can_seek: bool },
    /// Playback stopped or ended: status `Stopped`, not playable, metadata cleared.
    Stopped,
}

#[cfg(target_os = "linux")]
pub use imp::run;

#[cfg(target_os = "linux")]
mod imp {
    use std::collections::{BTreeSet, HashMap};
    use std::sync::Arc;
    use std::time::Instant;

    use fcast_protocol::v3;
    use tokio::sync::mpsc::UnboundedReceiver;
    use tracing::{debug, error, info, warn};
    use zbus::object_server::{InterfaceRef, SignalEmitter};
    use zbus::{interface, zvariant};

    use super::{MprisUpdate, PlaybackStatus};
    use crate::application::PacketOrigin;
    use crate::fcast::WrappedPlayMessage;
    use crate::media_formats::{Container, Protocol, SupportedFormats};
    use crate::message::MessageSender;
    use crate::Operation;

    const PATH: &str = "/org/mpris/MediaPlayer2";
    const NAME: &str = "org.mpris.MediaPlayer2.fcast";

    fn us_to_clock(us: i64) -> gst::ClockTime {
        gst::ClockTime::from_useconds(us.max(0) as u64)
    }

    fn guess_mime(uri: &str) -> String {
        let path = uri.split(['?', '#']).next().unwrap_or(uri);
        let ext = path.rsplit('.').next().unwrap_or("").to_ascii_lowercase();
        match ext.as_str() {
            "mp4" | "m4v" | "mov" => "video/mp4",
            "mkv" => "video/x-matroska",
            "webm" => "video/webm",
            "avi" => "video/x-msvideo",
            "ts" => "video/mp2t",
            "mp3" => "audio/mpeg",
            "m4a" | "aac" => "audio/mp4",
            "flac" => "audio/flac",
            "ogg" | "oga" | "opus" => "audio/ogg",
            "wav" => "audio/wav",
            "m3u8" => "application/x-mpegURL",
            "mpd" => "application/dash+xml",
            _ => "application/octet-stream",
        }
        .to_owned()
    }

    fn mime_types_for(formats: &SupportedFormats) -> Vec<String> {
        let mut set = BTreeSet::new();
        for container in &formats.containers {
            let mimes: &[&str] = match container {
                Container::Mp4 => &["video/mp4", "audio/mp4"],
                Container::Quicktime => &["video/quicktime"],
                Container::Mkv => &["video/x-matroska"],
                Container::Webm => &["video/webm", "audio/webm"],
                Container::MpegTs => &["video/mp2t"],
                Container::Avi => &["video/x-msvideo"],
                Container::Wav => &["audio/wav", "audio/x-wav"],
                Container::Ogg => &["application/ogg", "audio/ogg", "video/ogg"],
                Container::Flv => &["video/x-flv"],
                Container::Hls => &["application/x-mpegURL", "application/vnd.apple.mpegurl"],
                Container::Dash => &["application/dash+xml"],
            };
            set.extend(mimes.iter().map(|m| (*m).to_owned()));
        }
        set.into_iter().collect()
    }

    fn uri_schemes_for(formats: &SupportedFormats) -> Vec<String> {
        let mut set = BTreeSet::new();
        set.insert("file".to_owned());
        for protocol in &formats.protocols {
            let scheme = match protocol {
                Protocol::Http => "http",
                Protocol::Https => "https",
                Protocol::Rtmp => "rtmp",
                Protocol::Data => "data",
                Protocol::Rtsp => "rtsp",
                Protocol::Srt => "srt",
                Protocol::Whep | Protocol::Sabr => continue,
            };
            set.insert(scheme.to_owned());
        }
        set.into_iter().collect()
    }

    struct Root {
        mime_types: Vec<String>,
        uri_schemes: Vec<String>,
    }

    #[interface(name = "org.mpris.MediaPlayer2")]
    impl Root {
        async fn raise(&self) {}

        async fn quit(&self) {}

        #[zbus(property)]
        async fn can_quit(&self) -> bool {
            false
        }

        #[zbus(property)]
        async fn can_raise(&self) -> bool {
            false
        }

        #[zbus(property)]
        async fn has_track_list(&self) -> bool {
            false
        }

        #[zbus(property)]
        async fn identity(&self) -> String {
            "FCast Receiver".to_owned()
        }

        #[zbus(property)]
        async fn desktop_entry(&self) -> String {
            "org.fcast.Receiver".to_owned()
        }

        #[zbus(property)]
        async fn supported_uri_schemes(&self) -> Vec<String> {
            self.uri_schemes.clone()
        }

        #[zbus(property)]
        async fn supported_mime_types(&self) -> Vec<String> {
            self.mime_types.clone()
        }
    }

    struct Player {
        msg_tx: MessageSender,
        status: PlaybackStatus,
        volume: f64,
        rate: f64,
        can_play: bool,
        can_seek: bool,
        last_position_us: i64,
        position_anchor: Instant,
        length_us: i64,
        track_number: u64,
        title: Option<String>,
        artist: Option<String>,
    }

    impl Player {
        fn new(msg_tx: MessageSender) -> Self {
            Self {
                msg_tx,
                status: PlaybackStatus::Stopped,
                volume: 1.0,
                rate: 1.0,
                can_play: false,
                can_seek: false,
                last_position_us: 0,
                position_anchor: Instant::now(),
                length_us: 0,
                track_number: 0,
                title: None,
                artist: None,
            }
        }

        fn send(&self, op: Operation) {
            self.msg_tx.operation(PacketOrigin::Mpris, op);
        }

        fn track_path(&self) -> String {
            format!("/org/fcast/receiver/track/{}", self.track_number)
        }

        fn position_now_us(&self) -> i64 {
            if self.status == PlaybackStatus::Playing {
                let elapsed_us = self.position_anchor.elapsed().as_micros() as f64;
                let pos = self.last_position_us + (elapsed_us * self.rate) as i64;
                if self.length_us > 0 {
                    pos.clamp(0, self.length_us)
                } else {
                    pos.max(0)
                }
            } else {
                self.last_position_us
            }
        }

        fn build_metadata(&self) -> HashMap<String, zvariant::OwnedValue> {
            use zvariant::Value;

            // Value -> OwnedValue is infallible for the basic types used here.
            let owned = |v: Value| zvariant::OwnedValue::try_from(v).expect("owned metadata value");

            let mut m = HashMap::new();
            let path = zvariant::ObjectPath::try_from(self.track_path())
                .expect("valid track object path");
            m.insert("mpris:trackid".to_owned(), owned(Value::from(path)));
            if self.length_us > 0 {
                m.insert("mpris:length".to_owned(), owned(Value::from(self.length_us)));
            }
            if let Some(title) = &self.title {
                m.insert("xesam:title".to_owned(), owned(Value::from(title.clone())));
            }
            if let Some(artist) = &self.artist {
                m.insert(
                    "xesam:artist".to_owned(),
                    owned(Value::from(vec![artist.clone()])),
                );
            }
            m
        }
    }

    #[interface(name = "org.mpris.MediaPlayer2.Player")]
    impl Player {
        async fn play(&self) {
            self.send(Operation::Resume);
        }

        async fn pause(&self) {
            self.send(Operation::Pause);
        }

        async fn play_pause(&self) {
            self.send(Operation::ResumeOrPause);
        }

        async fn stop(&self) {
            self.send(Operation::Stop);
        }

        async fn next(&self) {}

        async fn previous(&self) {}

        /// Relative seek by `offset` microseconds (may be negative).
        async fn seek(&self, offset: i64) {
            let target = self.position_now_us().saturating_add(offset).max(0);
            self.send(Operation::Seek(us_to_clock(target)));
        }

        /// Absolute seek. Ignored if `track_id` is not the current track, per spec.
        async fn set_position(&self, track_id: zvariant::ObjectPath<'_>, position: i64) {
            if track_id.as_str() != self.track_path() || position < 0 {
                return;
            }
            let target = if self.length_us > 0 {
                position.min(self.length_us)
            } else {
                position
            };
            self.send(Operation::Seek(us_to_clock(target)));
        }

        async fn open_uri(&self, uri: String) {
            let msg = v3::PlayMessage {
                container: guess_mime(&uri),
                url: Some(uri),
                content: None,
                time: None,
                volume: None,
                speed: None,
                headers: None,
                metadata: None,
            };
            self.send(Operation::PlayNew(WrappedPlayMessage::Legacy(msg)));
        }

        #[zbus(property)]
        async fn playback_status(&self) -> String {
            self.status.as_str().to_owned()
        }

        #[zbus(property)]
        async fn rate(&self) -> f64 {
            self.rate
        }

        #[zbus(property)]
        async fn set_rate(&mut self, rate: f64) {
            if rate > 0.0 {
                self.send(Operation::SetSpeed(rate as f32));
            }
        }

        #[zbus(property)]
        async fn volume(&self) -> f64 {
            self.volume
        }

        #[zbus(property)]
        async fn set_volume(&mut self, volume: f64) {
            self.send(Operation::SetVolume(volume.clamp(0.0, 1.0) as f32));
        }

        // Position is polled by clients and must NOT be part of PropertiesChanged (MPRIS spec).
        #[zbus(property(emits_changed_signal = "false"))]
        async fn position(&self) -> i64 {
            self.position_now_us()
        }

        #[zbus(property)]
        async fn minimum_rate(&self) -> f64 {
            0.25
        }

        #[zbus(property)]
        async fn maximum_rate(&self) -> f64 {
            2.0
        }

        #[zbus(property)]
        async fn can_go_next(&self) -> bool {
            false
        }

        #[zbus(property)]
        async fn can_go_previous(&self) -> bool {
            false
        }

        #[zbus(property)]
        async fn can_play(&self) -> bool {
            self.can_play
        }

        #[zbus(property)]
        async fn can_pause(&self) -> bool {
            self.can_play
        }

        #[zbus(property)]
        async fn can_seek(&self) -> bool {
            self.can_seek
        }

        #[zbus(property(emits_changed_signal = "const"))]
        async fn can_control(&self) -> bool {
            true
        }

        #[zbus(property)]
        async fn metadata(&self) -> HashMap<String, zvariant::OwnedValue> {
            self.build_metadata()
        }

        #[zbus(signal)]
        async fn seeked(emitter: &SignalEmitter<'_>, position: i64) -> zbus::Result<()>;
    }

    /// Apply one state delta: mutate the cached fields and emit the matching `PropertiesChanged` /
    /// `Seeked` signals.
    #[allow(clippy::float_cmp)]
    async fn apply(iface_ref: &InterfaceRef<Player>, update: MprisUpdate) -> zbus::Result<()> {
        let emitter = iface_ref.signal_emitter();
        // Deferred until the write lock is released (the InterfaceRef signal impl does not need the
        // interface lock, but keep it clean).
        let mut seek_to: Option<i64> = None;

        {
            let mut it = iface_ref.get_mut().await;
            match update {
                MprisUpdate::Status(status) => {
                    if it.status != status {
                        // Freeze/unfreeze the position extrapolation across the transition by
                        // re-anchoring at the current value.
                        it.last_position_us = it.position_now_us();
                        it.position_anchor = Instant::now();
                        it.status = status;
                        it.playback_status_changed(emitter).await?;
                    }
                }
                MprisUpdate::Volume(volume) => {
                    if it.volume != volume {
                        it.volume = volume;
                        it.volume_changed(emitter).await?;
                    }
                }
                MprisUpdate::Rate(rate) => {
                    if it.rate != rate {
                        it.last_position_us = it.position_now_us();
                        it.position_anchor = Instant::now();
                        it.rate = rate;
                        it.rate_changed(emitter).await?;
                    }
                }
                MprisUpdate::Position {
                    position_us,
                    length_us,
                } => {
                    it.last_position_us = position_us;
                    it.position_anchor = Instant::now();
                    if length_us > 0 && it.length_us != length_us {
                        it.length_us = length_us;
                        it.metadata_changed(emitter).await?;
                    }
                }
                MprisUpdate::Seeked {
                    position_us,
                    length_us,
                } => {
                    it.last_position_us = position_us;
                    it.position_anchor = Instant::now();
                    if length_us > 0 && it.length_us != length_us {
                        it.length_us = length_us;
                        it.metadata_changed(emitter).await?;
                    }
                    seek_to = Some(position_us);
                }
                MprisUpdate::Metadata { title, artist } => {
                    // `None` means "unchanged", not "clear": title and artist arrive in separate
                    // tag events, so a title-only update must not wipe the artist and vice versa. A
                    // new load clears both via `Loaded`.
                    let mut changed = false;
                    if title.is_some() && it.title != title {
                        it.title = title;
                        changed = true;
                    }
                    if artist.is_some() && it.artist != artist {
                        it.artist = artist;
                        changed = true;
                    }
                    if changed {
                        it.metadata_changed(emitter).await?;
                    }
                }
                MprisUpdate::Loaded {
                    length_us,
                    can_seek,
                } => {
                    it.track_number += 1;
                    it.length_us = length_us;
                    it.title = None;
                    it.artist = None;
                    it.last_position_us = 0;
                    it.position_anchor = Instant::now();
                    let can_play_changed = !it.can_play;
                    it.can_play = true;
                    let can_seek_changed = it.can_seek != can_seek;
                    it.can_seek = can_seek;
                    it.metadata_changed(emitter).await?;
                    if can_play_changed {
                        it.can_play_changed(emitter).await?;
                        it.can_pause_changed(emitter).await?;
                    }
                    if can_seek_changed {
                        it.can_seek_changed(emitter).await?;
                    }
                }
                MprisUpdate::Stopped => {
                    it.status = PlaybackStatus::Stopped;
                    it.can_play = false;
                    it.can_seek = false;
                    it.last_position_us = 0;
                    it.position_anchor = Instant::now();
                    it.title = None;
                    it.artist = None;
                    it.length_us = 0;
                    it.playback_status_changed(emitter).await?;
                    it.can_play_changed(emitter).await?;
                    it.can_pause_changed(emitter).await?;
                    it.can_seek_changed(emitter).await?;
                    it.metadata_changed(emitter).await?;
                }
            }
        }

        if let Some(position) = seek_to {
            iface_ref.seeked(position).await?;
        }
        Ok(())
    }

    /// Register the MPRIS interfaces on the session bus and pump application updates into D-Bus
    /// signals until the app shutdown.
    pub async fn run(
        msg_tx: MessageSender,
        mut rx: UnboundedReceiver<MprisUpdate>,
        receiver_info: Arc<crate::ReceiverInfo>,
    ) {
        let formats = &receiver_info.supported_formats;
        let root = Root {
            mime_types: mime_types_for(formats),
            uri_schemes: uri_schemes_for(formats),
        };

        let conn = match connect(root, Player::new(msg_tx)).await {
            Ok(conn) => conn,
            Err(err) => {
                warn!(?err, "could not register on the session bus, disabled");
                return;
            }
        };
        info!("registered {NAME} at {PATH}");

        let iface_ref = match conn.object_server().interface::<_, Player>(PATH).await {
            Ok(iface_ref) => iface_ref,
            Err(err) => {
                error!(?err, "failed to resolve the Player interface");
                return;
            }
        };

        while let Some(update) = rx.recv().await {
            debug!(?update, "applying update");
            if let Err(err) = apply(&iface_ref, update).await {
                warn!(?err, "failed to apply update");
            }
        }
    }

    async fn connect(root: Root, player: Player) -> zbus::Result<zbus::Connection> {
        let conn = zbus::connection::Builder::session()?
            .serve_at(PATH, root)?
            .serve_at(PATH, player)?
            .build()
            .await?;

        // Prefer the plain name; if another receiver already owns it, fall back to the spec's
        // instance-suffixed form so both coexist.
        if conn.request_name(NAME).await.is_err() {
            let alt = format!("{NAME}.instance{}", std::process::id());
            warn!("{NAME} is taken, requesting {alt}");
            conn.request_name(alt).await?;
        }
        Ok(conn)
    }
}
