use std::{collections::HashMap, sync::Arc};

use gst::{glib, prelude::*};
use parking_lot::Mutex;
use url::Url;

/// One Annex-B H.264 access unit handed from the stream reader to the source.
pub struct AccessUnit {
    /// Annex-B byte-stream (start-code-prefixed NALs).
    pub data: Vec<u8>,
    /// The client's presentation timestamp in nanoseconds (its monotonic clock,
    /// no fixed epoch). Used for relative PTS so playback follows the client's
    /// frame cadence rather than network arrival jitter.
    pub pts_ns: u64,
}

/// One decrypted AAC-ELD audio frame handed from the audio receiver to the
/// source. It is timestamped on arrival by the audio appsrc.
pub struct AudioFrame {
    pub data: Vec<u8>,
}

/// Per-session state: the video/audio channels (each receiver claimed once by
/// the Bin) and the abort handles of the session's background tasks, so the
/// session can be force-ended from either the control-connection handler or the
/// app.
///
/// Both channels are created up front at [`try_register`](AirPlayContext::try_register),
/// even though the audio stream is usually set up later (on demand, when the
/// client starts playing audio). This lets the Bin create its audio pad when it
/// starts - a late audio `SETUP` then just begins feeding the existing pad,
/// avoiding a risky dynamic pad-add on the live pipeline. `audio_tx` is retained
/// so the audio receiver task can be handed a sender whenever it appears.
#[derive(Default)]
struct SessionEntry {
    video_rx: Option<std::sync::mpsc::Receiver<AccessUnit>>,
    audio_rx: Option<std::sync::mpsc::Receiver<AudioFrame>>,
    audio_tx: Option<std::sync::mpsc::Sender<AudioFrame>>,
    aborts: Vec<tokio::task::AbortHandle>,
}

/// Shared registry mapping a mirror session's `streamConnectionID` to its
/// [`SessionEntry`]. Cloneable; the clone shares state.
///
/// The registry also enforces the single-mirror policy: [`try_register`] refuses
/// a second concurrent session, and it exposes [`end_session`] so the app can
/// tear down a session it decides to refuse (e.g. when other media is already
/// playing) by aborting its tasks - which closes the client's data connections.
///
/// [`try_register`]: Self::try_register
/// [`end_session`]: Self::end_session
#[derive(Clone, Default)]
pub struct AirPlayContext {
    sessions: Arc<Mutex<HashMap<u64, SessionEntry>>>,
}

impl std::fmt::Debug for AirPlayContext {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("AirPlayContext")
            .field("sessions", &self.sessions.lock().keys().collect::<Vec<_>>())
            .finish()
    }
}

impl AirPlayContext {
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a new session, creating both the video and audio channels and
    /// returning the sender the stream reader uses to deliver video access units
    /// - or `None` if a *different* mirror session is already active (we serve
    /// one mirror at a time). The receivers are held until the source Bin claims
    /// them via [`take_video`](Self::take_video)/[`take_audio`](Self::take_audio).
    pub fn try_register(
        &self,
        stream_connection_id: u64,
    ) -> Option<std::sync::mpsc::Sender<AccessUnit>> {
        let mut sessions = self.sessions.lock();
        if sessions.keys().any(|&id| id != stream_connection_id) {
            return None;
        }
        let (video_tx, video_rx) = std::sync::mpsc::channel();
        let (audio_tx, audio_rx) = std::sync::mpsc::channel();
        let entry = sessions.entry(stream_connection_id).or_default();
        entry.video_rx = Some(video_rx);
        entry.audio_rx = Some(audio_rx);
        entry.audio_tx = Some(audio_tx);
        Some(video_tx)
    }

    /// A sender for the session's audio channel, handed to the audio receiver
    /// task when the audio stream is set up (possibly long after the video). The
    /// original sender is retained in the session so the audio pad stays alive
    /// until teardown even if audio is never set up.
    pub fn audio_sender(
        &self,
        stream_connection_id: u64,
    ) -> Option<std::sync::mpsc::Sender<AudioFrame>> {
        self.sessions
            .lock()
            .get(&stream_connection_id)
            .and_then(|entry| entry.audio_tx.clone())
    }

    /// Record a background task's abort handle against a session so
    /// [`end_session`](Self::end_session) can stop it.
    pub fn add_abort(&self, stream_connection_id: u64, handle: tokio::task::AbortHandle) {
        if let Some(entry) = self.sessions.lock().get_mut(&stream_connection_id) {
            entry.aborts.push(handle);
        }
    }

