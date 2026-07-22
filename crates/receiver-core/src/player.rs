use anyhow::Result;
use fcast_protocol::PlaybackState;
use gst::{glib::object::ObjectExt, prelude::*};
use tracing::{debug, error, info, instrument, warn};

use crate::MessageSender;
use fcastplaybin::state_machine::{
    BufferingStateResult, RunningState, Seek, StateChangeResult, StateMachine,
};

/// What plays. Re-exported from `fcastplaybin`: a URI, or a pre-built source
/// element. The APPLICATION builds the element (HTTP with per-load headers,
/// WHEP bin, fwebrtc, AirPlay mirror) rather than the playbin resolving a
/// URI scheme itself: those sources are receiver elements wired to receiver
/// state (signalling channels, mirror sessions, GStreamer contexts), which
/// fcastplaybin deliberately knows nothing about: no fake-URI dispatch, no
/// global config side channels.
pub use fcastplaybin::MediaInput;

/// The playback snapshot a load returns to once it prerolls (the start
/// position/rate seek `fcastplaybin::load` applies in PAUSED).
#[derive(Debug, Clone, Copy)]
pub struct RestorePoint {
    pub position: gst::ClockTime,
    pub rate: f32,
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum PlayerState {
    Paused,
    Playing,
    Buffering,
    Stopped,
}

impl PlayerState {
    pub fn as_fcast_v4(&self) -> fcast_protocol::v4::PlaybackState {
        use fcast_protocol::v4;
        match self {
            PlayerState::Paused => v4::PlaybackState::Paused,
            PlayerState::Playing => v4::PlaybackState::Playing,
            PlayerState::Buffering => v4::PlaybackState::Buffering,
            PlayerState::Stopped => v4::PlaybackState::Idle,
        }
    }
}

pub type StreamId = String;

/// Which stream slot a track-change request targets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrackKind {
    Video,
    Audio,
    Subtitle,
}

/// A full track selection, keyed by GStreamer stream id (`None` = slot
/// disabled). Stream ids are stable across collections of the same load,
/// unlike stream-list indices, so the selection never needs remapping when
/// a new collection arrives; indices exist only at the protocol/GUI edge.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TrackSelection {
    pub video: Option<StreamId>,
    pub audio: Option<StreamId>,
    pub subtitle: Option<StreamId>,
}

/// What `TrackOps::pump` decided to dispatch next.
#[derive(Debug, Clone, PartialEq, Eq)]
enum TrackOpCommand {
    SelectStreams(TrackSelection),
    RefreshSeek,
}

/// Pipeline conditions `TrackOps::pump` dispatches under (a snapshot taken by
/// `Player::pump_track_ops`).
#[derive(Debug, Clone)]
struct TrackOpCtx {
    /// No async state change in progress and the state machine is settled in
    /// `Running` (not buffering/seeking/changing).
    quiet: bool,
    /// Settled in `Running { Paused }`: the streaming threads are parked after
    /// preroll, so a dispatched selection won't apply (or confirm) until data
    /// flows again.
    paused: bool,
    /// The selection currently applied (or optimistically in flight).
    applied: TrackSelection,
}

/// Serialized track selection and subtitle refresh.
///
/// A `SELECT_STREAMS` is confirmed by a `STREAMS_SELECTED` message carrying
/// the event's seqnum (decodebin3 stamps it), so selections settle by exact
/// seqnum match. A refresh seek completes with a top-level `ASYNC_DONE`,
/// which CANNOT be seqnum-matched (`GstBin` aggregates with a fresh seqnum),
/// so the refresh settles by exclusivity: at most one async-causing
/// operation is in flight, making the next ASYNC_DONE its completion. New
/// work is held back until the pipeline is quiet, because overlapping
/// re-prerolls deadlock the pipeline, the failure mode all of this prevents.
///
/// Requests are latest-wins: while an operation is in flight only the newest
/// composed selection is remembered (`pending`) and dispatched at settle.
///
/// Paused is special (streaming threads are parked after preroll): a
/// dispatched selection won't confirm until data flows, so a parked
/// selection neither blocks a superseding one (no re-preroll to overlap
/// with) nor blocks the refresh flush, which is exactly what makes data
/// flow and the selection apply.
///
/// Deliberately receiver-side rather than inside fcastplaybin: it composes
/// selections from the receiver's index-based track bookkeeping
/// (`current_*`), which is protocol state.
#[derive(Debug)]
struct TrackOps {
    /// Latest desired selection not yet dispatched.
    pending: Option<TrackSelection>,
    /// In-flight `SELECT_STREAMS`: the seqnum its `STREAMS_SELECTED` will
    /// carry and the selection we asked decodebin3 to apply. Settles on an
    /// exact seqnum match OR on a `STREAMS_SELECTED` reporting exactly this
    /// selection (a superseded/coalesced/no-op selection confirms under a
    /// different seqnum, so the content match keeps confirmation
    /// deterministic). A `STREAMS_SELECTED` matching NEITHER means the
    /// request was overtaken by a selection decodebin3 made on its own (its
    /// collection-default auto-select racing ours), so it is re-queued for
    /// re-dispatch. No timeout: a slow selection stays in flight until its
    /// confirmation arrives and the pump holds new work off until then.
    selecting: Option<(gst::Seqnum, TrackSelection)>,
    /// Dispatches superseded before confirming (the paused supersede path).
    /// Their late confirmations are our own stale echoes, recognized here by
    /// seqnum or content so they neither settle the live request nor
    /// masquerade as an overtaking foreign selection. Cleared on settle.
    superseded: Vec<(gst::Seqnum, TrackSelection)>,
    /// In-flight subtitle refresh seek. Settled by the next `ASYNC_DONE`
    /// (attribution by exclusivity, see the struct docs). The seqnum only
    /// matches the job's failure report and the logs.
    refreshing: Option<gst::Seqnum>,
    /// A re-emit flush is due once the pipeline settles: a sparse text track
    /// doesn't render its current cue after a switch until the next cue
    /// boundary, so a flushing seek to the current position re-emits it.
    refresh_wanted: bool,
    /// The latest request forbade its re-emit flush (an external subtitle is
    /// attached, and any flush races the external inputs' reconfiguration
    /// and can freeze the play item). Re-decided by every request (see
    /// `suppress_refresh`), cleared by `reset`.
    refresh_suppressed: bool,
}

impl TrackOps {
    fn new() -> Self {
        Self {
            pending: None,
            selecting: None,
            superseded: Vec::new(),
            refreshing: None,
            refresh_wanted: false,
            refresh_suppressed: false,
        }
    }

    fn reset(&mut self) {
        *self = Self::new();
    }

    /// A new stream collection arrived: an in-flight confirmation dispatched
    /// against the old one may never confirm (decodebin3 is on the new
    /// collection). Abandon those waits deterministically. This is exactly
    /// what the (removed) selection watchdog used to do after a 5 s timeout.
    /// The applied selection itself is stream-id-keyed and stays valid.
    fn invalidate_in_flight(&mut self) {
        self.selecting = None;
        self.superseded.clear();
        self.refreshing = None;
    }

    /// Compose a single-slot change onto the latest desired selection.
    fn request(&mut self, kind: TrackKind, sid: Option<StreamId>, applied: TrackSelection) {
        let mut desired = self.pending.take().unwrap_or(applied);
        match kind {
            TrackKind::Video => desired.video = sid,
            TrackKind::Audio => desired.audio = sid,
            TrackKind::Subtitle => desired.subtitle = sid,
        }
        // Each request re-decides whether its flush is allowed. The caller
        // re-applies `suppress_refresh` (external suburi attached) after this
        // call. Reset for every kind so an audio/video switch's flush is gated
        // by the current suburi state, not a stale subtitle request's.
        self.refresh_suppressed = false;
        self.pending = Some(desired);
    }

    /// Forbid the re-emit flush for the subtitle selection just composed by
    /// `request` (external suburi attached, see `refresh_suppressed`).
    fn suppress_refresh(&mut self) {
        self.refresh_suppressed = true;
    }

    /// Decide the next operation to dispatch, if the pipeline allows one.
    fn pump(&mut self, ctx: &TrackOpCtx) -> Option<TrackOpCommand> {
        if !ctx.quiet {
            return None;
        }
        // A refresh flush is an async re-preroll. Never dispatch on top of it.
        if self.refreshing.is_some() {
            return None;
        }
        // An unconfirmed selection blocks new work while data flows (its
        // playsink reconfigure may still be about to re-preroll). While paused
        // it is merely parked: superseding it, or flushing past it, is safe --
        // nothing is re-prerolling.
        if self.selecting.is_some() && !ctx.paused {
            return None;
        }

        if let Some(desired) = self.pending.take()
            && desired != ctx.applied
        {
            // A flushing seek to the current position drops the
            // deeply-buffered old track (decoded audio piled up in
            // fpb-aqueue, video frames still carrying the old subtitle's
            // overlay meta) so a switch takes effect immediately instead of
            // after that buffer drains. Scheduled only for a switch TO a
            // real audio/subtitle track, and never when:
            //   * an external subtitle is attached (any flush races the
            //     external inputs' reconfiguration and can freeze the item)
            //   * any slot is being DISABLED (Some -> None): flushing across
            //     a sink/branch teardown wedges (audio-off drops the
            //     pipeline clock, video-off freezes the audio clock,
            //     subtitle-off fails vaapi renegotiation) and there is no
            //     incoming track to make immediate anyway.
            // Video switches keep the pre-existing no-flush behaviour (rare,
            // and a flush re-prerolls the whole video chain).
            let switching_to_track = (desired.subtitle != ctx.applied.subtitle
                && desired.subtitle.is_some())
                || (desired.audio != ctx.applied.audio && desired.audio.is_some());
            let disabling = (ctx.applied.audio.is_some() && desired.audio.is_none())
                || (ctx.applied.video.is_some() && desired.video.is_none())
                || (ctx.applied.subtitle.is_some() && desired.subtitle.is_none());
            self.refresh_wanted = switching_to_track && !disabling && !self.refresh_suppressed;
            return Some(TrackOpCommand::SelectStreams(desired));
        }

        if self.refresh_wanted {
            self.refresh_wanted = false;
            return Some(TrackOpCommand::RefreshSeek);
        }

        None
    }

