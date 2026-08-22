//! The pure routing decisions, separated from the pipeline calls that act on
//! them so the invariants are unit-testable without a live pipeline.

use tracing::warn;

use crate::routing::StreamKind;

pub(crate) mod replay;
pub(crate) mod route;
pub(crate) mod select;
pub(crate) mod selection_replay;
pub(crate) mod text_seat;

/// Seek flags for a `rate`. TRICKMODE lets decoders drop frames to keep up:
/// right for fast-scrub, wrong for pitch-corrected speed playback where
/// scaletempo wants every frame. Only high forward rates and reverse (which
/// can't be decoded frame-complete anyway) enable it, so a 1.25x/1.5x/2x
/// "watch faster" stays full quality.
pub(crate) fn seek_flags_for(rate: f64) -> gst::SeekFlags {
    let mut flags = gst::SeekFlags::FLUSH;
    if rate < 0.0 || rate > 2.0 {
        flags |= gst::SeekFlags::TRICKMODE;
    }
    flags
}

/// A load's start rate, made safe for `gst_event_new_seek`, which asserts
/// `rate != 0.0` and returns NULL (the binding then panics on it and the
/// panic kills the worker thread for good). Field: a sender's Load carried
/// `speed: 0.0`. Coerced to 1.0 rather than refused so the start position is
/// still honoured; a sender that means "paused" pauses the transport instead.
pub(crate) fn sanitize_start_rate(rate: f64) -> f64 {
    if rate.is_finite() && rate != 0.0 {
        rate
    } else {
        warn!(rate, "invalid start rate, playing at 1.0x instead");
        1.0
    }
}

/// Whether a subtitle transition must park the live text branches before
/// its `SELECT_STREAMS` goes out (see `FcastPlaybin::dispatch_selection`).
///
/// The answer is the OR of two inputs. A subtitles-off parks because the
/// deselected stream must stop feeding a renderer about to have nothing to
/// render. A replace parks because the outgoing branch must hand its slot
/// over before the incoming one can take it. Decided from the transition
/// alone, never a state read.
///
/// A flush is deliberately not used here. Sending a serialized FLUSH_STOP
/// into a live text branch can deadlock against a streaming thread's
/// sticky-event re-push cascade (an ABBA on the pad stream lock). What the
/// flush was for is carried without it. The outgoing mid-push is woken by
/// the disposal that follows the park, and the backlog leaves with the
/// detached queue.
pub(crate) fn park_text_before_dispatch(subtitle_off: bool, replacing: bool) -> bool {
    subtitle_off || replacing
}

/// Whether applying `selected_ids` drops video entirely (the video-chain
/// deactivation case), as opposed to a video-to-video switch, whose new
/// id is not routed yet and would otherwise look like "no video". An
/// empty `collection_video_ids` (nothing advertised yet, see
/// [`crate::selection::SelectionEngine::video_ids`]) means kinds are
/// unknowable, so never deactivate then.
pub(crate) fn deselects_video(
    video_linked: bool,
    collection_video_ids: &[String],
    selected_ids: &[String],
) -> bool {
    video_linked
        && !collection_video_ids.is_empty()
        && !collection_video_ids
            .iter()
            .any(|vid| selected_ids.contains(vid))
}

/// The state a dynamically (re)activated element joins the pipeline at.
/// Cap at PAUSED while a transition is in flight (the commit's child
/// walk lifts it the rest of the way with the fresh base_time), match
/// the pipeline exactly otherwise (see `Inner::join_state`).
pub(crate) fn join_state(current: gst::State, pending: gst::State) -> gst::State {
    if pending == gst::State::VoidPending {
        current
    } else {
        pending.min(gst::State::Paused)
    }
}

/// Whether a decodebin3 output pad may be routed. Only during a preroll
/// (pending at least PAUSED) or in a settled pipeline at PAUSED or
/// above. Anything else is a straggler from a superseded load.
pub(crate) fn pad_accepting(current: gst::State, pending: gst::State) -> bool {
    pending >= gst::State::Paused
        || (pending == gst::State::VoidPending && current >= gst::State::Paused)
}

/// Whether parked text may join its consumer tail. Only in a settled
/// pipeline at PAUSED or above (linking mid-transition splices a
/// reconfiguration into the async preroll and wedges it under churn).
pub(crate) fn text_may_link(current: gst::State, pending: gst::State) -> bool {
    current >= gst::State::Paused && pending == gst::State::VoidPending
}

/// What a bus error from a live external subtitle input means (see
/// `Inner::handle_external_error` for the mechanism).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ExternalErrorAction {
    /// Genuine failure. Detach and report `ExternalSubtitleFailed`.
    Fail,
    /// A transport race. The source is fine, only its task stopped (a
    /// deselect or a flush caught it mid-push). Keep the input
    /// attached. The join-time replay seek restarts it (idempotent,
    /// so a dying input's error burst needs no debounce).
    Recover,
}