    /// End a session: abort its background tasks and free the slot. Idempotent -
    /// a session already gone is a no-op. Called by the control handler on
    /// TEARDOWN/drop and by the app when it refuses a mirror.
    pub fn end_session(&self, stream_connection_id: u64) {
        if let Some(entry) = self.sessions.lock().remove(&stream_connection_id) {
            for handle in entry.aborts {
                handle.abort();
            }
        }
    }

    fn take_video(
        &self,
        stream_connection_id: u64,
    ) -> Option<std::sync::mpsc::Receiver<AccessUnit>> {
        self.sessions
            .lock()
            .get_mut(&stream_connection_id)
            .and_then(|entry| entry.video_rx.take())
    }

    fn take_audio(
        &self,
        stream_connection_id: u64,
    ) -> Option<std::sync::mpsc::Receiver<AudioFrame>> {
        self.sessions
            .lock()
            .get_mut(&stream_connection_id)
            .and_then(|entry| entry.audio_rx.take())
    }
}

/// Build the `airplay://mirror/<id>` URI for a mirror session.
pub fn mirror_uri(stream_connection_id: u64) -> String {
    format!("airplay://mirror/{stream_connection_id}")
}

/// Parse the `streamConnectionID` out of an `airplay://mirror/<id>` URI.
fn parse_uri(url: &Url) -> Option<u64> {
    if url.scheme() != "airplay" {
        return None;
    }
    if url.host_str()? != "mirror" {
        return None;
    }
    url.path().strip_prefix('/')?.parse::<u64>().ok()
}

pub mod imp {
    use std::{
        sync::{
            Arc, LazyLock,
            atomic::{AtomicBool, Ordering},
            mpsc::{Receiver, RecvTimeoutError},
        },
        thread::JoinHandle,
        time::Duration,
    };

    use gst::{glib, prelude::*, subclass::prelude::*};
    use parking_lot::Mutex;
    use url::Url;

    use super::{AccessUnit, AirPlayContext, AudioFrame};

    static CAT: LazyLock<gst::DebugCategory> = LazyLock::new(|| {
        gst::DebugCategory::new(
            "airplaysrc",
            gst::DebugColorFlags::empty(),
            Some("AirPlay mirror source"),
        )
    });

    /// GStreamer context type used to hand the [`AirPlayContext`] to the element.
    pub const AIRPLAY_CONTEXT: &str = "fcast.airplay.context";

    #[derive(Clone, glib::Boxed)]
    #[boxed_type(name = "FCastAirPlayContext")]
    pub struct BoxedAirPlayContext(pub AirPlayContext);

    fn video_caps() -> gst::Caps {
        gst::Caps::builder("video/x-h264")
            .field("stream-format", "byte-stream")
            .field("alignment", "au")
            .build()
    }

    /// AudioSpecificConfig for AAC-ELD 44100 Hz / 2ch (`audio_renderer.c`).
    const AAC_ELD_CODEC_DATA: [u8; 4] = [0xf8, 0xe8, 0x50, 0x00];

    fn audio_caps() -> gst::Caps {
        let codec_data = gst::Buffer::from_slice(AAC_ELD_CODEC_DATA);
        gst::Caps::builder("audio/mpeg")
            .field("mpegversion", 4i32)
            .field("channels", 2i32)
            .field("rate", 44100i32)
            .field("stream-format", "raw")
            .field("codec_data", codec_data)
            .build()
    }

    #[derive(Default)]
    pub struct AirPlaySrc {
        context: Mutex<Option<AirPlayContext>>,
        /// `streamConnectionID` parsed from the URI.
        session_id: Mutex<Option<u64>>,
        url: Mutex<Option<Url>>,
        video_appsrc: Mutex<Option<gst_app::AppSrc>>,
        video_rx: Mutex<Option<Receiver<AccessUnit>>>,
        audio_appsrc: Mutex<Option<gst_app::AppSrc>>,
        audio_rx: Mutex<Option<Receiver<AudioFrame>>>,
        /// `(running_time, remote_ns)` captured on the first video access unit,
        /// mapping the client clock onto the live running-time timeline.
        base: Mutex<Option<(gst::ClockTime, u64)>>,
        /// Signals the pusher threads to exit.
        stop: Arc<AtomicBool>,
        threads: Mutex<Vec<JoinHandle<()>>>,
    }

    impl AirPlaySrc {
        fn current_context(&self) -> Option<AirPlayContext> {
            self.context.lock().clone()
        }

