use std::sync::atomic::{AtomicBool, Ordering};

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

/// Correlates missing-plugin element messages with decodebin's follow-up "missing plugin" WARNING
/// (posted right after, on the same thread) so the user-facing warning can be dropped when the only
/// undecodable streams were non-media metadata that needs no decoder.
#[derive(Default)]
struct MissingPluginTracker {
    /// A real media stream had no decoder.
    saw_real: AtomicBool,
    /// Only a non-media metadata stream had no "decoder".
    saw_ignorable: AtomicBool,
}

/// Whether a missing-plugin element message is for a non-media metadata stream (e.g. qtdemux's
/// `meta/x-gst-fourcc-priv` for an unknown atom like `wide`), which needs no decoder and so should
/// not be reported as a missing codec.
fn missing_plugin_is_ignorable(msg: &gst::Message) -> bool {
    let Some(structure) = msg.structure() else {
        return false;
    };
    // Missing decoder/encoder messages carry the offending caps in `detail`; other kinds
    // (element/urisource/...) store a string there, so a failed caps read means "treat as real".
    let Ok(caps) = structure.get::<gst::Caps>("detail") else {
        return false;
    };
    !caps.is_empty() && caps.iter().all(|s| s.name().as_str().starts_with("meta/"))
}

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
/// disabled). Re-exported from `fcastplaybin`, whose selection engine owns
/// all dispatch/confirmation sequencing; indices exist only at the
/// protocol/GUI edge.
pub use fcastplaybin::TrackSelection;



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
    /// ASYNC_DONE with a fresh seqnum (fcastplaybin's selection engine
    /// relies on exclusivity instead).
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
        /// attribution). Never an external subtitle input: those errors are
        /// handled inside fcastplaybin (re-arm or `ExternalSubtitleFailed`).
        origin: fcastplaybin::ErrorOrigin,
        kind: MediaErrorKind,
        message: String,
        /// Diagnostic only (the failing source's URI, when it has one).
        failed_uri: Option<String>,
    },
    /// An attached external subtitle input failed for good and fcastplaybin
    /// already detached it (failed attach, bus error while shown, or its
    /// stream never materialized within the crate's watchdog). The
    /// application drops its catalog entry and reports `ResourceNotFound`.
    ExternalSubtitleFailed {
        id: fcastplaybin::ExternalSubId,
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

/// Rebuild the stream list for a new collection with STABLE positions:
/// every stream of `previous` that is still advertised keeps its index
/// (adopting the collection's fresh `gst::Stream` object, whose tags may
/// have changed), streams that left are dropped in place, and newcomers
/// append in collection order. Positions are the protocol/GUI track ids,
/// which must not shift mid-item (see `handle_stream_collection`).
fn merge_streams_stable(previous: Vec<Stream>, collection: &gst::StreamCollection) -> Vec<Stream> {
    let fresh: Vec<gst::Stream> = collection.iter().collect();
    let sid_of = |s: &gst::Stream| s.stream_id().map(|id| id.to_string());

    let mut merged: Vec<Stream> = Vec::with_capacity(fresh.len());
    for old in previous {
        let old_sid = sid_of(&old.inner);
        if let Some(new) = fresh
            .iter()
            .find(|s| old_sid.is_some() && sid_of(s) == old_sid)
        {
            merged.push(Stream {
                title: stream_title(new),
                inner: new.clone(),
            });
        }
    }
    for new in &fresh {
        let new_sid = sid_of(new);
        let known = merged
            .iter()
            .any(|m| new_sid.is_some() && sid_of(&m.inner) == new_sid);
        if !known {
            merged.push(Stream {
                title: stream_title(new),
                inner: new.clone(),
            });
        }
    }
    merged
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
        let missing_plugins = MissingPluginTracker::default();
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
                    if let Ok(mp) = gst_pbutils::MissingPluginMessage::parse(msg) {
                        // qtdemux exposes non-media metadata streams (unknown atoms) as `meta/*`;
                        // decodebin then reports "no decoder" for them even though none is
                        // needed. Note it for the follow-up warning and don't cry wolf.
                        if missing_plugin_is_ignorable(msg) {
                            debug!(detail = %mp.installer_detail(), "Ignoring missing plugin for non-media stream");
                            missing_plugins.saw_ignorable.store(true, Ordering::SeqCst);
                        } else {
                            error!(detail = %mp.installer_detail(), desc = %mp.description(), "GStreamer missing plugin");
                            missing_plugins.saw_real.store(true, Ordering::SeqCst);
                        }
                    }
                    true
                }
                MessageView::Warning(warning) => {
                    if warning.error().matches(gst::CoreError::MissingPlugin) {
                        let real = missing_plugins.saw_real.swap(false, Ordering::SeqCst);
                        let ignorable = missing_plugins.saw_ignorable.swap(false, Ordering::SeqCst);
                        ignorable && !real
                    } else {
                        false
                    }
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
            E::ExternalSubtitleFailed { id } => PlayerEvent::ExternalSubtitleFailed { id },
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

        // STABLE ORDER across collections of one load: a stream keeps its
        // position for as long as it is advertised, newcomers append. The
        // list position is the protocol/GUI track id, and decodebin3 does
        // NOT keep collection order stable when it rebuilds the collection
        // (an external subtitle attach can flip video/audio). Ids shifting
        // mid-item desynchronize the senders' TracksAvailable/TracksSelected
        // view: a TracksSelected relayed before the flip can never match a
        // TracksAvailable advertised after it unless a further selection
        // change happens to re-relay, which is exactly the stuck
        // track-state settle FAST used to flake on.
        self.streams = merge_streams_stable(std::mem::take(&mut self.streams), &collection);

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

        // The crate's selection engine already reconciled against this
        // collection (and abandoned unconfirmable in-flight work) when it
        // translated the message; give it a pump now that the receiver's
        // own bookkeeping is consistent too.
        self.pump_selection();
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
        // Track desires reset inside fcastplaybin (they are per-item and it
        // owns the engine): its load reset and teardown both clear them.
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
            self.fcast.cancel_selection_refresh();
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

    /// Handle a track-change request. Sequencing (latest-wins composition,
    /// serialization against in-flight work, confirmation, re-assertion
    /// when decodebin3's auto-select stomps it, the switch's re-emit flush
    /// and its hazards) all lives in fcastplaybin's selection engine; this
    /// only states the desire and pumps. Returns whether the currently
    /// displayed subtitle cue became stale. The caller should clear the
    /// overlay so the change registers visually, even while paused.
    pub fn request_track_change(&mut self, kind: TrackKind, sid: Option<StreamId>) -> bool {
        let applied = self.applied_track_selection();
        let stale_cue =
            kind == TrackKind::Subtitle && applied.subtitle.is_some() && sid != applied.subtitle;
        let slot = match kind {
            TrackKind::Video => fcastplaybin::TrackSlot::Video,
            TrackKind::Audio => fcastplaybin::TrackSlot::Audio,
            TrackKind::Subtitle => fcastplaybin::TrackSlot::Subtitle,
        };
        self.fcast
            .request_track(slot, fcastplaybin::TrackTarget::Stream(sid));
        self.pump_selection();
        stale_cue
    }

    /// Ask for an attached external subtitle input's stream, before or
    /// after it materializes in a collection: the engine parks the desire
    /// until the stream is advertised, then selects it and re-asserts it
    /// against decodebin3's collection-default auto-select. Replaces the
    /// application's parked-desire enforcement.
    pub fn request_external_subtitle(&mut self, handle: fcastplaybin::ExternalSubId) {
        self.fcast.request_track(
            fcastplaybin::TrackSlot::Subtitle,
            fcastplaybin::TrackTarget::ExternalSubtitle(handle),
        );
        self.pump_selection();
    }

    /// Dispatch pending track work now that the pipeline may have settled.
    /// Called from the state-change handler (a re-preroll finishing is what
    /// unblocks work parked behind it). The pump is otherwise driven event-
    /// driven: a new request, `streams_selected`, `async_done`, buffering
    /// completion, collection changes and refresh failure, no periodic
    /// poll.
    pub fn poll_track_ops(&mut self) {
        self.pump_selection();
    }

    /// Let the selection engine act, under the transport gate only this
    /// side knows (see `fcastplaybin::SelectionGate`).
    fn pump_selection(&mut self) {
        // Ask the pipeline whether an async state change (re-preroll, seek
        // preroll) is in progress instead of predicting from the kind of
        // change, mispredictions are what used to wedge this logic.
        let async_busy = self.fcast.has_async_transition();
        let (running, paused) = match self.state_machine.running() {
            Some(state) => (true, state == RunningState::Paused),
            None => (false, false),
        };
        self.fcast.pump_selection(fcastplaybin::SelectionGate {
            quiet: running && !async_busy,
            paused,
            seekable: self.seekable,
        });
    }

    /// A top-level `ASYNC_DONE`: the pipeline has re-prerolled and settled.
    pub fn async_done(&mut self) {
        // A flush (e.g. the subtitle re-emit) has re-prerolled and the pipeline
        // is settled again. If a subtitle switch happened while paused, its new
        // text branch may still be parked (it routed mid-flush, when the
        // pipeline wasn't settled), link it now that we're steady so the
        // re-emit's cue actually composites onto the frozen frame.
        self.fcast.poll_text_policy();
        // The in-flight refresh seek, if any, was settled by the crate when
        // it translated this ASYNC_DONE; dispatch whatever was parked
        // behind it.
        self.pump_selection();
    }

    /// The refresh seek job could not perform its seek (already recorded by
    /// the crate; this is the pump trigger).
    pub fn subtitle_refresh_failed(&mut self, _seqnum: gst::Seqnum) {
        self.pump_selection();
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

    /// Produce a graph snapshot of the pipeline for the inspector, delivered
    /// via `done`. Runs on the fcastplaybin worker so the graph walk is
    /// serialized against loads and teardowns (the walk reads every
    /// element's properties, and racing the per-load audio sink's finalize
    /// double-freed in the sink back when this was a dot dump). `done` is
    /// invoked on the worker thread: hand the work off, do not block in it.
    pub fn request_graph_snapshot(
        &self,
        done: impl FnOnce(fcastplaybin::graph::GraphSnapshot) + Send + 'static,
    ) {
        self.fcast.debug_graph_async(Box::new(done));
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
        self.pump_selection();

        res
    }

    /// Live-attach an external subtitle input to the running pipeline.
    /// Returns the reserved id immediately, the attach itself runs on the
    /// playbin's worker thread (the source's `start()` blocks). The stream
    /// becomes selectable once decodebin3 announces the updated collection
    /// (always a later collection, mapped back with
    /// `external_stream_sid_of`). fcastplaybin babysits the input from
    /// here: deselect-race deaths re-arm internally under the same id, and
    /// a genuine failure (failed attach, error while shown, or no stream
    /// within its watchdog) comes back as
    /// `PlayerEvent::ExternalSubtitleFailed` with the input already
    /// detached.
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

    /// The GStreamer stream id of an attached external subtitle input, once
    /// its stream has appeared in the advertised collection. The id is
    /// URI-derived and therefore STABLE across fcastplaybin's internal
    /// re-arms of the input, so callers should remember it rather than
    /// re-query.
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
        // need no index mapping). The engine's confirmation/overtake logic
        // already ran when the crate translated this message; this mirror
        // only serves the protocol/GUI reads.
        self.selected = TrackSelection {
            video: video_sid.map(str::to_string),
            audio: audio_sid.map(str::to_string),
            subtitle: subtitle_sid.map(str::to_string),
        };

        // Dispatch the next queued operation now that this one confirmed. A
        // plain switch (subtitle, or an audio/video switch between already-
        // decoded streams) applies with no re-preroll and so posts no further
        // bus message, this is the event that advances the queue for it. If
        // the switch DID trigger a re-preroll, the pump's quiet gate (it
        // queries the pipeline's async state) holds the next op back until
        // the ASYNC_DONE/state-change handler pumps again, so this never
        // dispatches into a re-preroll.
        self.pump_selection();

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

    #[test]
    fn missing_plugin_ignorable_only_for_metadata_streams() {
        crate::gstreamer::init_for_tests();
        // gst_missing_decoder_message_new requires a non-null src element.
        let src = gst::ElementFactory::make("identity").build().unwrap();

        // qtdemux's non-media metadata stream: no decoder is needed, so it must not be reported as
        // a missing codec.
        let meta = gst::Caps::builder("meta/x-gst-fourcc-priv").build();
        let msg = gst_pbutils::MissingPluginMessage::builder_for_decoder(&meta)
            .src(&src)
            .build();
        assert!(missing_plugin_is_ignorable(&msg));

        // A real codec with no decoder must still be reported.
        let video = gst::Caps::builder("video/x-h264").build();
        let msg = gst_pbutils::MissingPluginMessage::builder_for_decoder(&video)
            .src(&src)
            .build();
        assert!(!missing_plugin_is_ignorable(&msg));
    }

    fn stream(sid: &str, ty: gst::StreamType) -> gst::Stream {
        gst::Stream::new(Some(sid), None, ty, gst::StreamFlags::empty())
    }

    fn collection(streams: &[gst::Stream]) -> gst::StreamCollection {
        let mut builder = gst::StreamCollection::builder(None);
        for s in streams {
            builder = builder.stream(s.clone());
        }
        builder.build()
    }

    fn sids(streams: &[Stream]) -> Vec<String> {
        streams
            .iter()
            .filter_map(|s| s.inner.stream_id().map(|id| id.to_string()))
            .collect()
    }

    #[test]
    fn stream_positions_stay_stable_across_collections() {
        crate::gstreamer::init_for_tests();
        let audio = stream("a0", gst::StreamType::AUDIO);
        let video = stream("v0", gst::StreamType::VIDEO);
        let text = stream("t0", gst::StreamType::TEXT);

        // Initial collection: [audio, video].
        let first = merge_streams_stable(Vec::new(), &collection(&[audio.clone(), video.clone()]));
        assert_eq!(sids(&first), ["a0", "v0"]);

        // decodebin3 rebuilds the collection in a DIFFERENT order and with a
        // new text stream (an external subtitle attach). Positions of the
        // known streams must not move (they are the advertised track ids);
        // the newcomer appends.
        let second = merge_streams_stable(
            first,
            &collection(&[video.clone(), audio.clone(), text.clone()]),
        );
        assert_eq!(sids(&second), ["a0", "v0", "t0"]);

        // A stream leaving (external detached) drops in place; the rest
        // keep their positions.
        let third = merge_streams_stable(second, &collection(&[video, audio]));
        assert_eq!(sids(&third), ["a0", "v0"]);
    }
}