/// Decide an external input's error by its cause, read from the error
/// message's debug string. A transport race (a deselect or one of our
/// own flushes catching the source mid-push) dies with reason
/// not-linked or flushing and recovers in place. Everything else
/// (resource, decode, network) fails fast. Selection state plays no
/// part. decodebin3's auto-select can route, join and show a fresh
/// external before anyone asked for it, so heuristics on selection
/// misclassify a flush-killed healthy input as a genuine failure.
pub(crate) fn external_error_action(debug_info: Option<&str>) -> ExternalErrorAction {
    let transport_race = debug_info
        .is_some_and(|d| d.contains("reason not-linked") || d.contains("reason flushing"));
    if transport_race {
        ExternalErrorAction::Recover
    } else {
        ExternalErrorAction::Fail
    }
}

/// What the subtitle consumer arm carries for a stream with these caps.
///
/// A [`ConsumerStreamFormat::Text`] stream's buffers are readable strings
/// the transport turns into cues. A [`ConsumerStreamFormat::Bitmap`]
/// stream's buffers are opaque bytes it forwards untouched.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum ConsumerStreamFormat {
    Text(super::SubtitleTextFormat),
    Bitmap(super::BitmapSubFormat),
}

/// Whether the subtitle consumer arm can carry a stream with these caps,
/// and in which format.
///
/// The caps gate the link loop applies before building a consumer branch,
/// re-applied by the transport per sample. `None` means the branch stays
/// parked and `PlaybinEvent::SubtitleTrackUnsupported` is emitted (loud
/// degradation). Real cases: raw ASS/SSA, `subpicture/x-xsub`, and any
/// subpicture format unimplemented or levered off. `kind_from_caps_name`
/// is deliberately unchanged. Those streams stay classified as Text so
/// their pads keep being routed and parked, which decodebin3's drain of a
/// deselected sparse stream needs (see `RoutedStream::park_pad`).
///
/// The text arm takes `text/x-raw` in `utf8` or `pango-markup` and nothing
/// else. A `text/x-raw` with no `format` field is taken as utf8. The field
/// is optional, and treating its absence as a capability failure would
/// turn a renderable stream into a user-visible error.
///
/// The bitmap arm answers for the [`super::BitmapSubFormat`] caps, and
/// only when the format is implemented
/// ([`super::bitmap_format_implemented`]). Anything else gets `None`, which
/// is the loud refusal a subpicture stream with no decoder behind it has to
/// get.
///
/// Cue-IR is not a caps format and is deliberately absent here. A parser
/// in `text-format=cue-ir` mode negotiates `text/x-raw, format=utf8` and
/// attaches the styling as a buffer meta, so this gate needs no cue-ir
/// arm. The variant
/// is chosen per sample in [`super::Inner::item_from_sample`] from the
/// meta's presence. The bitmap arm cannot collide because it is reached
/// by caps name (`subpicture/*`) and a cue-IR stream's name is
/// `text/x-raw`.
pub(crate) fn consumer_stream_format(caps: &gst::CapsRef) -> Option<ConsumerStreamFormat> {
    let structure = caps.structure(0)?;
    if structure.name() == "text/x-raw" {
        return Some(ConsumerStreamFormat::Text(
            match structure.get::<&str>("format") {
                Ok("utf8") => super::SubtitleTextFormat::Utf8,
                Ok("pango-markup") => super::SubtitleTextFormat::PangoMarkup,
                Ok(_) => return None,
                Err(_) => super::SubtitleTextFormat::Utf8,
            },
        ));
    }
    let format = bitmap_sub_format(structure.name().as_str())?;
    super::bitmap_format_implemented(format).then_some(ConsumerStreamFormat::Bitmap(format))
}

/// The bitmap subtitle format a caps name denotes, implemented or not.
///
/// `subpicture/x-xsub` is deliberately absent. No decoder is planned, so it
/// stays a permanent member of the loud-refusal set.
pub(crate) fn bitmap_sub_format(name: &str) -> Option<super::BitmapSubFormat> {
    match name {
        "subpicture/x-pgs" => Some(super::BitmapSubFormat::Pgs),
        "subpicture/x-dvd" => Some(super::BitmapSubFormat::Vobsub),
        "subpicture/x-dvb" => Some(super::BitmapSubFormat::Dvb),
        _ => None,
    }
}