        fn ensure_context(&self) -> Result<AirPlayContext, gst::ErrorMessage> {
            if let Some(ctx) = self.current_context() {
                return Ok(ctx);
            }
            // The playbin bus uses a *sync* handler, so `set_context` runs
            // synchronously inside this post - the context is available right
            // after (see `crate::player`).
            let _ = self.obj().post_message(
                gst::message::NeedContext::builder(AIRPLAY_CONTEXT)
                    .src(&*self.obj())
                    .build(),
            );
            self.current_context().ok_or_else(|| {
                gst::error_msg!(gst::ResourceError::Failed, ["Missing AirPlay context"])
            })
        }

        /// Map a client (remote) timestamp in nanoseconds onto the pipeline's
        /// running-time PTS. The first access unit anchors the client clock to
        /// the current running time; later frames are offset by their delta from
        /// that anchor. Returns `None` (leave the buffer unstamped) if the
        /// element has no clock yet.
        fn compute_pts(&self, remote_ns: u64) -> Option<gst::ClockTime> {
            let mut base = self.base.lock();
            let (base_running, base_remote) = match *base {
                Some(pair) => pair,
                None => {
                    let running = self.obj().current_running_time()?;
                    *base = Some((running, remote_ns));
                    (running, remote_ns)
                }
            };
            let delta = remote_ns.saturating_sub(base_remote);
            Some(base_running + gst::ClockTime::from_nseconds(delta))
        }

        /// Build the appsrcs and ghost pads for the claimed session. Runs while
        /// the Bin transitions `Null -> Ready`, so the pads exist before
        /// `playbin3` links its decoders.
        fn prepare(&self) -> Result<(), gst::ErrorMessage> {
            let session_id = self.session_id.lock().ok_or_else(|| {
                gst::error_msg!(gst::ResourceError::Settings, ["No AirPlay URI set"])
            })?;
            let context = self.ensure_context()?;
            *self.base.lock() = None;
            self.stop.store(false, Ordering::SeqCst);

            let video_rx = context.take_video(session_id).ok_or_else(|| {
                gst::error_msg!(
                    gst::ResourceError::NotFound,
                    ["No mirror session {session_id} registered"]
                )
            })?;
            let video_appsrc = gst_app::AppSrc::builder()
                .name("airplay_video_src")
                .stream_type(gst_app::AppStreamType::Stream)
                .is_live(true)
                .format(gst::Format::Time)
                // PTS is set explicitly from the client's timestamps.
                .do_timestamp(false)
                .caps(&video_caps())
                .build();
            self.add_appsrc(&video_appsrc, "video")?;
            *self.video_appsrc.lock() = Some(video_appsrc);
            *self.video_rx.lock() = Some(video_rx);

            if let Some(audio_rx) = context.take_audio(session_id) {
                let audio_appsrc = gst_app::AppSrc::builder()
                    .name("airplay_audio_src")
                    .stream_type(gst_app::AppStreamType::Stream)
                    .is_live(true)
                    .format(gst::Format::Time)
                    // Timestamp AAC frames on arrival; both streams share the
                    // one pipeline clock, so this keeps rough A/V alignment.
                    .do_timestamp(true)
                    .caps(&audio_caps())
                    .build();
                self.add_appsrc(&audio_appsrc, "audio")?;
                *self.audio_appsrc.lock() = Some(audio_appsrc);
                *self.audio_rx.lock() = Some(audio_rx);
                gst::debug!(
                    CAT,
                    imp = self,
                    "mirror session {session_id} audio pad ready"
                );
            }

            gst::debug!(CAT, imp = self, "prepared mirror session {session_id}");
            Ok(())
        }

        /// Add an appsrc to the Bin and expose its src pad as a ghost pad named
        /// after the `pad_name` template (both appsrcs' own pads are named
        /// "src", so the ghost pads must be given distinct names).
        fn add_appsrc(
            &self,
            appsrc: &gst_app::AppSrc,
            pad_name: &str,
        ) -> Result<(), gst::ErrorMessage> {
            let bin = self.obj();
            bin.add(appsrc)
                .map_err(|_| gst::error_msg!(gst::CoreError::Failed, ["failed to add appsrc"]))?;
            let src_pad = appsrc
                .static_pad("src")
                .ok_or_else(|| gst::error_msg!(gst::CoreError::Pad, ["appsrc has no src pad"]))?;
            let templ = bin.pad_template(pad_name).ok_or_else(|| {
                gst::error_msg!(gst::CoreError::Pad, ["no pad template {pad_name}"])
            })?;
            // The template names are non-wildcard ("video"/"audio"), so the
            // ghost pad is named after them - unique within the Bin.
            let ghost =
                gst::GhostPad::from_template_with_target(&templ, &src_pad).map_err(|_| {
                    gst::error_msg!(gst::CoreError::Pad, ["failed to create ghost pad"])
                })?;
            ghost.set_active(true).ok();
            bin.add_pad(&ghost)
                .map_err(|_| gst::error_msg!(gst::CoreError::Pad, ["failed to add ghost pad"]))?;
            Ok(())
        }