    fn selection_dispatched(&mut self, seqnum: gst::Seqnum, desired: TrackSelection) {
        if let Some(old) = self.selecting.replace((seqnum, desired)) {
            self.superseded.push(old);
        }
    }

    fn refresh_dispatched(&mut self, seqnum: gst::Seqnum) {
        self.refreshing = Some(seqnum);
    }

    /// A `STREAMS_SELECTED` arrived reporting `applied` as the now-active
    /// selection. Settles the in-flight selection when it is ours: by the
    /// SELECT_STREAMS seqnum decodebin3 stamps on, or, when that seqnum was
    /// lost to superseding/coalescing/a no-op, by the reported selection
    /// matching what we asked for.
    ///
    /// Matching neither means the request was overtaken: decodebin3 applied
    /// a selection of its own on top of ours (its collection-default
    /// auto-select computed for a fresh collection lands AFTER a
    /// SELECT_STREAMS sent against that collection and stomps it). Waiting
    /// would deadlock the queue forever (there is no timeout), so re-queue
    /// the overtaken request for re-dispatch. A newer pending request
    /// supersedes it instead (latest wins). Converges: each re-dispatch
    /// needs a fresh non-matching `STREAMS_SELECTED` to fire again, and
    /// decodebin3 auto-selects at most once per collection.
    fn streams_selected(&mut self, seqnum: gst::Seqnum, applied: &TrackSelection) {
        let Some((expected, desired)) = self.selecting.take() else {
            return;
        };
        if expected == seqnum || &desired == applied {
            self.superseded.clear();
            return;
        }
        if self
            .superseded
            .iter()
            .any(|(sn, sel)| *sn == seqnum || sel == applied)
        {
            // A superseded dispatch's late confirmation: ours but stale, the
            // live request's own confirmation is still en route.
            self.selecting = Some((expected, desired));
            return;
        }
        debug!(?desired, ?applied, "selection overtaken, re-dispatching");
        if self.pending.is_none() {
            self.pending = Some(desired);
        }
    }

    /// A top-level `ASYNC_DONE` arrived. Returns whether it finished our
    /// refresh seek. Attribution is by exclusivity, not seqnum: `GstBin` posts
    /// its aggregated ASYNC_DONE with a fresh seqnum, and this queue never has
    /// more than one async-causing operation out.
    fn refresh_done(&mut self) -> bool {
        self.refreshing.take().is_some()
    }

    fn refresh_failed(&mut self, seqnum: gst::Seqnum) {
        if self.refreshing == Some(seqnum) {
            self.refreshing = None;
        }
    }

    /// A user-initiated flushing seek re-emits the current cue by itself, so
    /// a separately queued refresh flush would be redundant.
    fn cancel_refresh(&mut self) {
        self.refresh_wanted = false;
    }