/// The pad offset that puts a text branch onto the A/V branches' running-time
/// line, in signed nanoseconds (routinely negative). Derived in
/// [`super::Inner::sync_text_running_time`]. Rate 1.0 takes an integer path so
/// the common case stays exact.
///
/// Wrong under a REVERSE segment, which measures running time from `stop`, so
/// the start term names the wrong end. That term is zero whenever both segments
/// share bounds (every load, seek and gapless swap here), leaving only an
/// adaptive demuxer's mid-stream track add during reverse play. Left as is
/// rather than guessed at, a reverse formula needs the stop bounds threaded
/// through and a measurement to check against.
pub(crate) fn text_pad_offset(
    video_start: i64,
    video_base: i64,
    video_rate: f64,
    text_start: i64,
    text_base: i64,
) -> i64 {
    let start_delta = text_start - video_start;
    let scaled = if video_rate == 1.0 {
        start_delta
    } else {
        (start_delta as f64 / video_rate) as i64
    };
    scaled + (video_base - text_base)
}

/// What the gapless EOS gates do with one EOS (see
/// [`super::Inner::gapless_eos_gate`]).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum EosGate {
    /// An EOS of this group already entered streamsynchronizer, so this one
    /// MUST follow or the group never completes and the pushing thread ssync
    /// parked (a multiqueue slot task) never wakes. Records nothing: the
    /// group is already committed and the item's end is not being consumed.
    SiblingPass,
    /// Nothing holds this EOS back. `commit` is the group to record as
    /// passing so the siblings still to come take the arm above (`None` for
    /// text, which bypasses ssync, and for an unknown group).
    Pass { commit: Option<gst::GroupId> },
    /// Held back. The two reasons stay apart for the log, and `pending` is
    /// what makes the drop a CONSUMED item end (see `SwapState::dropped_eos`).
    Drop { pending: bool, behind: bool },
}

/// The gapless EOS-hold decision, pure over everything the two gates read.
///
/// An EOS on a pad whose stream group is `pad_group` must be dropped while a
/// swap is pending (committed to a next item, nothing may end the pipeline)
/// or while the pad still carries a non-active group, either lagging the
/// active one or positively the RETIRED one (old-item drainage, see
/// [`super::Inner::retired_group`]). Unknowns on either side never drop: only
/// a positively known group mismatch is old-item drainage.
///
/// The sibling-pass arm outranks both, and is what makes the gate
/// all-or-nothing per group: `passing_group` is the group a previous EOS was
/// already let through with, and dropping a strict subset of a group's EOS
/// wedges streamsynchronizer forever. Only `av` pads take part, text bypasses
/// ssync entirely. The post-ssync gate passes `passing_group: None`: it sits
/// downstream of the group wait, so it has no group to complete.
pub(crate) fn gapless_eos_decision(
    pad_group: Option<gst::GroupId>,
    active_group: Option<gst::GroupId>,
    retired_group: Option<gst::GroupId>,
    passing_group: Option<gst::GroupId>,
    pending: bool,
    av: bool,
) -> EosGate {
    if av && pad_group.is_some() && pad_group == passing_group {
        return EosGate::SiblingPass;
    }
    let behind = match (pad_group, active_group) {
        (Some(pad_group), Some(active)) => pad_group != active,
        _ => false,
    } || (pad_group.is_some() && pad_group == retired_group);
    if pending || behind {
        return EosGate::Drop { pending, behind };
    }
    EosGate::Pass {
        commit: if av { pad_group } else { None },
    }
}

/// Stream kind from a GstStream's type flags, the primary classifier.
///
/// The order is the answer to a multi-flag type: VIDEO wins over AUDIO wins
/// over TEXT, and anything carrying none of the three is not routable. Every
/// collection walk and every pad classifier reads this one ladder, because
/// four copies of it is how a kind silently disagrees between the load path,
/// the gapless path and the router.
pub(crate) fn kind_from_stream_type(ty: gst::StreamType) -> Option<StreamKind> {
    if ty.contains(gst::StreamType::VIDEO) {
        Some(StreamKind::Video)
    } else if ty.contains(gst::StreamType::AUDIO) {
        Some(StreamKind::Audio)
    } else if ty.contains(gst::StreamType::TEXT) {
        Some(StreamKind::Text)
    } else {
        None
    }
}

/// Caps-name fallback for pads without a GstStream.
pub(crate) fn kind_from_caps_name(name: &str) -> Option<StreamKind> {
    // image/* is video: parsebin types image streams as VIDEO and
    // fimagedec decodes them into raw video frames.
    if name.starts_with("video/") || name.starts_with("image/") {
        Some(StreamKind::Video)
    } else if name.starts_with("audio/") {
        Some(StreamKind::Audio)
    } else if name.starts_with("text/") || name.starts_with("subpicture/") {
        Some(StreamKind::Text)
    } else {
        None
    }
}