        /// Start the pusher threads that drain the channels into the appsrcs.
        fn start_pushers(&self) {
            let mut threads = self.threads.lock();
            if !threads.is_empty() {
                return;
            }

            if let (Some(appsrc), Some(rx)) = (
                self.video_appsrc.lock().clone(),
                self.video_rx.lock().take(),
            ) {
                let stop = self.stop.clone();
                let elem = self.obj().clone();
                threads.push(std::thread::spawn(move || {
                    video_pusher(&elem, &appsrc, rx, &stop);
                }));
            }
            if let (Some(appsrc), Some(rx)) = (
                self.audio_appsrc.lock().clone(),
                self.audio_rx.lock().take(),
            ) {
                let stop = self.stop.clone();
                threads.push(std::thread::spawn(move || {
                    audio_pusher(&appsrc, rx, &stop);
                }));
            }
        }

        /// Stop the pusher threads and wait for them to exit.
        fn stop_pushers(&self) {
            self.stop.store(true, Ordering::SeqCst);
            let handles: Vec<JoinHandle<()>> = self.threads.lock().drain(..).collect();
            for handle in handles {
                let _ = handle.join();
            }
        }
    }

    /// Drain video access units into the video appsrc, stamping PTS from the
    /// client clock, until the channel closes or we are told to stop.
    fn video_pusher(
        elem: &super::AirPlaySrc,
        appsrc: &gst_app::AppSrc,
        rx: Receiver<AccessUnit>,
        stop: &Arc<AtomicBool>,
    ) {
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(au) => {
                    let mut buffer = gst::Buffer::from_slice(au.data);
                    if let Some(pts) = elem.imp().compute_pts(au.pts_ns) {
                        buffer.get_mut().unwrap().set_pts(pts);
                    }
                    if appsrc.push_buffer(buffer).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = appsrc.end_of_stream();
                    return;
                }
            }
        }
    }

    /// Drain AAC-ELD frames into the audio appsrc (arrival-timestamped) until the
    /// channel closes or we are told to stop.
    fn audio_pusher(appsrc: &gst_app::AppSrc, rx: Receiver<AudioFrame>, stop: &Arc<AtomicBool>) {
        loop {
            if stop.load(Ordering::SeqCst) {
                return;
            }
            match rx.recv_timeout(Duration::from_millis(200)) {
                Ok(frame) => {
                    let buffer = gst::Buffer::from_slice(frame.data);
                    if appsrc.push_buffer(buffer).is_err() {
                        return;
                    }
                }
                Err(RecvTimeoutError::Timeout) => continue,
                Err(RecvTimeoutError::Disconnected) => {
                    let _ = appsrc.end_of_stream();
                    return;
                }
            }
        }
    }

    impl ObjectImpl for AirPlaySrc {}

    impl GstObjectImpl for AirPlaySrc {}

    impl ElementImpl for AirPlaySrc {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: LazyLock<gst::subclass::ElementMetadata> = LazyLock::new(|| {
                gst::subclass::ElementMetadata::new(
                    "AirPlay mirror source",
                    "Source/Network",
                    "Receive mirrored H.264 video and AAC-ELD audio from an AirPlay session",
                    "Marcus Hanestad <marcus@futo.org>",
                )
            });
            Some(&*METADATA)
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: LazyLock<Vec<gst::PadTemplate>> = LazyLock::new(|| {
                // Both pads are created dynamically once the session's streams
                // are known, so they are `Sometimes`.
                let video = gst::PadTemplate::new(
                    "video",
                    gst::PadDirection::Src,
                    gst::PadPresence::Sometimes,
                    &video_caps(),
                )
                .unwrap();
                let audio = gst::PadTemplate::new(
                    "audio",
                    gst::PadDirection::Src,
                    gst::PadPresence::Sometimes,
                    &audio_caps(),
                )
                .unwrap();
                vec![video, audio]
            });
            TEMPLATES.as_ref()
        }

        fn set_context(&self, context: &gst::Context) {
            if context.context_type() == AIRPLAY_CONTEXT
                && let Ok(ctx) = context.structure().get::<&BoxedAirPlayContext>("context")
            {
                *self.context.lock() = Some(ctx.0.clone());
            }
            self.parent_set_context(context);
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            // Build appsrcs/pads before the children transition upward.
            if transition == gst::StateChange::NullToReady {
                self.prepare().map_err(|err| {
                    gst::error!(CAT, imp = self, "prepare failed: {err}");
                    gst::StateChangeError
                })?;
            }

            let success = self.parent_change_state(transition)?;

            match transition {
                // appsrcs are live in PAUSED; start pumping once the children
                // have reached it.
                gst::StateChange::ReadyToPaused => self.start_pushers(),
                gst::StateChange::PausedToReady => self.stop_pushers(),
                _ => {}
            }

            Ok(success)
        }
    }

    impl BinImpl for AirPlaySrc {}

    impl URIHandlerImpl for AirPlaySrc {
        const URI_TYPE: gst::URIType = gst::URIType::Src;

        fn protocols() -> &'static [&'static str] {
            &["airplay"]
        }

        fn uri(&self) -> Option<String> {
            self.url.lock().as_ref().map(Url::to_string)
        }

        fn set_uri(&self, uri: &str) -> Result<(), glib::Error> {
            let url = Url::parse(uri).map_err(|err| {
                glib::Error::new(gst::URIError::BadUri, &format!("bad URI {uri}: {err}"))
            })?;
            let session_id = super::parse_uri(&url).ok_or_else(|| {
                glib::Error::new(gst::URIError::BadUri, "invalid AirPlay mirror URI")
            })?;
            *self.session_id.lock() = Some(session_id);
            *self.url.lock() = Some(url);
            Ok(())
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for AirPlaySrc {
        const NAME: &'static str = "FCastAirPlaySrc";
        type Type = super::AirPlaySrc;
        type ParentType = gst::Bin;
        type Interfaces = (gst::URIHandler,);
    }
}