    fn has_dispatchable_work(&self) -> bool {
        self.pending.is_some() || self.refresh_wanted
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MediaErrorKind {
    NotFound,
    NotAuthorized,
    UnsupportedFormat,
    Other,
}

impl MediaErrorKind {
    fn from_glib_error(err: &gst::glib::Error) -> Self {
        if let Some(err) = err.kind::<gst::ResourceError>() {
            match err {
                gst::ResourceError::NotFound => Self::NotFound,
                gst::ResourceError::NotAuthorized => Self::NotAuthorized,
                _ => Self::Other,
            }
        } else if let Some(err) = err.kind::<gst::StreamError>() {
            match err {
                gst::StreamError::TypeNotFound
                | gst::StreamError::WrongType
                | gst::StreamError::CodecNotFound
                | gst::StreamError::Decode
                | gst::StreamError::Demux
                | gst::StreamError::Format => Self::UnsupportedFormat,
                _ => Self::Other,
            }
        } else {
            Self::Other
        }
    }
}

/// Receiver-facing playback events, forwarded into the application loop.
/// The raw GStreamer bus lives inside `fcastplaybin` now: it translates
/// messages into typed [`fcastplaybin::PlaybinEvent`]s on the posting
/// thread, and [`Player`] maps those onto this protocol-facing enum (see
/// `relay_event`).
#[derive(Debug)]
pub enum PlayerEvent {
    EndOfStream,
    UriLoaded,
    Tags(gst::TagList),
    VolumeChanged(f64),
    /// User must call Player::handle_stream_collection()
    StreamCollection(gst::StreamCollection),
    /// An async state change or (flushing) seek finished prerolling. Not
    /// attributable to a specific operation: `GstBin` posts its aggregated
    /// ASYNC_DONE with a fresh seqnum (`TrackOps` relies on exclusivity
    /// instead).
    AsyncDone,
    Buffering(i32),
    IsLive,
    StateChanged {
        old: gst::State,
        current: gst::State,
        pending: gst::State,
    },
    /// An element asked the application to change the pipeline state.
    RequestState(gst::State),
    QueueSeek(Seek),
    StreamsSelected {
        video: Option<StreamId>,
        audio: Option<StreamId>,
        subtitle: Option<StreamId>,
        /// Seqnum of the `SELECT_STREAMS` event this confirms (decodebin3
        /// stamps it onto the message).
        seqnum: gst::Seqnum,
    },
    /// A subtitle refresh seek could not be performed.
    SubtitleRefreshFailed {
        seqnum: gst::Seqnum,
    },
    RateChanged(f64),
    SeekFailed,
    /// The element providing the pipeline clock went away (e.g. the audio
    /// sink after the audio track was deselected). User must call
    /// `Player::recover_clock()`.
    ClockLost,
    Error {
        /// Which input the error came from (fcastplaybin's generation-tagged
        /// attribution); decides external-subtitle handling vs fatal.
        origin: fcastplaybin::ErrorOrigin,
        kind: MediaErrorKind,
        message: String,
        /// Diagnostic only (the failing source's URI, when it has one).
        failed_uri: Option<String>,
    },
    Warning(String),
    StreamTagsUpdated,
}

pub fn stream_title(stream: &gst::Stream) -> String {
    let mut res = String::new();
    if let Some(tags) = stream.tags() {
        if let Some(language) = tags.get::<gst::tags::LanguageName>() {
            res += language.get();
        } else if let Some(language) = tags.get::<gst::tags::LanguageCode>() {
            let code = language.get();
            if let Some(lang) = gst_tag::language_codes::language_name(code) {
                res += lang;
            } else {
                res += code;
            }
        }
        if let Some(title) = tags.get::<gst::tags::Title>() {
            let title = title.get();
            if !title.is_empty() {
                if !res.is_empty() {
                    res += " - ";
                }
                res += title;
            }
        }
    }

    if res.is_empty() {
        res += "Unknown";
    }

    res
}

pub struct Stream {
    pub inner: gst::Stream,
    pub title: String,
}

pub struct Player {
    /// The fcastplaybin playback orchestrator (see fcastplaybin-plan.md):
    /// the only pipeline handle. State changes, seeks, queries and events
    /// all go through its API.
    fcast: fcastplaybin::FcastPlaybin,
    /// A volume change was dispatched and its `VolumeChanged` confirmation
    /// has not arrived yet (see `set_volume`).
    volume_confirm_in_flight: bool,
    msg_tx: MessageSender,
    /// The transport state the user last asked for, committed by
    /// `uri_loaded` once a load prerolls. Requests landing mid-load are
    /// recorded here instead of being stomped by the load's own climb, so
    /// there is exactly ONE post-load transport driver.
    desired_transport: RunningState,
    /// The generation of the load this player currently expects events for
    /// (returned by `fcastplaybin::load_async`); `None` when stopped. The
    /// application drops load-scoped events from any other generation.
    expected_generation: Option<u64>,
    pub streams: Vec<Stream>,
    /// The applied (or optimistically in-flight) selection, keyed by stream
    /// id. Never index-based: indices exist only at the protocol/GUI edge.
    selected: TrackSelection,
    pub seekable: bool,
    /// Whether `seekable` reflects an actual answer from the pipeline. The
    /// seeking query only succeeds around preroll completion, well after
    /// tracks are first advertised. Until then `seekable == false` merely
    /// means "not known yet".
    pub seekable_known: bool,
    /// The newest volume requested while a previous change's confirmation
    /// was still in flight, applied when it arrives (see `set_volume`).
    pending_volume: Option<f32>,
    state_machine: StateMachine,
    track_ops: TrackOps,
    stream_collection: Option<gst::StreamCollection>,
    stream_collection_notify: Option<gst::glib::SignalHandlerId>,
}

impl Player {
    pub fn new(
        video_sink: Option<gst::Element>,
        msg_tx: MessageSender,
        fcomp_context: crate::fcompsrc::imp::CompContext,
        #[cfg(feature = "airplay")] airplay_context: crate::airplay::AirPlayContext,
    ) -> Result<Self> {
        // The fcastplaybin orchestrator owns the pipeline, its bus and its
        // worker thread, this constructor only wires the receiver-specific
        // pieces onto its API.
        //
        // Audio: the native PipeWire sink on Linux when a daemon is
        // reachable (see pwaudiosink.rs for why), autoaudiosink otherwise.
        // FCAST_NO_PW_AUDIO=1 forces the fallback for A/B comparisons.
        #[cfg(target_os = "linux")]
        let audio = if std::env::var("FCAST_NO_PW_AUDIO").is_ok_and(|v| v == "1")
            || !crate::pwaudiosink::is_available()
        {
            info!("audio sink: autoaudiosink (PipeWire disabled or unreachable)");
            fcastplaybin::AudioSink::Auto
        } else {
            info!("audio sink: native PipeWire (fcastpwaudiosink)");
            fcastplaybin::AudioSink::Factory(Box::new(|| {
                use anyhow::Context;
                gst::ElementFactory::make("fcastpwaudiosink")
                    .build()
                    .context("creating fcastpwaudiosink")
            }))
        };
        #[cfg(not(target_os = "linux"))]
        let audio = fcastplaybin::AudioSink::Auto;

        let fcast = fcastplaybin::FcastPlaybin::new(fcastplaybin::Sinks {
            video: video_sink,
            audio,
        })?;

        // Raw-message hook: bus traffic only the receiver understands
        // (context requests from its custom source elements, missing-plugin
        // reports). Runs on the posting (streaming) thread.
        let hook: fcastplaybin::MessageHook = Box::new(move |msg| {
            use gst::MessageView;
            match msg.view() {
                MessageView::NeedContext(ctx) => {
                    let typ = ctx.context_type();
                    debug!(typ, "Need context");
                    if let Some(element) = msg
                        .src()
                        .and_then(|source| source.downcast_ref::<gst::Element>())
                    {
                        if typ == crate::fcompsrc::imp::FCOMP_CONTEXT {
                            let mut ctx = gst::Context::new(typ, true);
                            {
                                let ctx = ctx.get_mut().unwrap();
                                let s = ctx.structure_mut();
                                s.set("context", &fcomp_context);
                            }
                            element.set_context(&ctx);
                        }
                        #[cfg(feature = "airplay")]
                        if typ == crate::airplay::source::imp::AIRPLAY_CONTEXT {
                            let mut ctx = gst::Context::new(typ, true);
                            {
                                let ctx = ctx.get_mut().unwrap();
                                let s = ctx.structure_mut();
                                s.set(
                                    "context",
                                    crate::airplay::source::imp::BoxedAirPlayContext(
                                        airplay_context.clone(),
                                    ),
                                );
                            }
                            element.set_context(&ctx);
                        }
                    }
                    true
                }
                MessageView::Element(_) => {
                    if let Ok(msg) = gst_pbutils::MissingPluginMessage::parse(msg) {
                        error!(detail = %msg.installer_detail(), desc = %msg.description(), "GStreamer missing plugin");
                    }
                    true
                }
                _ => false,
            }
        });

        // Everything else arrives as typed events (bus translation and
        // worker feedback alike), mapped onto the protocol-facing
        // `PlayerEvent` and forwarded into the application loop.
        let event_tx = msg_tx.clone();
        fcast.set_event_handler(Some(hook), move |event, generation| {
            Self::relay_event(&event_tx, event, generation);
        });

        fcast.set_state_async(gst::State::Ready);

        Ok(Self {
            fcast,
            volume_confirm_in_flight: false,
            msg_tx,
            desired_transport: RunningState::Playing,
            expected_generation: None,
            selected: TrackSelection::default(),
            seekable: false,
            seekable_known: false,
            pending_volume: None,
            state_machine: StateMachine::new(),
            track_ops: TrackOps::new(),
            stream_collection: None,
            stream_collection_notify: None,
            streams: Vec::new(),
        })
    }

    /// Map a playbin event onto the protocol-facing [`PlayerEvent`] and
    /// forward it into the application loop with the load generation it
    /// belongs to. Runs on whatever thread emitted the event (a streaming
    /// thread or the playbin worker). It only sends.
    fn relay_event(msg_tx: &MessageSender, event: fcastplaybin::PlaybinEvent, generation: u64) {
        use fcastplaybin::PlaybinEvent as E;
        let event = match event {
            E::EndOfStream => PlayerEvent::EndOfStream,
            E::Loaded { live } => {
                if live {
                    msg_tx.player(PlayerEvent::IsLive, Some(generation));
                }
                PlayerEvent::UriLoaded
            }
            E::Tags(tags) => PlayerEvent::Tags(tags),
            E::VolumeChanged(volume) => PlayerEvent::VolumeChanged(volume),
            E::StreamCollection(collection) => PlayerEvent::StreamCollection(collection),
            E::AsyncDone => PlayerEvent::AsyncDone,
            E::Buffering(percent) => PlayerEvent::Buffering(percent),
            E::StateChanged {
                old,
                current,
                pending,
            } => PlayerEvent::StateChanged {
                old,
                current,
                pending,
            },
            E::RequestState(state) => PlayerEvent::RequestState(state),
            E::QueueSeek(seek) => PlayerEvent::QueueSeek(seek),
            E::StreamsSelected {
                video,
                audio,
                subtitle,
                seqnum,
            } => PlayerEvent::StreamsSelected {
                video,
                audio,
                subtitle,
                seqnum,
            },
            E::RefreshSeekFailed { seqnum } => PlayerEvent::SubtitleRefreshFailed { seqnum },
            E::RateChanged(rate) => PlayerEvent::RateChanged(rate),
            E::SeekFailed => PlayerEvent::SeekFailed,
            E::ClockLost => PlayerEvent::ClockLost,
            E::Error {
                origin,
                error,
                failed_uri,
            } => PlayerEvent::Error {
                origin,
                kind: MediaErrorKind::from_glib_error(&error),
                message: error.message().to_string(),
                failed_uri,
            },
            E::Warning(message) => PlayerEvent::Warning(message),
        };
        msg_tx.player(event, Some(generation));
    }

    fn cleanup_stream_collection(&mut self) {
        if let Some(old_collection) = self.stream_collection.take()
            && let Some(sig_id) = self.stream_collection_notify.take()
        {
            old_collection.disconnect(sig_id);
        }
    }

    pub fn handle_stream_collection(&mut self, collection: gst::StreamCollection) {
        self.cleanup_stream_collection();

        let msg_tx = self.msg_tx.clone();
        self.stream_collection_notify = Some(collection.connect_stream_notify(
            None,
            move |_collection, _stream, param| {
                if param.name() == "tags" {
                    msg_tx.player(PlayerEvent::StreamTagsUpdated, None);
                }
            },
        ));

        self.streams.clear();

        for stream in collection.iter() {
            let title = stream_title(&stream);
            let stream = Stream {
                inner: stream,
                title,
            };

            self.streams.push(stream);
        }

        // The selection is stream-id-keyed, so nothing needs remapping across
        // collections: drop slots whose stream left the collection and seed
        // still-unselected slots with playbin3's defaults (the first stream
        // of each type), so a track change arriving before the initial
        // `StreamsSelected` keeps the other streams selected instead of
        // dropping them. The real `StreamsSelected` corrects these the moment
        // it arrives.
        self.selected.video = self
            .selected
            .video
            .take()
            .filter(|sid| Self::find_stream_idx(sid, &self.streams).is_some())
            .or_else(|| self.first_sid_of(gst::StreamType::VIDEO));
        self.selected.audio = self
            .selected
            .audio
            .take()
            .filter(|sid| Self::find_stream_idx(sid, &self.streams).is_some())
            .or_else(|| self.first_sid_of(gst::StreamType::AUDIO));
        self.selected.subtitle = self
            .selected
            .subtitle
            .take()
            .filter(|sid| Self::find_stream_idx(sid, &self.streams).is_some())
            .or_else(|| self.first_sid_of(gst::StreamType::TEXT));

        self.stream_collection = Some(collection);

        // Any SELECT_STREAMS/refresh still in flight targeted the previous
        // collection and can never confirm now (its stream ids are gone) --
        // abandon it deterministically rather than leaning on the watchdog.
        self.track_ops.invalidate_in_flight();
    }

    fn first_sid_of(&self, ty: gst::StreamType) -> Option<StreamId> {
        self.streams
            .iter()
            .find(|s| s.inner.stream_type().contains(ty))
            .and_then(|s| s.inner.stream_id())
            .map(|sid| sid.to_string())
    }

    /// The applied (or optimistically in-flight) stream id per slot.
    pub fn current_video_sid(&self) -> Option<&str> {
        self.selected.video.as_deref()
    }

    pub fn current_audio_sid(&self) -> Option<&str> {
        self.selected.audio.as_deref()
    }

    pub fn current_subtitle_sid(&self) -> Option<&str> {
        self.selected.subtitle.as_deref()
    }

    pub fn get_duration(&self) -> Option<gst::ClockTime> {
        self.fcast.duration()
    }

    pub fn get_position(&self) -> Option<gst::ClockTime> {
        self.fcast.position()
    }

    /// Buffered regions of the current media as timeline fractions, for the
    /// scrubber's buffered indicator. Empty when the source can't answer a
    /// buffering query (local file, live/SABR, pre-preroll).
    pub fn buffered_ranges(&self) -> Vec<fcastplaybin::BufferedRange> {
        self.fcast.buffered_ranges()
    }

    /// Inspector: full buffering state (fill percent, mode, rates, ranges).
    pub fn dbg_buffering(&self) -> Option<fcastplaybin::BufferingInfo> {
        self.fcast.buffering_info()
    }

    /// "Buffered ahead of the playhead" duration, for the scrubber's buffered
    /// nub in STREAM mode (where the buffering query reports no ranges).
    pub fn buffered_ahead(&self) -> Option<gst::ClockTime> {
        self.fcast.buffered_ahead()
    }

    fn clear_state(&mut self) {
        self.streams.clear();
        self.selected = TrackSelection::default();
        self.seekable = false;
        self.seekable_known = false;
        self.volume_confirm_in_flight = false;
        self.expected_generation = None;
        // A volume queued behind an in-flight confirmation must not be
        // stranded by the load (volume is not item-scoped): apply it now
        // that nothing is in flight.
        if let Some(volume) = self.pending_volume.take() {
            self.set_volume(volume);
        }
        self.track_ops.reset();
    }

    /// Whether an event stamped with `generation` belongs to the current
    /// load. Everything else is a superseded load's straggler.
    pub fn is_event_current(&self, generation: u64) -> bool {
        self.expected_generation == Some(generation)
    }

    /// Load a new main source (the crate resets to READY and wires it into
    /// decodebin3 on its worker thread. Completion comes back as
    /// `UriLoaded`). External subtitles attach separately as live inputs
    /// (`attach_external_subtitle`). Callers go through `load`.
    fn set_source(&mut self, source: MediaInput, start: fcastplaybin::StartPoint) {
        self.clear_state();
        self.state_machine.clear_state();
        self.expected_generation = Some(self.fcast.load_async(source, start));
        self.state_machine.begin_load();
    }

    /// Load a new main source. `start` is the post-preroll start seek
    /// (`None` for live sources, no seek at all). Embedded text auto-selects
    /// and links itself inside `fcastplaybin`, nothing to sequence here.
    pub fn load(&mut self, source: MediaInput, start: Option<RestorePoint>) {
        // A new load auto-plays unless a pause arrives while it is in flight.
        self.desired_transport = RunningState::Playing;
        // The start position/rate is applied inside `fcastplaybin::load`
        // while the pipeline is still in PAUSED, so a non-1.0 rate never
        // renders a 1.0x slice that a later seek flushes (the pop). `None`
        // marks a source with no start seek (live sources).
        let start = match start {
            Some(rp) => fcastplaybin::StartPoint::Seek {
                position: rp.position,
                rate: rp.rate as f64,
            },
            None => fcastplaybin::StartPoint::Live,
        };
        self.set_source(source, start);
    }

    fn seek_internal(&mut self, seek: Seek) {
        if let Some(rate) = seek.rate
            && !Seek::rate_is_safe(rate)
        {
            warn!(rate, "Ignoring invalid seek rate");
            return;
        }

        // An unresolved seekability query (`!seekable_known`) is not a
        // refusal: let the seek through. The state machine queues seeks that
        // land mid-preroll, so it runs once the pipeline settles. Only a
        // KNOWN unseekable stream drops the seek.
        if self.seekable || !self.seekable_known {
            // A user seek is itself a flushing seek and re-emits the current
            // subtitle cue, a separately queued refresh flush is redundant.
            self.track_ops.cancel_refresh();
            if let Some(seek) = self.state_machine.seek_internal(seek, None) {
                self.fcast.seek_async(seek);
            }
        } else {
            warn!(?seek, "Attempted to seek on a non seekable stream");
        }
    }

    pub fn seek(&mut self, position: gst::ClockTime) {
        self.seek_internal(Seek {
            position: Some(position),
            rate: None,
        });
    }

    fn applied_track_selection(&self) -> TrackSelection {
        self.selected.clone()
    }

    /// Handle a track-change request (latest-wins, serialized against other
    /// track operations, see `TrackOps`). Returns whether the currently
    /// displayed subtitle cue became stale. The caller should clear the
    /// overlay so the change registers visually, even while paused.
    pub fn request_track_change(&mut self, kind: TrackKind, sid: Option<StreamId>) -> bool {
        self.request_track_change_impl(kind, sid, false)
    }

    /// Like `request_track_change`, but never schedules the switch flush.
    /// For loads with an external subtitle attached: any flush races the
    /// external inputs' reconfiguration and can freeze the play item, so the
    /// new track takes effect at its next cue/buffer boundary instead.
    pub fn request_track_change_no_refresh(
        &mut self,
        kind: TrackKind,
        sid: Option<StreamId>,
    ) -> bool {
        self.request_track_change_impl(kind, sid, true)
    }

    fn request_track_change_impl(
        &mut self,
        kind: TrackKind,
        sid: Option<StreamId>,
        suppress_refresh: bool,
    ) -> bool {
        let applied = self.applied_track_selection();
        let stale_cue =
            kind == TrackKind::Subtitle && applied.subtitle.is_some() && sid != applied.subtitle;
        // The re-emit flush is safe to issue immediately for every subtitle
        // kind, bitmap included: fcastplaybin's subtitle branch splices in
        // without the playsink-era video-chain-rebuild deadlock (validated
        // by stressing the bitmap switch), so no timing defer is needed.
        self.track_ops.request(kind, sid, applied);
        if suppress_refresh {
            self.track_ops.suppress_refresh();
        }
        self.pump_track_ops();
        stale_cue
    }

    /// Dispatch pending track work now that the pipeline may have settled.
    /// Called from the state-change handler (a re-preroll finishing is what
    /// unblocks work parked behind it). The pump is otherwise driven event-
    /// driven: a new request, `streams_selected`, `async_done`, buffering
    /// completion, and refresh failure, no periodic poll.
    pub fn poll_track_ops(&mut self) {
        self.pump_track_ops();
    }

    fn track_op_ctx(&self) -> TrackOpCtx {
        // Ask the pipeline whether an async state change (re-preroll, seek
        // preroll) is in progress instead of predicting from the kind of
        // change, mispredictions are what used to wedge this logic.
        let async_busy = self.fcast.has_async_transition();
        let (running, paused) = match self.state_machine.running() {
            Some(state) => (true, state == RunningState::Paused),
            None => (false, false),
        };
        TrackOpCtx {
            quiet: running && !async_busy,
            paused,
            applied: self.applied_track_selection(),
        }
    }

    fn pump_track_ops(&mut self) {
        while self.track_ops.has_dispatchable_work() {
            let ctx = self.track_op_ctx();
            let Some(cmd) = self.track_ops.pump(&ctx) else {
                break;
            };
            match cmd {
                TrackOpCommand::SelectStreams(sel) => {
                    let seqnum = gst::Seqnum::next();
                    match self.select_streams(sel, seqnum) {
                        Ok(true) => {
                            // `select_streams` set `current_*` to exactly what
                            // it sent (after the video-less-subtitle
                            // adjustment), so this is the selection whose
                            // `STREAMS_SELECTED` we wait for, by seqnum or by
                            // content.
                            let desired = self.applied_track_selection();
                            self.track_ops.selection_dispatched(seqnum, desired);
                        }
                        // Nothing was sent, so there is no completion to wait
                        // for. A refresh scheduled for this switch must not
                        // fire as an orphan flush either.
                        Ok(false) => self.track_ops.cancel_refresh(),
                        Err(err) => {
                            error!(?err, "Failed to apply track selection");
                            self.track_ops.cancel_refresh();
                        }
                    }
                }
                TrackOpCommand::RefreshSeek => {
                    if !self.seekable {
                        debug!("Skipping subtitle refresh: stream is not seekable");
                        continue;
                    }
                    let seqnum = gst::Seqnum::next();
                    self.track_ops.refresh_dispatched(seqnum);
                    self.fcast.refresh_seek_async(seqnum);
                }
            }
        }
    }

    /// A top-level `ASYNC_DONE`: the pipeline has re-prerolled and settled.
    pub fn async_done(&mut self) {
        // A flush (e.g. the subtitle re-emit) has re-prerolled and the pipeline
        // is settled again. If a subtitle switch happened while paused, its new
        // text branch may still be parked (it routed mid-flush, when the
        // pipeline wasn't settled), link it now that we're steady so the
        // re-emit's cue actually composites onto the frozen frame.
        self.fcast.poll_text_policy();
        // Settle any in-flight refresh seek. The flushing seek re-prerolls
        // while paused, so every sink has its composited preroll frame before
        // this ASYNC_DONE fires, a single flush deterministically renders the
        // new cue, no retry needed.
        self.track_ops.refresh_done();
        self.pump_track_ops();
    }

    /// The refresh seek job could not perform its seek.
    pub fn subtitle_refresh_failed(&mut self, seqnum: gst::Seqnum) {
        self.track_ops.refresh_failed(seqnum);
        self.pump_track_ops();
    }

    pub fn is_seeking(&self) -> bool {
        self.state_machine.is_seeking()
    }

    pub fn queue_seek(&mut self, seek: Seek) {
        self.state_machine.queue_seek(seek);
    }

    /// Set the volume. The value itself lives in the playbin
    /// (`FcastPlaybin::set_volume`). What stays here is the receiver's
    /// confirmation protocol: senders expect exactly one `VolumeChanged`
    /// per request, so overlapping requests are queued (latest wins) and an
    /// idempotent set re-emits its confirmation.
    pub fn set_volume(&mut self, volume: f32) {
        if self.volume_confirm_in_flight {
            // A previous change's confirmation is still in flight. Don't
            // drop the request (the sender would wait forever for its
            // confirmation). Remember the latest and apply it once the
            // confirmation arrives.
            debug!(volume, "Volume change pending; queueing");
            self.pending_volume = Some(volume);
            return;
        }

        let target = (volume as f64).clamp(0.0, 1.0);
        if (self.fcast.volume() - target).abs() < 1e-9 {
            // Setting the property to its current value emits no notify,
            // but senders expect a confirmation for an idempotent set too.
            // Re-emit it manually through the same VolumeChanged path.
            debug!(volume, "Volume unchanged; re-emitting the confirmation");
            self.fcast.renotify_volume();
            return;
        }

        self.fcast.set_volume(target);
        self.volume_confirm_in_flight = true;
    }

    pub fn volume_changed(&mut self) {
        self.volume_confirm_in_flight = false;
        // Apply the newest request that arrived while the confirmation was
        // in flight (last one wins).
        if let Some(volume) = self.pending_volume.take() {
            self.set_volume(volume);
        }
    }

    pub fn set_rate(&mut self, rate: f32) {
        self.seek_internal(Seek {
            position: None,
            rate: Some(rate),
        });
    }

    pub fn update_media_info(&mut self) {
        if let Some(seekable) = self.fcast.query_seekable() {
            let dur = self.get_duration();
            debug!(?dur, seekable, "Seek query returned");
            self.seekable = seekable && dur.is_some();
            self.seekable_known = true;
        }
    }

    fn set_state_async(&self, target_state: gst::State) {
        self.fcast.set_state_async(target_state);
    }

    pub fn play(&mut self) {
        self.desired_transport = RunningState::Playing;
        if let Some(state) = self.state_machine.set_playback_state(RunningState::Playing) {
            self.set_state_async(state);
        }
    }

    /// Honor a `RequestState` message from an element by dispatching the state
    /// change to the worker thread (off the streaming thread it arrived on).
    pub fn request_state(&self, state: gst::State) {
        self.set_state_async(state);
    }

    /// Handle `ClockLost`: the element providing the pipeline clock went away
    /// (typically the audio sink after the audio track was deselected).
    pub fn recover_clock(&mut self) {
        if !matches!(self.player_state(), PlayerState::Playing) {
            debug!("Ignoring clock loss while not playing");
            return;
        }
        debug!("Pipeline clock lost; cycling through Paused to elect a new one");
        self.fcast.recover_clock_async();
    }

    /// Produce a dot dump of the pipeline for the inspector, delivered via
    /// `done`. Runs on the fcastplaybin worker so the graph walk is
    /// serialized against loads and teardowns (`debug_to_dot_data` reads
    /// every element's properties mid-walk, and racing the per-load audio
    /// sink's finalize double-freed in the sink). `done` is invoked on the
    /// worker thread: hand the work off, do not block in it.
    pub fn request_graph_dot_data(&self, done: impl FnOnce(String) + Send + 'static) {
        self.fcast.debug_dot_data_async(Box::new(done));
    }

    #[cfg(debug_assertions)]
    pub fn dump_graph(&self, _trigger: remote_pipeline_dbg::Trigger) {
        // Disabled: an inline dot walk races per-load audio sink teardown
        // into a double-free (see request_graph_dot_data). A fatal crash in
        // a debugging aid whose endpoint usually isn't even listening.
    }

    pub fn pause(&mut self) {
        self.desired_transport = RunningState::Paused;
        if let Some(state) = self.state_machine.set_playback_state(RunningState::Paused) {
            self.set_state_async(state);
        }
    }

    fn go_to_stopped_state(&mut self, null: Option<oneshot::Sender<()>>) {
        self.desired_transport = RunningState::Playing;
        self.cleanup_stream_collection();

        // A full teardown either way (pipeline down, inputs and the per-load
        // audio sink removed), so a Stop releases the item's network/audio
        // resources NOW rather than at the next load. Queued on the worker,
        // it also aborts an in-flight load cleanly (jobs are ordered).
        match null {
            Some(feedback) => self.fcast.shutdown_async(Box::new(move || {
                debug!(res = ?feedback.send(()), "Sent shutdown feedback signal");
            })),
            None => {
                // Don't raise an already shut-down pipeline back to READY.
                if self.state_machine.current_state != gst::State::Null {
                    self.fcast.stop_async();
                }
            }
        }

        // Unconditional: even when the pipeline needed no state change (a
        // stop landing mid-load, with the pipeline still at READY), the
        // machine and the per-item state must reset or the aborted load's
        // leftovers leak into the next one.
        self.state_machine.clear_state();
        self.clear_state();
    }

    pub fn stop(&mut self) {
        debug!("Stopping playback");
        self.go_to_stopped_state(None)
    }

    pub fn shutdown(&mut self, feedback: oneshot::Sender<()>) {
        debug!("Shutting down player");
        self.go_to_stopped_state(Some(feedback));
    }

    /// Returns `true` if any stream has new properties.
    pub fn update_stream_properties(&mut self) -> bool {
        let mut did_change = false;

        for stream in &mut self.streams {
            let title = stream_title(&stream.inner);
            if title != stream.title {
                stream.title = title;
                did_change = true;
            }
        }

        did_change
    }

    /// Send a `SELECT_STREAMS` for the given selection, stamped with `seqnum`
    /// so the confirming `STREAMS_SELECTED` message can be attributed to it.
    /// Returns whether an event was actually sent.
    fn select_streams(
        &mut self,
        mut selection: TrackSelection,
        seqnum: gst::Seqnum,
    ) -> Result<bool> {
        // A text stream cannot be presented without a video stream, so a
        // selection without video must never carry a subtitle stream.
        // Deselecting video therefore implicitly deselects subtitles, and
        // the relayed `TracksSelected` reports that to the senders.
        if selection.video.is_none() && selection.subtitle.is_some() {
            debug!("Dropping the subtitle stream from a selection without video");
            selection.subtitle = None;
        }

        // Only ids the current collection actually advertises (a stale sid
        // in the event would confuse decodebin3's selection).
        let ids: Vec<&str> = [&selection.video, &selection.audio, &selection.subtitle]
            .into_iter()
            .filter_map(|sid| sid.as_deref())
            .filter(|sid| Self::find_stream_idx(sid, &self.streams).is_some())
            .collect();

        // An empty selection would trip a GStreamer assertion
        // (`gst_event_new_select_streams: streams != NULL`) and leave the
        // pipeline in an undefined state, so refuse to send one.
        if ids.is_empty() {
            debug!("Refusing to send an empty stream selection");
            return Ok(false);
        }

        // Straight to decodebin3, no detour through the sinks.
        if let Err(err) = self.fcast.select_streams(&ids, Some(seqnum)) {
            warn!(?err, "fcastplaybin refused the stream selection");
            return Ok(false);
        }

        // Track the requested selection right away instead of waiting for
        // `StreamsSelected`: a second track change arriving before the first
        // one is confirmed must compose with it, not revert it (each change
        // rebuilds the full selection from the applied one).
        // `streams_selected` overwrites this with whatever the pipeline
        // actually applied.
        self.selected = selection;

        Ok(true)
    }

    /// The index of the stream with this GStreamer stream id, if advertised.
    pub fn stream_idx_by_id(&self, sid: &str) -> Option<u32> {
        Self::find_stream_idx(sid, &self.streams)
    }

    /// Cumulative parsed-byte counters per live input stream, for the
    /// inspector's bitrate sampling (poll and diff; see
    /// `fcastplaybin::StreamIoStats`). All of the item's streams are counted,
    /// selected or not; correlate with `streams`/`current_*_sid` for kind and
    /// selection.
    pub fn stream_io_stats(&self) -> Vec<fcastplaybin::StreamIoStats> {
        self.fcast.stream_io_stats()
    }

    /// Inspector: every advertised stream plus whether it is currently
    /// selected, for the track table (`gst::Stream` clones are refcounted).
    pub fn stream_dbg_rows(&self) -> Vec<(gst::Stream, bool)> {
        self.streams
            .iter()
            .map(|s| {
                let sid = s.inner.stream_id().map(|id| id.to_string());
                let selected = sid.is_some()
                    && [
                        &self.selected.video,
                        &self.selected.audio,
                        &self.selected.subtitle,
                    ]
                    .into_iter()
                    .any(|sel| *sel == sid);
                (s.inner.clone(), selected)
            })
            .collect()
    }

    /// Inspector: pipeline current + pending state.
    pub fn dbg_state_summary(&self) -> (gst::State, gst::State) {
        self.fcast.state_summary()
    }

    /// Inspector: "kind:pad" for every routed decodebin3 stream.
    pub fn dbg_routed_summary(&self) -> Vec<String> {
        self.fcast.routed_summary()
    }

    /// Inspector: every live input's factory and uri.
    pub fn dbg_sources(&self) -> Vec<fcastplaybin::SourceDbg> {
        self.fcast.source_summaries()
    }

    /// Inspector: elements with an unfinished state transition.
    pub fn dbg_unsettled_elements(&self) -> Vec<String> {
        self.fcast.unsettled_elements()
    }

    /// Inspector: the video sink's rendered/dropped buffer counts.
    pub fn dbg_video_sink_stats(&self) -> Option<gst::Structure> {
        self.fcast.video_sink_stats()
    }

    /// Inspector: the audio sink's negotiated caps and rendered/dropped
    /// counts, while a per-load sink exists.
    pub fn dbg_audio_sink_health(&self) -> Option<(Option<gst::Caps>, Option<gst::Structure>)> {
        self.fcast.audio_sink_health()
    }

    /// Inspector: the generation the player currently accepts events from.
    pub fn dbg_generation(&self) -> Option<u64> {
        self.expected_generation
    }

    /// Whether the pipeline is settled, meaning no async state transition
    /// is in progress (non-blocking query). Used to hold flushing operations
    /// off while a reconfiguration that posts no bus signal of its own is
    /// still in flight.
    pub fn is_pipeline_stable(&self) -> bool {
        self.fcast.is_settled()
    }

    /// Diagnostic (load-stall investigation): explain why a load has not
    /// reached a steady PAUSED. Logs the pipeline's current+pending state, the
    /// media's stream collection kinds vs the decodebin3 pads actually routed
    /// (a selected stream kind with no matching routed pad is the stall), and
    /// dumps a pipeline `.dot` (needs `GST_DEBUG_DUMP_DOT_DIR`).
    pub fn log_load_stall_diagnostics(&self, tag: &str) {
        let (current, pending) = self.fcast.state_summary();
        let collection: Vec<&'static str> = self
            .streams
            .iter()
            .map(|s| {
                let t = s.inner.stream_type();
                if t.contains(gst::StreamType::VIDEO) {
                    "video"
                } else if t.contains(gst::StreamType::AUDIO) {
                    "audio"
                } else if t.contains(gst::StreamType::TEXT) {
                    "text"
                } else {
                    "other"
                }
            })
            .collect();
        let routed = self.fcast.routed_summary();
        let elements = self.fcast.element_states();
        warn!(
            tag,
            ?current,
            ?pending,
            collection = ?collection,
            routed = ?routed,
            elements = ?elements,
            "LOAD STALL DIAGNOSTIC: pipeline not steady"
        );
        self.fcast.dump_dot(&format!("load-stall-{tag}"));
    }

    /// The GStreamer stream id of the `idx`th advertised stream.
    pub fn stream_id_of(&self, idx: u32) -> Option<String> {
        self.streams
            .get(idx as usize)?
            .inner
            .stream_id()
            .map(|id| id.to_string())
    }

    pub fn is_stream_of_type(&self, idx: u32, ty: gst::StreamType) -> bool {
        self.streams
            .get(idx as usize)
            .is_some_and(|s| s.inner.stream_type().contains(ty))
    }

    pub fn end_of_stream_reached(&mut self) {
        self.stop();
    }

    pub fn uri_loaded(&mut self) {
        // The load is wired (and usually still prerolling). Commit the
        // transport the user last asked for: Playing unless a pause landed
        // while the load was in flight. This is the ONE post-load transport
        // driver. A load whose user already paused never blips through
        // Playing at all.
        let desired = self.desired_transport;
        if let Some(state) = self.state_machine.set_playback_state(desired) {
            self.set_state_async(state);
        } else if self.state_machine.running() != Some(desired) {
            // The machine could not act on it, typically because the load's
            // preroll has not settled yet (Loaded arrives when the load job
            // returns, before the async climb finishes). Drive the pipeline
            // directly; the machine follows the state edges as always.
            self.set_state_async(desired.into());
        }
    }

    /// Returns `true` if buffering completed
    pub fn buffering(&mut self, percent: i32) -> bool {
        let res = match self.state_machine.buffering(percent) {
            BufferingStateResult::Started(state) => {
                self.set_state_async(state);
                false
            }
            BufferingStateResult::Buffering => false,
            BufferingStateResult::FinishedWithSeek(seek) => {
                debug!("Buffering finished, dispatching seek");
                self.fcast.seek_async(seek);
                true
            }
            BufferingStateResult::FinishedButWaitingSeek => {
                debug!("Buffering finished with seek");
                true
            }
            BufferingStateResult::Finished(state) => {
                debug!("Buffering finished");
                if let Some(state) = state {
                    self.set_state_async(state);
                }
                true
            }
        };

        // Buffering completion can settle the pipeline, dispatch queued track
        // work (no-op while still buffering: the machine is not `Running`).
        self.pump_track_ops();

        res
    }

    /// Live-attach an external subtitle input to the running pipeline.
    /// Returns the reserved id immediately, the attach itself runs on the
    /// playbin's worker thread (the source's `start()` blocks). The stream
    /// becomes selectable once decodebin3 announces the updated collection
    /// (always a later collection, mapped back with
    /// `external_stream_sid_of`). An attach that fails never produces one,
    /// which the caller's watchdog turns into `ResourceNotFound`.
    pub fn attach_external_subtitle(&mut self, url: &str) -> fcastplaybin::ExternalSubId {
        let id = self.fcast.allocate_subtitle_id();
        self.fcast.attach_subtitle_async(id, url.to_string());
        id
    }

    /// Detach a live external subtitle input (failed URL, or its catalog
    /// entry going away). Best effort, on the playbin's worker thread. The
    /// input is leaving regardless.
    pub fn detach_external_subtitle(&mut self, id: fcastplaybin::ExternalSubId) {
        self.fcast.detach_subtitle_async(id);
    }

    /// fcast backend: the GStreamer stream id of an attached external
    /// subtitle input, once its stream has appeared in the advertised
    /// collection. The id is URI-derived and therefore STABLE across
    /// replacements of the input (see the application's re-arm logic), so
    /// callers should remember it rather than re-query the (replaceable)
    /// handle.
    pub fn external_stream_sid_of(&self, id: fcastplaybin::ExternalSubId) -> Option<String> {
        let sids = self.fcast.subtitle_stream_ids(id);
        let sid = sids
            .into_iter()
            .find(|sid| Self::find_stream_idx(sid, &self.streams).is_some());
        debug!(?id, ?sid, "external subtitle stream lookup");
        sid
    }

    pub fn state_changed(
        &mut self,
        old: gst::State,
        new: gst::State,
        pending: gst::State,
    ) -> Option<PlaybackState> {
        // A state change is the settle event for the crate's text link
        // policy: parked text may join subtitleoverlay only once the
        // pipeline is SETTLED >= PAUSED, and this callback fires exactly
        // when that can newly hold (the crate re-checks, cheap no-op
        // otherwise).
        self.fcast.poll_text_policy();
        // Queued track work is deliberately NOT pumped from here: the
        // application runs this at the START of its StateChanged handling,
        // and a Playing commit's cascade may still launch a restore seek. A
        // selection dispatched into that one-instant-quiet window
        // interleaves with the seek's Playing->Paused->seek->Playing dance
        // and its reconfigure runs outside steady PLAYING (a parked
        // video-disable dispatched at the commit once wedged the pipeline
        // for good). The application pumps at the END of the cascade
        // instead, when the seek, if any, already owns the state machine.
        match self.state_machine.state_changed(old, new, pending) {
            // Map the backend-native playback state onto the FCast wire enum
            // (fcastplaybin is protocol-agnostic, this is the only seam).
            StateChangeResult::NewPlaybackState(new_state) => {
                use fcastplaybin::state_machine::PlaybackState as SmState;
                Some(match new_state {
                    SmState::Idle => PlaybackState::Idle,
                    SmState::Paused => PlaybackState::Paused,
                    SmState::Playing => PlaybackState::Playing,
                })
            }
            StateChangeResult::Seek(seek) => {
                self.fcast.seek_async(seek);
                None
            }
            StateChangeResult::Waiting => None,
            StateChangeResult::ChangeState(state) => {
                self.set_state_async(state);
                None
            }
        }
    }

    pub fn have_media_info(&self) -> bool {
        !self.streams.is_empty()
    }

    fn find_stream_idx(sid: &str, streams: &[Stream]) -> Option<u32> {
        for (idx, stream) in streams.iter().enumerate() {
            if let Some(this_id) = stream.inner.stream_id()
                && this_id == sid
            {
                return Some(idx as u32);
            }
        }

        None
    }

    #[cfg_attr(not(target_os = "android"), instrument(skip_all))]
    pub fn streams_selected(
        &mut self,
        video_sid: Option<&str>,
        audio_sid: Option<&str>,
        subtitle_sid: Option<&str>,
        seqnum: gst::Seqnum,
    ) -> TrackSelection {
        debug!(?video_sid, ?audio_sid, ?subtitle_sid, ?seqnum);

        self.fcast.poll_text_policy();

        // Adopt what the pipeline reports as applied, verbatim (stream ids
        // need no index mapping).
        self.selected = TrackSelection {
            video: video_sid.map(str::to_string),
            audio: audio_sid.map(str::to_string),
            subtitle: subtitle_sid.map(str::to_string),
        };

        // Settle the in-flight selection: by the seqnum decodebin3 stamped, or
        // by the reported selection matching what we dispatched (a superseded /
        // coalesced / no-op selection confirms under a different seqnum).
        self.track_ops.streams_selected(seqnum, &self.selected);

        // Dispatch the next queued operation now that this one confirmed. A
        // plain switch (subtitle, or an audio/video switch between already-
        // decoded streams) applies with no re-preroll and so posts no further
        // bus message, this is the event that advances the queue for it. If
        // the switch DID trigger a re-preroll, `pump`'s quiet gate (it
        // queries the pipeline's async state) holds the next op back until
        // the ASYNC_DONE/state-change handler pumps again, so this never
        // dispatches into a re-preroll.
        self.pump_track_ops();

        self.selected.clone()
    }

    pub fn player_state(&self) -> PlayerState {
        if self.state_machine.is_stopped() {
            return PlayerState::Stopped;
        }
        match self.state_machine.running() {
            Some(RunningState::Paused) => PlayerState::Paused,
            Some(RunningState::Playing) => PlayerState::Playing,
            // The wire protocol has no loading/seeking state. Buffering is
            // the honest "not rendering, working on it" for everything in
            // transition.
            None => PlayerState::Buffering,
        }
    }

    pub fn is_live(&self) -> bool {
        self.state_machine.is_live
    }

    pub fn set_is_live(&mut self, live: bool) {
        self.state_machine.is_live = live;
    }

    pub fn rate(&self) -> f64 {
        self.state_machine.rate
    }

    #[instrument(skip_all)]
    pub fn seek_failed(&mut self) {
        if let Some(target_state) = self.state_machine.seek_failed() {
            debug!(?target_state);
            self.set_state_async(target_state);
        }
    }

    pub fn set_rate_changed(&mut self, rate: f64) {
        self.state_machine.rate = rate;
    }
}

impl Drop for Player {
    fn drop(&mut self) {
        // The playbin's worker exits on its own once the last handle drops.
        // Queue the final teardown (usually a no-op, `shutdown` already
        // drove the pipeline to Null and waited).
        self.set_state_async(gst::State::Null);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // --- TrackOps -----------------------------------------------------------

    fn sel(video: Option<&str>, audio: Option<&str>, subtitle: Option<&str>) -> TrackSelection {
        TrackSelection {
            video: video.map(str::to_string),
            audio: audio.map(str::to_string),
            subtitle: subtitle.map(str::to_string),
        }
    }

    fn ctx(quiet: bool, paused: bool, applied: &TrackSelection) -> TrackOpCtx {
        TrackOpCtx {
            quiet,
            paused,
            applied: applied.clone(),
        }
    }

    #[test]
    fn selection_dispatches_immediately_when_quiet() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        ops.request(TrackKind::Audio, Some("st2".to_string()), applied.clone());
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st2"),
                None
            )))
        );
    }

    #[test]
    fn selection_waits_until_quiet() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        ops.request(TrackKind::Audio, Some("st2".to_string()), applied.clone());
        assert_eq!(ops.pump(&ctx(false, false, &applied)), None);
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st2"),
                None
            )))
        );
    }

    #[test]
    fn noop_selection_is_not_dispatched() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), Some("st2"));
        ops.request(
            TrackKind::Subtitle,
            Some("st2".to_string()),
            applied.clone(),
        );
        assert_eq!(ops.pump(&ctx(true, false, &applied)), None);
        assert!(!ops.has_dispatchable_work());
    }

    #[test]
    fn playing_switch_serializes_and_coalesces_latest_wins() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), Some("st2"));
        ops.request(
            TrackKind::Subtitle,
            Some("st3".to_string()),
            applied.clone(),
        );
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st1"),
                Some("st3")
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st1"), Some("st3")));
        // `select_streams` records the new selection optimistically.
        let applied = sel(Some("st0"), Some("st1"), Some("st3"));

        // Unconfirmed selection blocks everything while playing, including
        // the refresh the subtitle switch scheduled.
        assert_eq!(ops.pump(&ctx(true, false, &applied)), None);

        // Spammed changes only remember the latest.
        ops.request(
            TrackKind::Subtitle,
            Some("st4".to_string()),
            applied.clone(),
        );
        ops.request(
            TrackKind::Subtitle,
            Some("st2".to_string()),
            applied.clone(),
        );
        assert_eq!(ops.pump(&ctx(true, false, &applied)), None);

        // A foreign STREAMS_SELECTED (decodebin3 selecting on its own)
        // matching ours neither by seqnum nor by content means ours was
        // overtaken: the wait ends and the queued latest dispatches against
        // the adopted selection instead of parking forever.
        let adopted = sel(Some("st0"), Some("st1"), Some("st9"));
        ops.streams_selected(gst::Seqnum::next(), &adopted);
        assert_eq!(
            ops.pump(&ctx(true, false, &adopted)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st1"),
                Some("st2")
            )))
        );
    }

    #[test]
    fn selection_confirms_by_content_when_seqnum_is_lost() {
        // decodebin3 can post the confirming STREAMS_SELECTED under a seqnum
        // that isn't the one we stamped (a superseded/coalesced/no-op request
        // folds into another event). As long as the reported selection matches
        // what we dispatched, it settles, no watchdog needed.
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        ops.request(
            TrackKind::Subtitle,
            Some("st2".to_string()),
            applied.clone(),
        );
        assert!(ops.pump(&ctx(true, false, &applied)).is_some());
        ops.selection_dispatched(
            gst::Seqnum::next(),
            sel(Some("st0"), Some("st1"), Some("st2")),
        );
        let applied = sel(Some("st0"), Some("st1"), Some("st2"));

        // A confirmation under a *foreign* seqnum, but reporting exactly our
        // requested selection, settles it.
        ops.streams_selected(gst::Seqnum::next(), &applied);
        assert!(ops.selecting.is_none());
    }

    #[test]
    fn overtaken_selection_is_redispatched() {
        // The external_sub_add_unselected stress failure: attaching an
        // external subtitle with select=false posts a new collection, the
        // post-attach enforcement dispatches a no-subtitle selection against
        // it, and decodebin3's own collection-default auto-select (fresh
        // text stream included) lands after ours and stomps it. That
        // overtaking STREAMS_SELECTED must re-dispatch the enforcement
        // instead of waiting forever on a confirmation that never comes.
        let mut ops = TrackOps::new();
        let applied = sel(Some("v0"), Some("a0"), Some("ext0"));
        ops.request(TrackKind::Subtitle, None, applied.clone());
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("v0"),
                Some("a0"),
                None
            )))
        );
        ops.selection_dispatched(gst::Seqnum::next(), sel(Some("v0"), Some("a0"), None));

        // decodebin3's own auto-select arrives instead of our confirmation:
        // foreign seqnum, foreign content.
        let adopted = sel(Some("v0"), Some("a0"), Some("ext0"));
        ops.streams_selected(gst::Seqnum::next(), &adopted);

        // The overtaken request re-dispatches with a fresh seqnum.
        assert_eq!(
            ops.pump(&ctx(true, false, &adopted)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("v0"),
                Some("a0"),
                None
            )))
        );
        ops.selection_dispatched(gst::Seqnum::next(), sel(Some("v0"), Some("a0"), None));

        // This time it applies (content match settles under any seqnum) and
        // the queue drains.
        ops.streams_selected(gst::Seqnum::next(), &sel(Some("v0"), Some("a0"), None));
        assert!(ops.selecting.is_none());
        assert!(!ops.has_dispatchable_work());
    }

    #[test]
    fn overtaken_selection_yields_to_a_newer_request() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("v0"), Some("a0"), None);
        ops.request(TrackKind::Subtitle, Some("s1".to_string()), applied.clone());
        assert!(ops.pump(&ctx(true, false, &applied)).is_some());
        ops.selection_dispatched(gst::Seqnum::next(), sel(Some("v0"), Some("a0"), Some("s1")));

        // A newer request lands while the first is unconfirmed.
        let optimistic = sel(Some("v0"), Some("a0"), Some("s1"));
        ops.request(
            TrackKind::Subtitle,
            Some("s2".to_string()),
            optimistic.clone(),
        );

        // The overtaking event must not resurrect the old request over it.
        let adopted = sel(Some("v0"), Some("a0"), Some("s0"));
        ops.streams_selected(gst::Seqnum::next(), &adopted);
        assert_eq!(
            ops.pump(&ctx(true, false, &adopted)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("v0"),
                Some("a0"),
                Some("s2")
            )))
        );
    }

    #[test]
    fn refresh_dispatches_after_selection_settles_and_pipeline_quiets() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        // Enabling a subtitle schedules a refresh.
        ops.request(
            TrackKind::Subtitle,
            Some("st2".to_string()),
            applied.clone(),
        );
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st1"),
                Some("st2")
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st1"), Some("st2")));
        let applied = sel(Some("st0"), Some("st1"), Some("st2"));

        ops.streams_selected(sn, &applied);
        // Re-preroll in progress: refresh must hold.
        assert_eq!(ops.pump(&ctx(false, false, &applied)), None);
        // Settled: flush.
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
        // One flush only.
        assert_eq!(ops.pump(&ctx(true, false, &applied)), None);
    }

    #[test]
    fn audio_switch_schedules_refresh() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        // An audio switch must flush the deeply-buffered old track so it's
        // audible immediately.
        ops.request(TrackKind::Audio, Some("st2".to_string()), applied.clone());
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st2"),
                None
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st2"), None));
        let applied = sel(Some("st0"), Some("st2"), None);
        ops.streams_selected(sn, &applied);
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
        assert_eq!(ops.pump(&ctx(true, false, &applied)), None);
    }

    #[test]
    fn suppressed_audio_switch_schedules_no_refresh() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        // External suburi attached: any flush can freeze the item, so the app
        // suppresses it for A/V switches too.
        ops.request(TrackKind::Audio, Some("st2".to_string()), applied.clone());
        ops.suppress_refresh();
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st2"),
                None
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st2"), None));
        let applied = sel(Some("st0"), Some("st2"), None);
        ops.streams_selected(sn, &applied);
        assert_eq!(ops.pump(&ctx(true, false, &applied)), None);
    }

    #[test]
    fn audio_switch_with_subtitle_disable_schedules_no_refresh() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), Some("st2"));
        // Switching audio while also disabling subtitles: the subtitle-disable
        // flush hazard wins, so no flush (accept the audio drain in this combo).
        ops.request(TrackKind::Audio, Some("st3".to_string()), applied.clone());
        ops.request(TrackKind::Subtitle, None, applied.clone());
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st3"),
                None
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st3"), None));
        let applied = sel(Some("st0"), Some("st3"), None);
        ops.streams_selected(sn, &applied);
        assert_eq!(ops.pump(&ctx(true, false, &applied)), None);
    }

    #[test]
    fn subtitle_disable_cancels_refresh() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        ops.request(
            TrackKind::Subtitle,
            Some("st2".to_string()),
            applied.clone(),
        );
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st1"),
                Some("st2")
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st1"), Some("st2")));
        let applied = sel(Some("st0"), Some("st1"), Some("st2"));
        ops.streams_selected(sn, &applied);

        // Disable before the refresh fired: no flush may follow (flushing
        // right after the text-branch teardown breaks renegotiation).
        ops.request(TrackKind::Subtitle, None, applied.clone());
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st1"),
                None
            )))
        );
        let sn2 = gst::Seqnum::next();
        ops.selection_dispatched(sn2, sel(Some("st0"), Some("st1"), None));
        let applied = sel(Some("st0"), Some("st1"), None);
        ops.streams_selected(sn2, &applied);
        assert_eq!(ops.pump(&ctx(true, false, &applied)), None);
    }

    #[test]
    fn suppressed_subtitle_switch_schedules_no_refresh() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        // External suburi attached: the app forbids the re-emit flush.
        ops.request(
            TrackKind::Subtitle,
            Some("st2".to_string()),
            applied.clone(),
        );
        ops.suppress_refresh();
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st1"),
                Some("st2")
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st1"), Some("st2")));
        let applied = sel(Some("st0"), Some("st1"), Some("st2"));
        ops.streams_selected(sn, &applied);
        // No flush may follow the confirmed selection, and nothing stays
        // queued or in flight.
        assert_eq!(ops.pump(&ctx(true, false, &applied)), None);
        assert!(!ops.has_dispatchable_work());
        assert!(ops.selecting.is_none());
        assert!(ops.refreshing.is_none());
    }

    #[test]
    fn each_subtitle_request_redecides_refresh_suppression() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        // A suppressed request parks (pipeline busy)...
        ops.request(
            TrackKind::Subtitle,
            Some("st2".to_string()),
            applied.clone(),
        );
        ops.suppress_refresh();
        assert_eq!(ops.pump(&ctx(false, false, &applied)), None);
        // ...and is superseded by a plain one: its flush is allowed again.
        ops.request(
            TrackKind::Subtitle,
            Some("st3".to_string()),
            applied.clone(),
        );
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st1"),
                Some("st3")
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st1"), Some("st3")));
        let applied = sel(Some("st0"), Some("st1"), Some("st3"));
        ops.streams_selected(sn, &applied);
        assert_eq!(
            ops.pump(&ctx(true, false, &applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
    }

    #[test]
    fn user_seek_cancels_refresh() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        ops.request(
            TrackKind::Subtitle,
            Some("st2".to_string()),
            applied.clone(),
        );
        assert!(ops.pump(&ctx(true, false, &applied)).is_some());
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st1"), Some("st2")));
        let applied = sel(Some("st0"), Some("st1"), Some("st2"));
        ops.streams_selected(sn, &applied);

        // The user's own flushing seek re-emits the cue already.
        ops.cancel_refresh();
        assert_eq!(ops.pump(&ctx(true, false, &applied)), None);
    }

    #[test]
    fn paused_selection_parks_and_refresh_flushes_past_it() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        ops.request(
            TrackKind::Subtitle,
            Some("st2".to_string()),
            applied.clone(),
        );
        assert_eq!(
            ops.pump(&ctx(true, true, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st1"),
                Some("st2")
            )))
        );
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st1"), Some("st2")));
        let applied = sel(Some("st0"), Some("st1"), Some("st2"));

        // While paused the selection is parked (no STREAMS_SELECTED until data
        // flows), the refresh must dispatch anyway, it is what wakes the
        // pipeline and makes the selection apply.
        assert_eq!(
            ops.pump(&ctx(true, true, &applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
        let rn = gst::Seqnum::next();
        ops.refresh_dispatched(rn);

        // Flush in flight: nothing else dispatches even though paused.
        ops.request(TrackKind::Audio, Some("st3".to_string()), applied.clone());
        assert_eq!(ops.pump(&ctx(false, true, &applied)), None);
    }

    #[test]
    fn paused_selection_can_be_superseded() {
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        ops.request(TrackKind::Audio, Some("st2".to_string()), applied.clone());
        assert!(ops.pump(&ctx(true, true, &applied)).is_some());
        let sn1 = gst::Seqnum::next();
        ops.selection_dispatched(sn1, sel(Some("st0"), Some("st2"), None));
        let applied = sel(Some("st0"), Some("st2"), None);

        // A parked selection has no re-preroll to overlap with, the next
        // request replaces it instead of queueing behind it forever.
        ops.request(TrackKind::Audio, Some("st1".to_string()), applied.clone());
        assert_eq!(
            ops.pump(&ctx(true, true, &applied)),
            Some(TrackOpCommand::SelectStreams(sel(
                Some("st0"),
                Some("st1"),
                None
            )))
        );
        let sn2 = gst::Seqnum::next();
        ops.selection_dispatched(sn2, sel(Some("st0"), Some("st1"), None));

        // The stale confirmation (sn1, reporting the superseded audio=2) must
        // settle neither by its seqnum nor by content.
        ops.streams_selected(sn1, &applied);
        assert!(ops.selecting.is_some());
        // The superseding one settles on its own seqnum.
        ops.streams_selected(sn2, &sel(Some("st0"), Some("st1"), None));
        assert!(ops.selecting.is_none());
    }

    #[test]
    fn paused_switch_refreshes_exactly_once() {
        // A paused subtitle switch dispatches the selection, then a single
        // re-emit flush once it confirms. The flushing seek re-prerolls, so the
        // cue composites before ASYNC_DONE, one flush is enough, no retry.
        let mut ops = TrackOps::new();
        let applied = sel(Some("st0"), Some("st1"), None);
        ops.request(
            TrackKind::Subtitle,
            Some("st2".to_string()),
            applied.clone(),
        );
        assert!(ops.pump(&ctx(true, true, &applied)).is_some());
        let sn = gst::Seqnum::next();
        ops.selection_dispatched(sn, sel(Some("st0"), Some("st1"), Some("st2")));
        let applied = sel(Some("st0"), Some("st1"), Some("st2"));
        assert_eq!(
            ops.pump(&ctx(true, true, &applied)),
            Some(TrackOpCommand::RefreshSeek)
        );
        ops.refresh_dispatched(gst::Seqnum::next());

        // The selection confirms and the flush completes, nothing is re-queued.
        ops.streams_selected(sn, &applied);
        assert!(ops.refresh_done());
        assert_eq!(ops.pump(&ctx(true, true, &applied)), None);
        assert!(!ops.has_dispatchable_work());
    }

    #[test]
    fn async_done_settles_refresh_by_exclusivity() {
        let mut ops = TrackOps::new();
        // No refresh out: an unrelated ASYNC_DONE is not a refresh completion.
        assert!(!ops.refresh_done());
        // GstBin's aggregated ASYNC_DONE carries a fresh seqnum, so the next
        // one settles the in-flight refresh regardless of seqnums.
        ops.refresh_dispatched(gst::Seqnum::next());
        assert!(ops.refresh_done());
        assert!(ops.refreshing.is_none());
    }

    #[test]
    fn new_collection_invalidates_in_flight_selection() {
        // A reload posts a new stream collection, the in-flight selection
        // targeted the old one (its stream ids are gone) so it can never
        // confirm. `invalidate_in_flight` abandons it deterministically, the
        // job the removed watchdog used to do on a timeout.
        let mut ops = TrackOps::new();
        ops.selection_dispatched(
            gst::Seqnum::next(),
            sel(Some("st0"), Some("st1"), Some("st2")),
        );
        ops.refresh_dispatched(gst::Seqnum::next());
        assert!(ops.selecting.is_some());

        ops.invalidate_in_flight();
        assert!(ops.selecting.is_none());
        assert!(ops.refreshing.is_none());
    }
}