glib::wrapper! {
    pub struct AirPlaySrc(ObjectSubclass<imp::AirPlaySrc>)
        @extends gst::Bin, gst::Element, gst::Object,
        @implements gst::URIHandler, gst::ChildProxy;
}

pub fn plugin_init() -> Result<(), glib::BoolError> {
    gst::Element::register(
        None,
        "airplaysrc",
        gst::Rank::PRIMARY,
        AirPlaySrc::static_type(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_mirror_uri() {
        let url = Url::parse("airplay://mirror/441233617212555566").unwrap();
        assert_eq!(parse_uri(&url), Some(441233617212555566));
    }

    #[test]
    fn rejects_foreign_uri() {
        assert_eq!(parse_uri(&Url::parse("fcomp://0.fcast/0").unwrap()), None);
        assert_eq!(parse_uri(&Url::parse("airplay://other/1").unwrap()), None);
        assert_eq!(parse_uri(&Url::parse("airplay://mirror/x").unwrap()), None);
    }

    #[test]
    fn roundtrips_uri() {
        assert_eq!(parse_uri(&Url::parse(&mirror_uri(42)).unwrap()), Some(42));
    }

    #[test]
    fn context_register_video_and_audio() {
        let ctx = AirPlayContext::new();
        let vtx = ctx.try_register(7).expect("first session registers");
        vtx.send(AccessUnit {
            data: vec![1, 2, 3],
            pts_ns: 42,
        })
        .unwrap();
        // The audio channel exists from registration; a late audio stream just
        // grabs a sender for it.
        let atx = ctx.audio_sender(7).expect("audio channel exists");
        atx.send(AudioFrame { data: vec![9, 9] }).unwrap();

        let vrx = ctx.take_video(7).expect("video channel present");
        assert_eq!(vrx.recv().unwrap().data, vec![1, 2, 3]);
        let arx = ctx.take_audio(7).expect("audio channel present");
        assert_eq!(arx.recv().unwrap().data, vec![9, 9]);
        assert!(ctx.take_video(7).is_none(), "video channel consumed once");
    }

    #[test]
    fn context_refuses_second_concurrent_session() {
        let ctx = AirPlayContext::new();
        let _tx = ctx.try_register(7).expect("first session registers");
        assert!(
            ctx.try_register(8).is_none(),
            "a second, different session must be refused"
        );
        // Audio senders are only available for a registered session.
        assert!(ctx.audio_sender(8).is_none());
        assert!(ctx.audio_sender(7).is_some());

        // After the session ends, a new one can start.
        ctx.end_session(7);
        assert!(
            ctx.try_register(8).is_some(),
            "slot freed after end_session"
        );
    }
}
