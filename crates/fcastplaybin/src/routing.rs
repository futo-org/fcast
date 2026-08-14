//! Inputs and routing: adding urisourcebin inputs, and linking,
//! parking and unrouting decodebin3's output pads.

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use anyhow::{Context, Result, anyhow};
use gst::prelude::*;
use tracing::{debug, info, warn};

use crate::{
    FcastPlaybin, Inner,
    api::{ExternalSubId, SubtitleFeedItem},
    decisions,
    external::ExternalInput,
    flush::{FlowStage, FlushReason},
    hands,
    hands::{Effect, Lane},
    jobs::{ChainJoinJob, Job},
    selection,
};

/// How many cues a text park keeps for its join (see
/// [`Inner::parked_text_cues`]).
///
/// Sized off the shape it exists for, not off a guess. The window it has to
/// span is bring-up, and what crosses in bring-up is bounded by how far the
/// demuxer's output position can run ahead of a playhead that has not started
/// (however much the sinks preroll plus whatever the queues between hold).
/// Measured on the whole-period fixture that is 3 cues, against captures worth
/// about 6 seconds of media. Sixty-four leaves an order of
/// magnitude over both and costs, at rest, one small deque of buffer
/// references per parked text pad.
const PARKED_TEXT_CUES: usize = 64;

/// How old a parked cue may be, at the moment its branch joins, and still be
/// worth showing (see [`Inner::take_parked_text_cues`]).
///
/// Two seconds is chosen against the two joins that reach the replay. A
/// bring-up join is milliseconds behind everything it lost, so the bound is
/// not what decides that case, it is slack. A join that ends a LONG park is
/// where the bound does the work, and there the park is still consuming at the
/// demuxer's pace, so "consumed in the last two seconds" means "about the
/// playhead" and nothing older can come back as a stale cue.
///
/// The margin is against measurement, not against a guess: two captures of this
/// defect joined 0.42 s and 0.51 s after the load, and the fixture joins in
/// 0.03 s. A bring-up slow enough to exceed this loses its opening cues exactly
/// as it did before the repair, so the bound can cost the fix and can never
/// cost more than the fix.
const PARKED_TEXT_REPLAY_WINDOW: Duration = Duration::from_secs(2);

/// Cues handed to the consumer by [`Inner::take_parked_text_cues`], for
/// tests (see [`FcastPlaybin::parked_text_cues_replayed`]).
static PARKED_TEXT_CUES_REPLAYED: AtomicU64 = AtomicU64::new(0);

/// Reader for [`PARKED_TEXT_CUES_REPLAYED`], for the `#[doc(hidden)]`
/// accessor on [`FcastPlaybin`].
pub(crate) fn parked_text_cues_replayed() -> u64 {
    PARKED_TEXT_CUES_REPLAYED.load(Ordering::SeqCst)
}

/// A byte counter on one input stream's parsed data, for bitrate
/// inspection (see [`FcastPlaybin::stream_io_stats`]). The probe lives on
/// the input's source pad and is removed with the input.
pub(crate) struct StreamTap {
    /// The input element's source pad (one parsed elementary stream).
    pub(crate) pad: gst::Pad,
    pub(crate) bytes: Arc<AtomicU64>,
    probe: Option<gst::PadProbeId>,
    /// Drain watch: whether this pad has pushed EOS into decodebin3 (reset
    /// by SEGMENT/STREAM_START, i.e. by seeks and item switches). The
    /// gapless swap fires when every main-input pad is drained.
    saw_eos: Arc<AtomicBool>,
    /// The EVENT_DOWNSTREAM probe maintaining `saw_eos`.
    event_probe: Option<gst::PadProbeId>,
}

/// One live input: an element (urisourcebin or caller-provided) whose source
/// pads are linked into decodebin3 request sink pads.
pub(crate) struct Input {
    pub(crate) element: gst::Element,
    /// Which load (or attach) this input belongs to. A bumped generation
    /// makes this input's errors [`ErrorOrigin::Stale`].
    pub(crate) generation: u64,
    /// External-subtitle bookkeeping, `None` for the main input.
    pub(crate) external: Option<ExternalInput>,
    /// decodebin3 request sink pads we hold for this input.
    pub(crate) db3_sink_pads: Vec<gst::Pad>,
    /// Per-stream byte counters (see [`StreamTap`]).
    pub(crate) taps: Vec<StreamTap>,
    /// Signal handlers to disconnect on removal.
    pub(crate) pad_added_sig: Option<gst::glib::SignalHandlerId>,
    /// Prepared (gapless) inputs only: the per-pad block probes holding
    /// buffers back until the swap. Cleared by the swap itself; removed
    /// here when a still-pending prepare is cancelled.
    pub(crate) block_probes: Vec<(gst::Pad, gst::PadProbeId)>,
}

impl Input {
    /// The stream ids this input's source pads have produced so far. Empty
    /// until the pads exist and carry their stream-start events (guaranteed
    /// by the time decodebin3 posts the collection containing the streams).
    pub(crate) fn stream_ids(&self) -> Vec<String> {
        self.element
            .src_pads()
            .iter()
            .filter_map(|pad| pad.stream_id().map(|sid| sid.to_string()))
            .collect()
    }

    /// The input's TEXT stream ids only.
    ///
    /// The subtitle slot may only ever hold a text stream, and nothing stops a
    /// caller handing in an audio file, or a container, as "the subtitle".
    /// `stream_ids` answers "every stream this input has", which is what the
    /// seek forwarding and the routing bookkeeping want. Anything speaking for
    /// the SUBTITLE slot has to ask this instead, or an audio-only external
    /// looks healthy and its audio stream gets advertised to the caller as a
    /// subtitle track.
    ///
    /// A pad whose kind is not classifiable yet is excluded rather than
    /// guessed at, so a caller polling this waits instead of advertising a
    /// stream it cannot classify. See [`Self::has_unclassified_stream`] for
    /// the other half of that distinction.
    pub(crate) fn text_stream_ids(&self) -> Vec<String> {
        self.element
            .src_pads()
            .iter()
            .filter(|pad| Inner::stream_kind_of(pad) == Some(StreamKind::Text))
            .filter_map(|pad| pad.stream_id().map(|sid| sid.to_string()))
            .collect()
    }

    /// Whether any of the input's pads has no classifiable kind yet, i.e. no
    /// `GstStream` on its sticky stream-start and no caps. Distinguishes "this
    /// input carries no text" from "it has not said yet", which the watchdog
    /// must not confuse.
    pub(crate) fn has_unclassified_stream(&self) -> bool {
        self.element
            .src_pads()
            .iter()
            .any(|pad| Inner::stream_kind_of(pad).is_none())
    }

    pub(crate) fn is_external(&self, id: ExternalSubId) -> bool {
        self.external.as_ref().is_some_and(|e| e.id == id)
    }
}

/// A decodebin3 output stream routed into a chain.
///
/// Audio and video pass through streamsynchronizer (`ssync_*` are `Some`).
/// TEXT deliberately BYPASSES it (`ssync_*` are `None`) and links from
/// `db3_src_pad` directly: streamsynchronizer syncs ALL its sink pads, so a
/// sparse text stream through it stalls video/audio on every flushing seek's
/// re-preroll (no text buffer at the seek target to advance the sync) and
/// the pipeline hangs ASYNC. Text is timestamped against video by the cue
/// renderer, so it needs no ssync synchronization.
pub(crate) struct RoutedStream {
    pub(crate) db3_src_pad: gst::Pad,
    /// A/V only: the streamsynchronizer request sink pad (released on
    /// unroute) and its paired src pad feeding the chain. `None` for text.
    ssync_sink: Option<gst::Pad>,
    pub(crate) ssync_src: Option<gst::Pad>,
    /// The live chain entry this stream is linked to: the A/V chain head, or
    /// the text queue feeding the branch's appsink.
    pub(crate) downstream: Option<gst::Pad>,
    /// Text only: the parking sink's pad while the stream is parked. Parked
    /// text must be CONSUMED, not left unlinked: decodebin3 cannot finish a
    /// deselected sparse stream's drain into an unlinked pad, and a blocked
    /// drain holds up the whole selection reconfiguration. Exactly one of
    /// `downstream`/`park_pad` is Some for text streams.
    pub(crate) park_pad: Option<gst::Pad>,
    /// Text only: the per-stream parking `fakesink` behind `park_pad`. Must
    /// exist only WHILE its stream does: GstBin EOS aggregation requires an
    /// EOS from every sink child regardless of state, so a permanent parking
    /// sink that sees no data would swallow the pipeline's EOS forever. A
    /// per-stream sink receives its stream's drain, EOS included.
    pub(crate) park_sink: Option<gst::Element>,
    /// Text only: the per-stream `queue` in front of the branch's appsink
    /// while the stream is live. Load-bearing twice over: (a) it decouples the
    /// decodebin3 text slot from whatever the tail does with a cue, without
    /// which that slot's src pad can sit mid-push and stall slot
    /// (de)activation for the media's cue spacing (subtitleoverlay used to
    /// prefetch-block the next cue outright), and (b) it must NOT outlive the
    /// stream, or a tail stays wired across loads with stale caps and the next
    /// preroll wedges.
    pub(crate) tqueue: Option<gst::Element>,
    /// Text only: the per-stream `appsink` this branch ends in, the tail that
    /// feeds [`FcastPlaybin::set_subtitle_consumer`].
    /// Lives and dies with `tqueue` (same reason (b): a tail left wired across
    /// loads carries stale caps into the next preroll), and is the anchor the
    /// arm-dispatched graph reads use to recognise a LIVE consumer branch
    /// ([`Inner::observed_seat_occupant`],
    /// [`Inner::subtitle_origin_matches_video`]).
    pub(crate) appsink: Option<gst::Element>,
    /// The group id of the last STREAM_START this pad carried. An EOS on a
    /// pad whose group is BEHIND the pipeline's active group belongs to a
    /// previous item draining out during a gapless switch and is dropped
    /// (uridecodebin3 keeps its gapless EOS drop open until every output
    /// pad has flipped to the new group, this is the per-pad equivalent).
    pub(crate) group: Option<gst::GroupId>,
    /// Text only. The link policy's dead-branch reclaim took this branch out
    /// because its pad carried no sticky segment with nothing left upstream to
    /// send one. While that stays true the entry must not relink (it holds the
    /// one-live-branch slot against the stream that can actually render,
    /// forever, since routed order is stable). A segment appearing on the pad
    /// is proof of life and clears the verdict.
    pub(crate) evicted_dead: bool,
    /// Text only. decodebin3 exposed ANOTHER output pad carrying this entry's
    /// stream id, so this pad is the one it left behind
    /// (gstdecodebin3.c:3169-3183 / 4761-4784).
    ///
    /// NOT permanent, and that correction is the walk-back's whole root cause.
    /// This was written as a permanent verdict on the belief that
    /// decodebin3 only ever replaces an output with a LATER one and never
    /// comes back, so no proof of life could exist ("the sticky segment on
    /// a superseded pad is whatever it held before the flush, so 'a segment
    /// appeared' says nothing"). The segment half of that is still true.
    /// The conclusion was not: decodebin3 recycles outputs in BOTH
    /// directions. `gst_decodebin_get_slot_for_input_stream_locked` takes
    /// the LOWEST-INDEXED unused compatible slot (`:3874-3886`, "Re-using
    /// existing unused slot 2") and `db_output_stream_reconfigure`
    /// (`:4229`) re-points the EXISTING ghost pad at it, emitting no
    /// pad-added. Which direction a re-enable takes depends only on whether
    /// the previous input's slot has been released yet, so an off/on that
    /// drains the old input first walks BACK onto the pad this flag had
    /// condemned.
    ///
    /// The proof of life that does exist is a BUFFER: a pad decodebin3 has
    /// abandoned carries none, and one it has re-pointed at a live slot
    /// carries them at the stream's cadence. [`Self::last_buffer`] records it,
    /// and the flow reclaim clears this flag on the pad that is demonstrably
    /// being fed. The anti-thrash property the permanent verdict was protecting
    /// survives because that predicate is self-stabilising: seating the pad
    /// that carries data leaves the loser carrying none, so the loser can
    /// never justify taking the seat back.
    pub(crate) superseded: bool,
    /// Text only. The [`Inner::text_flow_ticket`] value stamped by the most
    /// recent buffer to cross [`Self::db3_src_pad`], or 0 if none ever has.
    ///
    /// Shared with the pad probe that writes it, which is why it is an
    /// `Arc<AtomicU64>` rather than a plain field: the probe runs on a
    /// streaming thread and must take no crate lock (the routing lock is held
    /// across pipeline surgery, and a probe that waited on it would park a
    /// multiqueue slot task inside the crate).
    pub(crate) last_buffer: Arc<AtomicU64>,
    /// Text only. Whether this entry ever held a consumer branch. A FIRST
    /// join is the bring-up the park-replay window exists to repair, and it
    /// replays everything the ring kept regardless of age: the 2 s wall-clock
    /// window was calibrated on idle bring-ups and a loaded box joins later
    /// than that, losing a whole-period track's opening for good (`EMB 00`,
    /// dash_testbed under parallel load). Later joins keep the window: their
    /// staleness question really is "what is around the playhead".
    pub(crate) ever_joined: bool,
    /// Text only. Whether an EOS has crossed [`Self::db3_src_pad`] since the
    /// last STREAM_START or FLUSH_STOP on it, i.e. whether decodebin3 has
    /// ENDED the slot this output ghosts.
    ///
    /// The output-side twin of [`Inner::input_eos_sids`], and the instrument
    /// the seat policy had no equivalent of. In upstream-selection mode a
    /// re-select makes decodebin3 build a FRESH slot for the re-added input and
    /// CLEAR the old one, EOSing it (`remove_input_stream` /
    /// "Sending EOS to unused slot", gstdecodebin3.c:1232/1313) while leaving
    /// the old OUTPUT pad ghosted onto it. That pad can never carry another
    /// buffer, and nothing else says so: its sticky SEGMENT survives (so the
    /// segmentless-holder rule cannot see it), and it carries no buffer either
    /// way (so the flow reclaim cannot order it against a rival that has not
    /// carried one yet). Without this flag the crate seats the dead pad, the
    /// one-live-branch rule then refuses every live rival, and the track
    /// renders nothing for the rest of the item.
    ///
    /// CLEARED BY PROOF OF LIFE, never latched. STREAM_START is decodebin3
    /// re-pointing this ghost at a live slot (the walk-back does exactly
    /// that, with no pad-added to announce it) and FLUSH_STOP is a seek
    /// restarting the stream; both mean the pad is in play again. That is what
    /// keeps a legitimate end-of-item EOS from being a permanent verdict on a
    /// pad the next item reuses.
    pub(crate) saw_eos: Arc<AtomicBool>,
    pub(crate) kind: StreamKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StreamKind {
    Video,
    Audio,
    Text,
}

/// What decodebin3 is ACTUALLY pushing right now, per kind, read off the
/// stickies of the pads this crate has routed (see
/// [`Inner::probe_routed_selection`]).
///
/// The point of probing at all: `STREAMS_SELECTED` is a MESSAGE, and a
/// message can be lost while the selection it would have confirmed is
/// perfectly applied. The pads carry the truth either way.
#[derive(Debug, Default)]
pub(crate) struct RoutedSids {
    pub(crate) video: Vec<String>,
    pub(crate) audio: Vec<String>,
    pub(crate) text: Vec<String>,
}

impl RoutedSids {
    /// Whether every slot `target` NAMES is live on a routed pad of its kind.
    ///
    /// An EMPTY slot is not probed, and that asymmetry is deliberate: a
    /// deselected stream keeps its routed entry (text parks, video is lifted),
    /// so "nothing of this kind is playing" is not observable this way at all.
    /// Reading an unprobeable slot as matched errs towards the harmless side,
    /// a synthetic confirmation of a selection that really was applied.
    pub(crate) fn matches(&self, target: &selection::TrackSelection) -> bool {
        let present = |live: &[String], want: &Option<String>| match want {
            None => true,
            Some(sid) => live.iter().any(|got| got == sid),
        };
        present(&self.video, &target.video)
            && present(&self.audio, &target.audio)
            && present(&self.text, &target.subtitle)
    }

    /// What to REPORT for a selection the pipeline never applied: per slot,
    /// the requested stream if it turns out to be live after all, else
    /// whatever of that kind actually is, else nothing.
    pub(crate) fn actual(&self, target: &selection::TrackSelection) -> selection::TrackSelection {
        let pick = |live: &[String], want: &Option<String>| match want {
            Some(sid) if live.iter().any(|got| got == sid) => Some(sid.clone()),
            _ => live.first().cloned(),
        };
        selection::TrackSelection {
            video: pick(&self.video, &target.video),
            audio: pick(&self.audio, &target.audio),
            subtitle: pick(&self.text, &target.subtitle),
        }
    }
}

/// The mutable, per-load state of the dynamic pad graph: the live [`Input`]s,
/// the decodebin3 output streams routed into the fixed chains, and the
/// generation / external-id bookkeeping. Guarded by one mutex. Distinct from
/// [`Core`] (the decodebin3 + streamsynchronizer elements themselves).
#[derive(Default)]
pub(crate) struct RoutingState {
    pub(crate) inputs: Vec<Input>,
    /// Routed streams. Text entries with `downstream: None` are parked
    /// awaiting the link policy (`poll_text_policy`).
    pub(crate) routed: Vec<RoutedStream>,
    /// Stream ids of the VIDEO streams in the latest advertised collection
    /// (cached by the bus translation, cleared per load). Lets
    /// [`FcastPlaybin::select_streams`] tell a selection that DROPS video
    /// entirely (video-chain deactivation needed) from a video-to-video
    /// switch, whose new id is not routed yet and would otherwise look like
    /// "no video".
    pub(crate) collection_video_ids: Vec<String>,
    pub(crate) next_external_id: u64,
}

/// [`Inner::remove_input`] pads that got the flush pair, against those a
/// quiescence probe let through with nothing.
///
/// SENT is the cost side - every send forwards downstream and briefly de-PLAYs
/// both sinks - and it is nonzero on the default arm, because the skip that
/// would have driven it to zero was measured to break the same-URL re-attach
/// (see `Inner::remove_input`). SKIPPED only moves under
/// `FCAST_REMOVE_INPUT_FLUSH_SKIP`. Both are here so the next attempt at the
/// skip is measured rather than argued.
static REMOVE_INPUT_PAIRS_SENT: AtomicU64 = AtomicU64::new(0);

static REMOVE_INPUT_PAIRS_SKIPPED: AtomicU64 = AtomicU64::new(0);

impl Inner {
    /// urisourcebin configured the way uridecodebin3 configures its source
    /// handlers: parsed streams out. `use_buffering` (main input only)
    /// matches playbin3's `buffering` flag, whose messages drive the
    /// caller's state machine.
    pub(crate) fn make_urisourcebin(uri: &str, use_buffering: bool) -> Result<gst::Element> {
        let usb = gst::ElementFactory::make("urisourcebin")
            .property("uri", uri)
            .property("parse-streams", true)
            .property("use-buffering", use_buffering)
            .build()
            .context("creating urisourcebin")?;
        Ok(usb)
    }

    /// Add an input element to the pipeline and link its (dynamic) source
    /// pads into decodebin3 request pads as they appear.
    pub(crate) fn add_input(
        inner: &Arc<Inner>,
        element: gst::Element,
        generation: u64,
        external: Option<ExternalInput>,
    ) -> Result<()> {
        // Externals attach STATE-LOCKED and only join the pipeline's state
        // machinery once materialized (Job::AdoptSubState): a pipeline state
        // change that recurses into a still-plugging input deadlocks against
        // typefind's streaming thread (state lock held while pausing the
        // task, the task holding its stream lock while syncing the plugged
        // parser's state).
        let lock_until_materialized = external.is_some();
        if lock_until_materialized {
            element.set_locked_state(true);
        }
        inner
            .pipeline
            .add(&element)
            .context("adding input element")?;

        // Register the input BEFORE any pad can appear, so `link_input_pad`
        // always finds it for request-pad bookkeeping (detach releases those
        // pads later).
        inner.routing.lock().inputs.push(Input {
            element: element.clone(),
            generation,
            external,
            db3_sink_pads: Vec::new(),
            taps: Vec::new(),
            pad_added_sig: None,
            block_probes: Vec::new(),
        });

        let pad_added_sig = element.connect_pad_added({
            let inner = Arc::downgrade(inner);
            move |element, pad| {
                let Some(inner) = inner.upgrade() else { return };
                if let Err(err) = Inner::link_input_pad(&inner, element, pad) {
                    warn!(?err, pad = %pad.name(), "failed to link input pad to decodebin3");
                }
            }
        });
        {
            let mut routing = inner.routing.lock();
            if let Some(input) = routing.inputs.iter_mut().find(|i| i.element == element) {
                input.pad_added_sig = Some(pad_added_sig);
            }
        }

        // Pads that already exist (pre-built elements may have static pads).
        for pad in element.src_pads() {
            if let Err(err) = Inner::link_input_pad(inner, &element, &pad) {
                warn!(?err, pad = %pad.name(), "failed to link existing input pad");
            }
        }

        let synced = if lock_until_materialized {
            // Locked children are skipped by the parent's state changes, so
            // drive the input to the pipeline's effective state explicitly.
            // ASYNC is the normal answer while its internals build.
            element
                .set_state(inner.join_state())
                .map(|_| ())
                .map_err(|err| anyhow!("driving the locked external input: {err}"))
        } else {
            element
                .sync_state_with_parent()
                .map_err(|err| anyhow!("{err}"))
        };
        if let Err(err) = synced {
            // Roll back: a half-attached input would keep posting errors
            // from inside the pipeline with nothing owning it.
            let mut routing = inner.routing.lock();
            if let Some(idx) = routing.inputs.iter().position(|i| i.element == element) {
                let input = routing.inputs.remove(idx);
                drop(routing);
                Inner::remove_input(inner, input);
            }
            return Err(err).context("syncing input element state");
        }
        Ok(())
    }

    /// The uridecodebin3 `link_src_pad_to_db3` recipe: request a decodebin3
    /// sink pad and link.
    fn link_input_pad(inner: &Arc<Inner>, element: &gst::Element, pad: &gst::Pad) -> Result<()> {
        let db3 = inner
            .core
            .lock()
            .as_ref()
            .map(|c| c.db3.clone())
            .ok_or_else(|| anyhow!("no dynamic core"))?;
        // A held external's buffers must never reach decodebin3 while its
        // stream is deselected (see `ExternalInput::hold_until_selected`).
        // Installed BEFORE the link so no buffer can slip through: this runs
        // on the element's streaming thread ahead of any push through `pad`.
        Inner::block_held_external_pad(inner, element, pad);
        let sinkpad = {
            // Serialized: see `Inner::db3_pad_request`.
            let _serial = inner.db3_pad_request.lock();
            db3.request_pad_simple("sink_%u")
        }
        .ok_or_else(|| anyhow!("decodebin3 gave no request sink pad"))?;
        pad.link(&sinkpad)
            .with_context(|| format!("linking {} to {}", pad.name(), sinkpad.name()))?;
        debug!(src = %pad.name(), sink = %sinkpad.name(), "linked input pad into decodebin3");

        // A stream that ends without ever producing DATA never gets a
        // decodebin3 multiqueue slot, and one slotless stream poisons the WHOLE
        // item. It can never be `all_streams_present`, so no collection holding
        // it becomes the output collection, every later SELECT_STREAMS is
        // accepted and silently discarded, no chain is built for the OTHER
        // streams and the pipeline never leaves ASYNC. Chain and proof in
        // UPSTREAM-GSTREAMER-ISSUES.md C15.
        //
        // decodebin3 slots an input stream on a BUFFER or a GAP and nothing
        // else (its EOS arm only sets `saw_eos`), so one zero-duration GAP
        // ahead of the EOS is the whole repair, the seeding
        // `Inner::seed_slot_for_held_pad` already does for a held external's
        // cue-less subtitle. Pushed from inside the EOS probe so it lands on
        // the pad's own streaming thread (serialized events must not come from
        // anywhere else) and BEFORE the EOS, since nothing may follow that. The
        // nested push is legal on the recursive stream lock.
        // Lever: `FCAST_NO_EMPTY_STREAM_SLOT_SEED`.
        if std::env::var_os("FCAST_NO_EMPTY_STREAM_SLOT_SEED").is_none() {
            let produced = AtomicBool::new(false);
            pad.add_probe(
                gst::PadProbeType::BUFFER
                    | gst::PadProbeType::BUFFER_LIST
                    | gst::PadProbeType::EVENT_DOWNSTREAM,
                move |pad, info| {
                    match &info.data {
                        Some(gst::PadProbeData::Buffer(_) | gst::PadProbeData::BufferList(_)) => {
                            produced.store(true, Ordering::Relaxed);
                        }
                        Some(gst::PadProbeData::Event(event)) => match event.view() {
                            // A GAP slots the stream by itself, so the seeding
                            // is owed only where neither ever arrives.
                            gst::EventView::Gap(_) => produced.store(true, Ordering::Relaxed),
                            // `swap` rather than a load, so a second EOS cannot
                            // seed twice. A slot survives a flush, so nothing
                            // resets this.
                            gst::EventView::Eos(_) if !produced.swap(true, Ordering::Relaxed) => {
                                debug!(
                                    pad = %pad.name(),
                                    "an input stream ended without ever producing data; \
                                     seeding its decodebin3 slot so it cannot strand the item"
                                );
                                Inner::seed_slot_for_held_pad(pad, None);
                            }
                            _ => {}
                        },
                        _ => {}
                    }
                    gst::PadProbeReturn::Ok
                },
            );
        }

        // Record input-side stream ends for the drained-resurrect park (see
        // `Inner::input_eos_sids`). EOS marks the stream drained, FLUSH_STOP
        // marks it restarted. A seek only flushes the streams it actually
        // restarts (a per-stream source leaves a deselected stream's pad
        // untouched), so the flag self-maintains across seeks. Installed
        // only while the park's lever is unset, so the lever restores the
        // untracked behavior wholesale.
        if std::env::var_os("FCAST_NO_DRAINED_RESURRECT_PARK").is_none() {
            pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, {
                let weak = Arc::downgrade(inner);
                move |pad, info| {
                    let Some(gst::PadProbeData::Event(event)) = &info.data else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let drained = match event.view() {
                        gst::EventView::Eos(_) => true,
                        gst::EventView::FlushStop(_) => false,
                        _ => return gst::PadProbeReturn::Ok,
                    };
                    let Some(inner) = weak.upgrade() else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let Some(sid) = pad
                        .sticky_event::<gst::event::StreamStart>(0)
                        .map(|event| event.stream_id().to_string())
                    else {
                        return gst::PadProbeReturn::Ok;
                    };
                    let mut drained_sids = inner.input_eos_sids.lock();
                    if drained {
                        debug!(%sid, "input stream drained (EOS into decodebin3)");
                        drained_sids.insert(sid);
                    } else {
                        debug!(%sid, "input stream restarted by a flush");
                        drained_sids.remove(&sid);
                    }
                    gst::PadProbeReturn::Ok
                }
            });
        }

        if !Inner::record_linked_input_pad(inner, element, pad, sinkpad.clone()) {
            // Only reachable for an input already removed (detach racing a
            // late pad). Release the pad we just took.
            warn!("pad appeared for an unregistered input; releasing");
            db3.release_request_pad(&sinkpad);
        }
        // The FIRST linked pad of an external input means its plugging
        // machinery is done: hand the state-locked input over to the
        // pipeline's state handling (see Job::AdoptSubState).
        let adopt = {
            let routing = inner.routing.lock();
            routing
                .inputs
                .iter()
                .find(|i| i.element == *element)
                .and_then(|input| {
                    let external = input.external.as_ref()?;
                    (input.db3_sink_pads.len() == 1).then_some((external.id, external.epoch))
                })
        };
        if let Some((id, epoch)) = adopt {
            inner.queue_job(Job::AdoptSubState { id, epoch });
        }
        // A FRESH INPUT PAD IS AN EDGE THE TEXT POLICY HAS TO SEE, and nothing
        // used to tell it.
        //
        // This is the demuxer ANSWERING a selection: in upstream-selection mode
        // a re-select is answered by exposing a pad, and the answer can be slow
        // (measured at 2.32 s after the send on a slow round trip).
        // Everything the policy would decide differently happens on the far
        // side of that pad: decodebin3 builds (or fails to build) an output for
        // it, the OLD slot for the same stream is cleared and EOSed, and the
        // seat the link loop took while waiting is now on a pad that will never
        // deliver.
        //
        // The polls that exist do not cover it. `route_db3_pad` asks when an
        // OUTPUT pad appears, and the whole defect is the case where decodebin3
        // builds none. The link loop's own follow-up asks at the seat, which
        // there was 2.3 s BEFORE this. Between them a receiver that polls on
        // events has no poll at all in the window that matters, and this is the
        // event.
        //
        // ASKED for, not performed: this runs on the input's streaming thread
        // (`pad-added`), where the policy's bin surgery must never run, and the
        // request is coalesced (see `Inner::request_text_policy_poll`), so an
        // input exposing many pads at once costs one job.
        // Lever: `FCAST_NO_INPUT_PAD_TEXT_POLL`.
        if std::env::var_os("FCAST_NO_INPUT_PAD_TEXT_POLL").is_none() {
            Inner::request_text_policy_poll(inner);
        }
        Ok(())
    }

    /// Bookkeeping for one linked input pad: the bitrate tap, the drain
    /// watch (gapless), and the input's pad lists. Returns `false` when the
    /// input is no longer registered (the caller releases the request pad).
    pub(crate) fn record_linked_input_pad(
        inner: &Arc<Inner>,
        element: &gst::Element,
        pad: &gst::Pad,
        sinkpad: gst::Pad,
    ) -> bool {
        // Bitrate inspection tap: count the stream's PARSED (compressed)
        // bytes, one relaxed atomic add per buffer. Callers poll cumulative
        // counters and compute rates from deltas (`stream_io_stats`).
        let bytes = Arc::new(AtomicU64::new(0));
        let probe = pad.add_probe(
            gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
            {
                let bytes = Arc::clone(&bytes);
                move |_pad, info| {
                    let n: usize = match &info.data {
                        Some(gst::PadProbeData::Buffer(buffer)) => buffer.size(),
                        Some(gst::PadProbeData::BufferList(list)) => {
                            list.iter().map(|b| b.size()).sum()
                        }
                        _ => 0,
                    };
                    bytes.fetch_add(n as u64, Ordering::Relaxed);
                    gst::PadProbeReturn::Ok
                }
            },
        );

        // Drain watch: track whether this pad has pushed EOS into
        // decodebin3 (a seek's SEGMENT or an item switch's STREAM_START
        // reset it). The gapless swap waits for every main-input pad to
        // drain.
        let saw_eos = Arc::new(AtomicBool::new(false));
        let event_probe = pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, {
            let saw_eos = Arc::clone(&saw_eos);
            let weak = Arc::downgrade(inner);
            move |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data {
                    match event.type_() {
                        gst::EventType::Eos => {
                            saw_eos.store(true, Ordering::SeqCst);
                            if let Some(inner) = weak.upgrade() {
                                Inner::note_input_pad_eos(&inner);
                            }
                        }
                        gst::EventType::StreamStart | gst::EventType::Segment => {
                            saw_eos.store(false, Ordering::SeqCst);
                        }
                        _ => {}
                    }
                }
                gst::PadProbeReturn::Ok
            }
        });

        let mut routing = inner.routing.lock();
        if let Some(input) = routing.inputs.iter_mut().find(|i| &i.element == element) {
            input.db3_sink_pads.push(sinkpad);
            input.taps.push(StreamTap {
                pad: pad.clone(),
                bytes,
                probe,
                saw_eos,
                event_probe,
            });
            true
        } else {
            drop(routing);
            if let Some(probe) = probe {
                pad.remove_probe(probe);
            }
            if let Some(probe) = event_probe {
                pad.remove_probe(probe);
            }
            false
        }
    }

    /// One main-input pad pushed EOS into decodebin3: when EVERY main-input
    /// pad has, the current item is fully drained and a pending gapless
    /// swap may proceed (the prepared input's blocked threads are parked on
    /// the swap gate).
    pub(crate) fn note_input_pad_eos(inner: &Arc<Inner>) {
        if !Inner::main_input_drained(inner) {
            return;
        }
        let mut state = inner.swap_gate.state.lock();
        if state.pending.is_some() && !state.drained {
            debug!("current input fully drained into decodebin3");
            state.drained = true;
            inner.swap_gate.cond.notify_all();
        }
    }

    /// The decodebin3 request sink pads the MAIN input(s) are linked into, i.e.
    /// which pads can be asked about upstream. Externals are excluded because
    /// decodebin3 flips into upstream-selection mode if ANY input answers TRUE,
    /// and an external subtitle IS the mix its own FIXME warns about.
    pub(crate) fn main_input_db3_sink_pads(&self) -> Vec<gst::Pad> {
        let routing = self.routing.lock();
        routing
            .inputs
            .iter()
            .filter(|input| input.external.is_none())
            .flat_map(|input| input.db3_sink_pads.iter().cloned())
            .collect()
    }

    /// Whether the current main input has pushed EOS on all its pads.
    pub(crate) fn main_input_drained(inner: &Arc<Inner>) -> bool {
        let current = inner.current_generation();
        let routing = inner.routing.lock();
        routing
            .inputs
            .iter()
            .filter(|i| i.generation == current && i.external.is_none())
            .any(|i| !i.taps.is_empty() && i.taps.iter().all(|t| t.saw_eos.load(Ordering::SeqCst)))
    }

    /// [`Inner::park_stream`] for a TEXT pad: the same non-blocking park, over
    /// a sink that KEEPS what it consumes so the join can put it back (see
    /// [`Inner::parked_text_cues`], and [`Inner::take_parked_text_cues`] for
    /// the other half).
    ///
    /// The contract that matters is the one it shares with the fakesink it
    /// replaces: `sync=false` so it never waits for a clock, `async=false` so
    /// it never gates a preroll, `drop=true` with `max-buffers` bounded so it
    /// can never park the pushing thread, and no SINK flag so it stays out of
    /// the bin's POSITION and DURATION folds. A park that can block is a
    /// pinned adaptive-demuxer output loop, which is the whole reason the park
    /// exists; keeping a copy of a cue must not cost that.
    ///
    /// `FCAST_NO_PARKED_TEXT_REPLAY` falls back to the discarding park, which
    /// is the A/B partner for the assertion in `dash_testbed.rs`.
    pub(crate) fn park_text_stream(
        self: &Arc<Self>,
        source: &gst::Pad,
    ) -> Result<(gst::Element, gst::Pad)> {
        if std::env::var_os("FCAST_NO_PARKED_TEXT_REPLAY").is_some() {
            return self.park_stream(source);
        }
        let sink = gst_app::AppSink::builder()
            .name(format!("fpb-textpark-{}", source.name()))
            .sync(false)
            .async_(false)
            .drop(true)
            .max_buffers(PARKED_TEXT_CUES as u32)
            .enable_last_sample(false)
            .build();
        sink.unset_element_flags(gst::ElementFlags::SINK);

        // Keyed by the PAD, not the stream id: the join reads the same key off
        // the same pad, and a pad outlives the moments where a stream id is
        // not yet readable.
        let key = source.name().to_string();
        let keep = {
            let weak = Arc::downgrade(self);
            move |sample: gst::Sample| {
                let Some(inner) = weak.upgrade() else { return };
                let mut parked = inner.parked_text_cues.lock();
                let ring = parked.entry(key.clone()).or_default();
                // Newest wins. A park nobody ever joins (subtitles off for the
                // whole item) would otherwise grow with the track.
                if ring.len() == PARKED_TEXT_CUES {
                    ring.pop_front();
                }
                ring.push_back((sample, Instant::now()));
            }
        };
        // BOTH callbacks, for the reason `build_text_consumer_tail` states at
        // length: below PLAYING the first buffer of every segment goes through
        // the preroll path and `new_sample` is never called for it. The first
        // cue of a bring-up window is exactly that buffer.
        //
        // And, exactly as there, the two are IDEMPOTENT rather than exclusive:
        // one buffer can be kept twice, once as a preroll and again as a
        // sample when the state advances. The replay then submits it twice
        // with identical bounds, which the renderer's single-active /
        // latest-start rule absorbs without a flicker. Deduplicating here
        // would mean comparing payloads, which is work proportional to a cue
        // for no visible difference.
        let preroll_keep = keep.clone();
        sink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    if let Some(sample) = sink.try_pull_sample(gst::ClockTime::ZERO) {
                        keep(sample);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .new_preroll(move |sink| {
                    if let Some(sample) = sink.try_pull_preroll(gst::ClockTime::ZERO) {
                        preroll_keep(sample);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let sink: gst::Element = sink.upcast();
        self.pipeline
            .add(&sink)
            .context("adding the text parking sink")?;
        sink.sync_state_with_parent()
            .context("syncing the text parking sink")?;
        let pad = sink.static_pad("sink").expect("appsink has a sink pad");
        source
            .link(&pad)
            .context("linking text into its parking sink")?;
        Ok((sink, pad))
    }

    /// Take what the park kept, for a text stream that has just joined its
    /// branch (see [`Inner::parked_text_cues`]), converted and ready to feed.
    ///
    /// Returns the cues to hand the consumer. THE CALLER FEEDS THEM, and it
    /// must do so with no crate lock held: the one join that reaches this
    /// runs under `Inner::routing`, and `feed_subtitle` calls the consumer,
    /// foreign code that this test suite is entitled to make block for
    /// seconds (`caller_bounded_switch`). Feeding under the lock wedged
    /// `pump_selection` (whose first read is that lock) behind a held
    /// renderer for the duration of the hold, which is exactly the property
    /// the caller-bounded test pins. The split keeps everything cheap under
    /// the lock (ring extraction, conversion, arming) and moves the only
    /// foreign call past it, the same discipline the join's own reports
    /// already follow ("PAST THE LOCK").
    ///
    /// # The window, and why it is wall clock
    ///
    /// The loss this repairs is a WALL-CLOCK window (from the pad being routed
    /// to the pipeline settling at PAUSED) that happens to contain an
    /// arbitrary amount of MEDIA, because the demuxer's output position runs
    /// ahead of a playhead that has not started. So the freshness test is the
    /// same axis as the window: a kept cue is replayed when it was consumed
    /// within [`PARKED_TEXT_REPLAY_WINDOW`] of this join, and dropped
    /// otherwise.
    ///
    /// That single rule covers both shapes without a clock query. A bring-up
    /// join is milliseconds behind the cues it lost, so everything replays. A
    /// join that follows a LONG park (subtitles off since the start, switched
    /// on at minute forty) replays only what the park consumed in the last
    /// couple of seconds, which is the material around the playhead and exactly
    /// what a viewer enabling subtitles should see. The item's opening cues,
    /// long expired, aged out of the ring on their own.
    ///
    /// The samples go through `Inner::item_from_sample` unchanged, so a
    /// replayed cue is bound-for-bound the cue the branch would have delivered
    /// with the same clip, the same UTF-8 gate and the same cue-IR meta.
    ///
    /// The counter, the arming and the log all happen HERE, at extraction:
    /// the suppression must beat the branch's first push (whose STREAM_START
    /// consumes it), and arming before the feed is strictly tighter than the
    /// old post-feed arm, where a slow consumer used to widen that race by the
    /// whole feed. The set fed and the set counted are the same either way, and
    /// the two halves run microseconds apart in one poll.
    pub(crate) fn take_parked_text_cues(
        self: &Arc<Self>,
        pad: &gst::Pad,
        redeliverable: bool,
        first_join: bool,
    ) -> Vec<SubtitleFeedItem> {
        let kept = self.parked_text_cues.lock().remove(pad.name().as_str());
        let held = kept.as_ref().map_or(0, |ring| ring.len());
        let Some(kept) = kept else {
            Self::report_an_empty_park(pad, 0);
            return Vec::new();
        };
        let now = Instant::now();
        // For an EXTERNAL stream (`redeliverable`), only cues on the VIDEO's
        // timeline may be fed: the park also captures the burst an input
        // pushes under its pre-alignment file segment (attach after a seek),
        // and replaying those shows the wrong cue at the video's position;
        // the realigning replay seek re-delivers them right. Measured:
        // `AAA00` fed with origin 0 against a video at 5 s, under parallel
        // load. NEVER for an embedded stream: nothing re-delivers a dropped
        // cue there, and a whole-period DASH track's opening has no second
        // copy anywhere (`EMB 00` lost to exactly this filter).
        let (_, video_origin) = self.video_timeline();
        let mut dropped_misaligned = 0usize;
        let items: Vec<SubtitleFeedItem> = kept
            .into_iter()
            // A FIRST join is the bring-up this replay exists to repair, and
            // it takes the whole ring: the wall-clock window was calibrated
            // on idle bring-ups (0.42 s / 0.51 s) and a loaded box joins
            // later, losing a whole-period track's opening for good. The
            // ring's own cap still bounds a long-parked track to its newest
            // 64 cues (see [`RoutedStream::ever_joined`]).
            .filter(|(_, at)| {
                first_join || now.saturating_duration_since(*at) <= PARKED_TEXT_REPLAY_WINDOW
            })
            .filter_map(|(sample, _)| Inner::item_from_sample(&sample, self.bitmap_subs))
            .filter(|item| match item {
                SubtitleFeedItem::Cue { origin, .. }
                    if redeliverable
                        && *origin != video_origin
                        && !Inner::misaligned_cue_gate_off() =>
                {
                    dropped_misaligned += 1;
                    false
                }
                _ => true,
            })
            .collect();
        if dropped_misaligned > 0 {
            debug!(
                pad = %pad.name(),
                dropped_misaligned,
                %video_origin,
                "dropped parked cues captured on another timeline; the realigning replay re-delivers them"
            );
        }
        if items.is_empty() {
            Self::report_an_empty_park(pad, held);
        }
        if !items.is_empty() {
            PARKED_TEXT_CUES_REPLAYED.fetch_add(items.len() as u64, Ordering::SeqCst);
            // Arm the branch's entry probe to let its own STREAM_START pass
            // without clearing what is about to be handed over.
            self.suppress_text_clear
                .lock()
                .insert(pad.name().to_string());
            debug!(
                pad = %pad.name(),
                replayed = items.len(),
                "replayed the cues the text park held through bring-up"
            );
        }
        items
    }

    /// FORENSICS: a join that found nothing to replay, and the ONE fact that
    /// tells the two reasons apart.
    ///
    /// "No `replayed the cues the text park held` line" is ambiguous in a
    /// capture and it cost this defect a whole round of misattribution: the
    /// park can be empty because nothing ever crossed the pad, or because
    /// everything that crossed it is stuck in decodebin3's single queue
    /// behind a LATCHED slot (see [`Inner::bring_up_parking_sink`]), and in
    /// the second case the heal that runs microseconds later
    /// destroys it. `held` separates "kept nothing" from "kept only
    /// unshowable records" (a zero-length twin is kept and correctly
    /// refused), and the slot read separates both from "the park was never
    /// reachable".
    ///
    /// Debug, one line per join, no counter: the counters that matter
    /// (`JOINS_INTO_AN_INACTIVE_BRANCH`, [`FcastPlaybin::slot_unlatches`])
    /// already exist and this line is what makes them readable together.
    fn report_an_empty_park(pad: &gst::Pad, held: usize) {
        debug!(
            pad = %pad.name(),
            held,
            slot_latched = ?Inner::slot_reads_latched(pad),
            "the text park had nothing showable to replay at the join; a latched slot here \
             means the opening is queued inside decodebin3 and about to be dropped by the \
             heal, an unlatched one means nothing ever crossed the pad"
        );
    }

    /// Whether this pad's branch may skip ONE `Clear`, consuming the right to
    /// (see [`Inner::take_parked_text_cues`] and `build_text_consumer_tail`).
    pub(crate) fn take_text_clear_suppression(&self, pad: &gst::Pad) -> bool {
        self.suppress_text_clear.lock().remove(pad.name().as_str())
    }

    /// Forget what a text park kept, for a pad that is leaving without a join.
    pub(crate) fn forget_parked_text_cues(&self, pad: &gst::Pad) {
        self.parked_text_cues.lock().remove(pad.name().as_str());
    }

    /// RETIRE a parking sink without handing FLUSHING to whatever is pushing
    /// into it. One extra state change, and that is the whole repair.
    ///
    /// # What goes wrong without it
    ///
    /// Both parks are configured `sync=false async=false` and both are
    /// documented as non-blocking. They are not, for the reason
    /// [`Inner::assert_text_consumer_config`] already writes down about the
    /// consumer tail: **basesink prerolls regardless of `async`**. A sink
    /// sitting at PAUSED takes ONE buffer through the preroll path and blocks
    /// every buffer behind it in `gst_base_sink_wait_preroll`; `drop=true` and
    /// `max-buffers` are appsink properties consulted after the render call
    /// that never returns.
    ///
    /// A park created during a bring-up (which is every park that matters,
    /// because bring-up is the window the park exists to cover) therefore ends
    /// up holding the multiqueue slot's loop thread, with decodebin3's single
    /// queue filling up behind it. That much is harmless in itself: the join
    /// links the branch and the queue drains into it.
    ///
    /// What is NOT harmless is how the park was being removed.
    /// `set_state(NULL)` goes through PAUSED -> READY, which calls
    /// `gst_base_sink_set_flushing`, which releases the parked push with
    /// **`GST_FLOW_FLUSHING`**, and `gst_single_queue_push_one` writes
    /// that into `sq->srcresult` (gstmultiqueue.c:2498). The slot is now
    /// latched with the item's whole opening queued behind it, and
    /// [`Inner::heal_latched_text_slots`] does its job microseconds later:
    /// re-activating the slot's src pad runs `gst_single_queue_flush (mq,
    /// sq, FALSE, full=TRUE)` (`:3023`), whose
    /// `gst_single_queue_flush_queue` pops every queued item and calls
    /// `sitem->destroy (sitem)` on it (`:3513-3538`), with the sticky rescue at
    /// `:3530` skipped because `full` is TRUE. There is no non-destructive
    /// un-latch in multiqueue (the FLUSH_STOP candidate reaches the same
    /// `gst_single_queue_flush (FALSE)` (`:2789`) and the sink-pad deactivation
    /// calls `gst_data_queue_flush` outright at `:2698`) so once the latch
    /// exists the data is owed to nobody.
    ///
    /// That is the reported "6.5 seconds" in full, on a whole-period DASH
    /// WebVTT: park at 28.377, join at 28.638, the heal 0.15 ms later,
    /// no replay line (the park's one prerolled buffer was the file's
    /// zero-length twin, which `item_from_sample` refuses), no `Discarding
    /// data` upstream (the latch lived 147 µs and nothing pushed into it in
    /// that window), and the first cue on screen is the THIRD one, 6.450,
    /// because everything covering 0 to 5.200 was in the single queue the
    /// repair emptied. The join's own precondition check
    /// (`JOINS_INTO_AN_INACTIVE_BRANCH`) says the branch was ACTIVE, which is
    /// what rules the join-window latch out and points here.
    ///
    /// # The fix
    ///
    /// PAUSED -> PLAYING broadcasts the same preroll condition PAUSED -> READY
    /// does, but WITHOUT setting `flushing`, so `gst_base_sink_wait_preroll`
    /// returns `GST_FLOW_OK`. One extra state change before the NULL and the
    /// parked push completes normally: no FLUSHING, no latch, no heal, and the
    /// queued opening stays where it is until the join drains it into the
    /// branch.
    ///
    /// Called AFTER the unlink, and at a JOIN after the new branch is linked as
    /// well (see [`Inner::unpark_stream_for_join`]). That ordering is what
    /// decides where the released push LANDS.
    ///
    /// # What was tried instead, and measured worse
    ///
    /// Making the park itself wait-free (bring it up at PLAYING, so it never
    /// prerolls and the queue never backs up) fixes the same defect and was the
    /// first version of this. It also changes what the park EATS: every cue the
    /// multiqueue would have delivered to the branch after the join goes into
    /// the park's 64-deep, 2-second ring instead, and only what survives that
    /// is replayed. `external_subtitle_lifecycle`, serial, 20 runs per arm:
    /// **16 pass / 4 fail** with the wait-free park against **19 pass / 1
    /// fail** with this one (the failures spread across three tests of the
    /// attach/seek family, whose subject is precisely cues in flight). This
    /// version does not touch what the park consumes.
    ///
    /// Lever: `FCAST_NO_WAITFREE_UNPARK` restores the bare `set_state(NULL)`,
    /// which is the A/B partner for `multiqueue_slot_unlatch`'s park tests.
    pub(crate) fn retire_parking_sink(sink: &gst::Element) {
        if std::env::var_os("FCAST_NO_WAITFREE_UNPARK").is_none() {
            let _ = sink.set_state(gst::State::Playing);
            // THE BARRIER, and without it the fix is a coin toss.
            //
            // PAUSED -> PLAYING only BROADCASTS the preroll condition; the
            // parked thread still has to be scheduled and re-take the preroll
            // lock, and `gst_base_sink_wait_preroll` re-reads `sink->flushing`
            // when it gets there. Racing straight on to the NULL below sets
            // that flag first often enough to matter, measured directly on
            // `multiqueue_slot_unlatch`'s park rig, which read the slot LATCHED
            // on the run where the NULL won.
            //
            // Taking the sink pad's stream lock and dropping it again is the
            // wait for that thread and nothing else: the pad is already
            // unlinked (see [`Inner::unpark_stream_for_join`]) so no new chain
            // call can start, and the one in flight is a `sync=false` render
            // at PLAYING. Bounded by a buffer, not by a clock.
            if let Some(pad) = sink.static_pad("sink") {
                drop(pad.stream_lock());
            }
        }
        let _ = sink.set_state(gst::State::Null);
    }

    /// Create and wire a per-stream parking sink for a text stream that may
    /// not join its renderer yet (see `RoutedStream::park_pad` /
    /// `RoutedStream::park_sink`).
    pub(crate) fn park_stream(&self, source: &gst::Pad) -> Result<(gst::Element, gst::Pad)> {
        let sink = gst::ElementFactory::make("fakesink")
            .property("sync", false)
            .property("async", false)
            .property("enable-last-sample", false)
            .build()
            .context("creating a text parking sink")?;
        // Keep it out of everything GstBin routes through SINK-flagged
        // children (the `fpb-token-sink` treatment, for the same class of
        // reason). An unsynced parking sink consumes at multiqueue speed, so
        // it races toward the item's end: in the bin's POSITION fold (MAX over
        // the flagged children) it can dominate the answer, and a `-1`
        // DURATION from it poisons the duration fold entirely.
        sink.unset_element_flags(gst::ElementFlags::SINK);
        self.pipeline
            .add(&sink)
            .context("adding the text parking sink")?;
        sink.sync_state_with_parent()
            .context("syncing the text parking sink")?;
        let pad = sink.static_pad("sink").expect("fakesink has a sink pad");
        // `source` is the decodebin3 text pad itself (text bypasses ssync).
        source
            .link(&pad)
            .context("linking text into its parking sink")?;
        Ok((sink, pad))
    }

    /// Undo `park_stream`: unlink and remove the stream's parking sink.
    pub(crate) fn unpark_stream(&self, routed: &mut RoutedStream) {
        if let Some(pad) = routed.park_pad.take() {
            // Text bypasses ssync, so its source is the decodebin3 pad.
            let _ = routed.db3_src_pad.unlink(&pad);
        }
        if let Some(sink) = routed.park_sink.take() {
            self.drop_parking_sink(&sink);
        }
    }

    /// [`Inner::unpark_stream`] for a JOIN, which is the one caller with
    /// somewhere better to put the slot's next push: the park's sink is
    /// unlinked and handed back UNRETIRED, so the caller can link the branch
    /// first and only then release the push parked inside the sink's preroll.
    ///
    /// The order is the whole point (see [`Inner::retire_parking_sink`]).
    /// The parked push is released by the retirement, not by the unlink, so
    /// with the branch already linked the slot's loop thread comes out of the
    /// park and pushes straight into it. Retiring first leaves a window, short
    /// and real, in which that push lands on an unlinked pad and multiqueue
    /// answers NOT_LINKED, which costs a cue and, on a sparse text stream,
    /// costs it at the worst possible moment.
    ///
    /// The caller MUST pass the sink to [`Inner::drop_parking_sink`] on every
    /// path, including a link that failed. Nothing else will: it is out of the
    /// routing entry and only the caller's stack holds it.
    #[must_use = "an unretired parking sink must reach Inner::drop_parking_sink"]
    pub(crate) fn unpark_stream_for_join(&self, routed: &mut RoutedStream) -> Option<gst::Element> {
        if let Some(pad) = routed.park_pad.take() {
            let _ = routed.db3_src_pad.unlink(&pad);
        }
        routed.park_sink.take()
    }

    /// Retire a parking sink and take it out of the pipeline.
    pub(crate) fn drop_parking_sink(&self, sink: &gst::Element) {
        Inner::retire_parking_sink(sink);
        let _ = self.pipeline.remove(sink);
    }

    /// Stop and remove one input: NULL the element (its streaming threads
    /// stop pushing), unlink, release the decodebin3 request pads (decodebin3
    /// updates its collection), drop from the pipeline.
    /// Remove an input, or postpone it when the pipeline is at rest in PAUSED.
    ///
    /// ONLY for a user-initiated detach. `remove_input` has to flush the
    /// input's decodebin3 sink pads before it can NULL the element, or the
    /// NULL deadlocks on the input's own parked pushes. That flush travels
    /// down into decodebin3 and ends in `gst_multi_queue_sink_event` calling
    /// `gst_pad_pause_task` on the slot's src task. At a pipeline resting in
    /// PAUSED that task is stuck inside the branch's tail behind sinks parked
    /// in `gst_base_sink_wait_preroll`, so the pause never returns and the
    /// caller is wedged. On the worker it took every job queued behind it.
    /// Captured with gdb, `remove_input -> send_event -> ... ->
    /// gst_multi_queue_sink_event -> gst_pad_pause_task`.
    ///
    /// Unlinking the branch first is not enough, and that is worth recording:
    /// the flush blocks one level ABOVE the pad this crate can unlink, inside
    /// decodebin3, and unlinking downstream does not retract a push already in
    /// flight through the multiqueue.
    ///
    /// Teardown, stop, shutdown and the load reset must NOT come through here.
    /// They cannot postpone anything, and their own state change to READY or
    /// NULL releases the sinks from `wait_preroll`, which is what lets the
    /// flush complete there. That holds only because those paths make the
    /// state change BEFORE they block; see [`FcastPlaybin::teardown`], where it
    /// used to come last and wedged the worker for exactly this reason.
    pub(crate) fn remove_input_or_defer(inner: &Arc<Inner>, input: Input) {
        // A text branch of THIS input live in the graph is one way to leave
        // the slot's multiqueue task stuck inside the branch's tail, which is
        // what the flush then cannot pause. It is not the only way: see the
        // postponed-disposal case below.
        //
        // Deferring on the pipeline state alone was too broad: a pipeline
        // mid-LOAD also rests at PAUSED, and postponing there left the input
        // in the graph across the load and hung
        // `attach_then_detach_mid_load_leaves_the_load_intact`.
        let has_live_text = {
            let sids = input.stream_ids();
            let routing = inner.routing.lock();
            routing.routed.iter().any(|routed| {
                routed.kind == StreamKind::Text
                    && routed.downstream.is_some()
                    && routed
                        .db3_src_pad
                        .stream_id()
                        .is_some_and(|sid| sids.contains(&sid.to_string()))
            })
        };
        // A branch of THIS input is not the only thing that can hold the
        // block. A POSTPONED DISPOSAL is a text queue whose loop task was
        // already inside the branch's tail when it was severed, and it
        // keeps holding that queue's stream lock until the disposal runs (the
        // case `Inner::dispose_text_branch_on` describes). decodebin3 shares
        // one multiqueue across every input, so this input's flush still ends
        // in a `gst_pad_pause_task` that waits behind that stuck queue.
        //
        // Captured with gdb on `fuzz_buffering` seed 1600016, deterministic 3
        // of 3: the worker parked in `remove_input`'s decodebin3-sink flush
        // with `branches=0` for the input it was removing, one second after a
        // "postponing a text branch disposal" at a pipeline resting in PAUSED.
        // The ownership half of the guard was doing its job and the block was
        // simply somewhere else.
        //
        // The drain is ordered to match: `Inner::run_deferred_text_work` runs
        // the disposals before the input removals, so by the time this removal
        // is retried the queue that blocked it is gone.
        //
        // `FCAST_NO_DISPOSAL_AWARE_DETACH_DEFERRAL` restores the old predicate.
        let disposal_pending = !inner.deferred_text_disposal.lock().is_empty()
            && std::env::var_os("FCAST_NO_DISPOSAL_AWARE_DETACH_DEFERRAL").is_none();
        if (has_live_text || disposal_pending)
            && Inner::resting_paused(&inner.pipeline)
            && std::env::var_os("FCAST_NO_TEXT_WORK_DEFERRAL").is_none()
        {
            debug!(
                generation = input.generation,
                "postponing an input removal: the pipeline is at rest in PAUSED"
            );
            // A new postponed item invalidates the last drain's no-op
            // verdict (see `Inner::drain_poke_parked`).
            inner.drain_poke_parked.store(false, Ordering::SeqCst);
            inner.deferred_input_removal.lock().push(input);
        } else {
            Inner::remove_input(inner, input);
        }
    }

    pub(crate) fn remove_input(inner: &Arc<Inner>, input: Input) {
        // Read before the fields below are moved out of `input`.
        let sids = input.stream_ids();
        // EPOCH RETIREMENT for the reconcile pass's in-flight bit. Every
        // removal path funnels here - the gapless reap of drained inputs, a
        // user detach, a failed external, a cancelled prepare, the re-attach
        // retry - so this is the one place that cannot be forgotten. An entry
        // whose input is gone is dead weight that never discharges: it grows
        // without bound across a long session and, if the id is ever reused at
        // the same epoch, silences the pass for a live external. See
        // [`Inner::replay_inflight`].
        if let Some(external) = input.external.as_ref() {
            inner
                .replay_inflight
                .lock()
                .remove(&(external.id, external.epoch));
        }
        if let Some(sig) = input.pad_added_sig {
            input.element.disconnect(sig);
        }
        // DROP everything this input still pushes, BEFORE the flush and the
        // unlink below. Its own chain (a queue inside its urisourcebin, upstream
        // of the text queue the detach probe guards) keeps pushing into
        // decodebin3 while this runs, and the first push after the unlink
        // returns GST_FLOW_NOT_LINKED, on which that queue's loop posts
        // "Internal data stream error". The causal error classification treats
        // the source's own task death as a transport race and recovers, but it
        // cannot eat an error a QUEUE posts, so it reached the caller as
        // `PlaybinEvent::Error`: a user-visible Media error for a routine
        // subtitle detach. Pinned on `fuzz_scenarios` seed 500002 iteration 1
        // (detach of a SELECTED external right after a seek).
        //
        // A push probe returns GST_FLOW_OK before the peer is ever looked up
        // (gstpad.c runs them first), so from here the input pushes into the
        // void harmlessly. Nothing is lost: this input is leaving and goes to
        // NULL a few lines down. Never removed, the pads die with the element.
        // Buffers only, so the flush pair and EOS below still travel.
        // Lever: `FCAST_NO_INPUT_DROP_ON_REMOVE`.
        let guard_pads: Vec<gst::Pad> = input.taps.iter().map(|tap| tap.pad.clone()).collect();
        if std::env::var_os("FCAST_NO_INPUT_DROP_ON_REMOVE").is_none() {
            debug!(
                pads = ?guard_pads.iter().map(|pad| pad.name().to_string()).collect::<Vec<_>>(),
                "dropping the leaving input's data before its decodebin3 chain is taken apart"
            );
            for pad in &guard_pads {
                pad.add_probe(
                    gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                    |_pad, _info| gst::PadProbeReturn::Drop,
                );
            }
        }
        for mut tap in input.taps {
            if let Some(probe) = tap.probe.take() {
                tap.pad.remove_probe(probe);
            }
            if let Some(probe) = tap.event_probe.take() {
                tap.pad.remove_probe(probe);
            }
        }
        // A cancelled prepare's block probes (a performed swap clears the
        // list itself). Any thread parked inside one was already woken and
        // flushed by the swap gate abort that precedes this removal.
        for (pad, probe) in input.block_probes {
            pad.remove_probe(probe);
        }
        // A still-locked prepared input unlocks on its way out (harmless
        // for ordinary inputs).
        input.element.set_locked_state(false);
        // This input's OWN text branches come out of the graph before the
        // flush below. A `FLUSH_START` on a decodebin3 sink pad does not stop
        // at the slot: gstpad forwards it through parsebin and multiqueue, out
        // of decodebin3's src pad and into the text queue, where
        // `gst_queue_handle_sink_event` pauses a task that may be stuck inside
        // the branch's tail. That wedged the WORKER, taking every job queued
        // behind it (the next load, the stop, the shutdown barrier). Unlinking
        // first leaves the flush nowhere to travel, and `gst_pad_unlink` takes
        // no stream lock so it is safe at any state. The branches belong to an
        // input that is leaving, so they had to go anyway.
        let text_parts: Vec<(
            gst::Pad,
            gst::Pad,
            Option<gst::Element>,
            Option<gst::Element>,
        )> = {
            let mut routing = inner.routing.lock();
            routing
                .routed
                .iter_mut()
                .filter(|routed| routed.kind == StreamKind::Text)
                .filter(|routed| {
                    routed
                        .db3_src_pad
                        .stream_id()
                        .is_some_and(|sid| sids.contains(&sid.to_string()))
                })
                .filter_map(|routed| {
                    let downstream = routed.downstream.take()?;
                    Some((
                        routed.db3_src_pad.clone(),
                        downstream,
                        routed.tqueue.take(),
                        routed.appsink.take(),
                    ))
                })
                .collect()
        };
        debug!(
            branches = text_parts.len(),
            ?sids,
            "detaching the leaving input's text branches before the decodebin3 flush"
        );
        for (db3_src_pad, downstream, tqueue, appsink) in text_parts {
            Inner::detach_text_parts(inner, &db3_src_pad, &downstream, tqueue, appsink, false);
        }
        // The upstream unlink moves ABOVE the pair, and the pair is sent
        // only where a push is actually parked.
        //
        // WHAT THE PAIR IS FOR, unchanged: a mid-push input's streaming thread
        // parked inside its decodebin3 slot holds its own pad locks, and the
        // NULL below deadlocks on them, which wedged the worker. Only a flush
        // releases that push, and waiting would not.
        //
        // WHAT IT COSTS when nothing is parked: a `FLUSH_START` on a
        // decodebin3 sink pad does not stop at the slot. gstpad forwards it
        // through parsebin and multiqueue, out of decodebin3's src pads and
        // into every chain downstream - including both SINKS, which
        // `gst_element_lost_state` then takes out of PLAYING:
        // every gapless boundary and every external-subtitle removal briefly
        // de-PLAYs the whole pipeline to wake a push that, in the common case,
        // was never parked at all.
        //
        // So: unlink first (a) so no source can refill the slot and re-park
        // between a FLUSH_STOP and the NULL - the campaign-5 shape documented
        // at `Teardown::run` - and (b) so "at most one push can be in flight
        // per pad" holds, which is what makes the probe below stable rather
        // than a TOCTOU. `gst_pad_unlink` takes no stream lock, so it is safe
        // at any state. `release_request_pad` stays BELOW the NULL.
        // # THE QUIESCENCE SKIP IS OPT-IN, BECAUSE IT WAS MEASURED WRONG
        //
        // Skipping the pair whenever no
        // push is parked, on the reasoning that the pair exists ONLY to wake
        // one. That reasoning is false, and the counter-evidence is
        // deterministic: with the skip on,
        // `external_subtitle_lifecycle::reattaching_the_same_url_after_a_detach_renders_again`
        // and `..._while_paused_renders_after_resume` both fail on "no DUPE
        // cue reached the renderer", 2 of 2, and pass 2 of 2 with the pair
        // restored. The debug log says exactly what happens: the branch is
        // detached, the pair is skipped, the NEW pad `text_1` appears - and
        // decodebin3's OLD `text_0` is STILL THERE, carrying the same
        // stream-id, so the policy links the dead one and no cue can ever
        // arrive.
        //
        // The pair therefore has a SECOND job nobody had written down: it is
        // part of what makes decodebin3 retire the leaving input's src pads
        // promptly (`release_request_pad` -> `gst_decodebin_input_reset` sets
        // parsebin to NULL, and a parsebin whose push is parked deeper in the
        // multiqueue cannot get there until something flushes). And the
        // trylock is the wrong question for that: it reads THIS pad's stream
        // lock, which says nothing about a thread parked further inside
        // decodebin3. So the de-PLAY survives, and removing it needs
        // a mechanism that retires the slot, not a better quiescence probe.
        //
        // The gate stays reachable so the next attempt starts from a running
        // instrument rather than from this comment.
        // Levers: `FCAST_NO_REMOVE_INPUT_FLUSH_SKIP` restores v1 wholesale
        // (old order, unconditional pair); `FCAST_REMOVE_INPUT_FLUSH_SKIP`
        // turns the measured-wrong skip back on.
        let v1_order = std::env::var_os("FCAST_NO_REMOVE_INPUT_FLUSH_SKIP").is_some();
        let try_skip = !v1_order && std::env::var_os("FCAST_REMOVE_INPUT_FLUSH_SKIP").is_some();
        // The upstream unlink moves ABOVE the pair. This half IS shipped: it
        // closes the campaign-5 refill race at this site (a FLUSH_STOP re-arms
        // the pad and an as-fast-as-possible source refills the slot and
        // re-parks before the NULL below - the shape `Teardown::run`
        // documents), and it guarantees "at most one push in flight per pad",
        // which is what any future skip predicate will need. Measured neutral:
        // with the pair still sent, external_subtitle_lifecycle is 19/19.
        // `gst_pad_unlink` takes no stream lock, so it is safe at any state.
        // `release_request_pad` stays BELOW the NULL.
        if !v1_order {
            for db3_sink in &input.db3_sink_pads {
                if let Some(peer) = db3_sink.peer() {
                    let _ = peer.unlink(db3_sink);
                }
            }
        }
        for db3_sink in &input.db3_sink_pads {
            if try_skip && Self::pad_is_quiescent(db3_sink) {
                REMOVE_INPUT_PAIRS_SKIPPED.fetch_add(1, Ordering::SeqCst);
                debug!(
                    pad = %db3_sink.name(),
                    "no push is parked on the leaving input's pad; skipping the flush pair \
                     (FCAST_REMOVE_INPUT_FLUSH_SKIP, measured to break the same-URL re-attach)"
                );
                continue;
            }
            // A mid-push input's streaming thread is parked inside its
            // decodebin3 slot HOLDING ITS OWN PAD LOCKS, and the NULL below
            // deadlocks on them, which wedged the worker. The
            // BARE pair, deliberately: a segment replay here regressed the
            // external-subtitle reattach path, and this window closes at the
            // NULL below anyway (see `Inner::flush_db3_sink_pads`).
            REMOVE_INPUT_PAIRS_SENT.fetch_add(1, Ordering::SeqCst);
            Self::send_flush_pair(db3_sink, FlushReason::RemoveInput);
        }
        // Losing the state change here is fine, the element is leaving.
        let _ = input.element.set_state(gst::State::Null);
        for db3_sink in &input.db3_sink_pads {
            if let Some(peer) = db3_sink.peer() {
                let _ = peer.unlink(db3_sink);
            }
            // Release against the pad's OWN decodebin3: after a core swap
            // this input's pads belong to the previous instance.
            if let Some(db3) = db3_sink.parent_element() {
                db3.release_request_pad(db3_sink);
            }
        }
        let _ = inner.pipeline.remove(&input.element);
        // Cluster (c) of the four surgery sites. The pair above does NOT stop
        // at the slot - gstpad forwards it out of decodebin3's src pads and
        // downstream - so the pads worth reading are the ones this removal
        // LEAVES BEHIND: everything still routed that did not belong to the
        // leaving input. Its own pads are gone from the graph and a FLUSHING
        // on them harms nobody.
        let remaining: Vec<gst::Pad> = {
            let routing = inner.routing.lock();
            routing
                .routed
                .iter()
                .filter(|routed| {
                    !routed
                        .db3_src_pad
                        .stream_id()
                        .is_some_and(|sid| sids.contains(&sid.to_string()))
                })
                .flat_map(|routed| {
                    std::iter::once(routed.db3_src_pad.clone()).chain(routed.downstream.clone())
                })
                .collect()
        };
        Self::flow_census(FlowStage::RemoveInput, &remaining);
    }

    pub(crate) fn remove_all_inputs(inner: &Arc<Inner>) {
        let inputs = std::mem::take(&mut inner.routing.lock().inputs);
        for input in inputs {
            Inner::remove_input(inner, input);
        }
    }

    /// Re-attempt every deferred pad through the full routing path. Runs on
    /// every [`RouteGate`] release. The guards re-reject stale (superseded
    /// core) or torn-down (not accepting) pads, and a pad still blocked by
    /// another gate holder is re-deferred (that holder's release drains it).
    pub(crate) fn drain_deferred_pads(inner: &Arc<Inner>) {
        let pending = std::mem::take(&mut *inner.deferred_pads.lock());
        for pad in pending {
            if let Err(err) = Inner::route_db3_pad(inner, &pad) {
                warn!(?err, pad = %pad.name(), "failed to route deferred pad");
            }
        }
    }

    /// Route a decodebin3 output pad through streamsynchronizer into its
    /// chain. Text pads obey the link policy (steady PLAYING only).
    pub(crate) fn route_db3_pad(inner: &Arc<Inner>, pad: &gst::Pad) -> Result<()> {
        // pad-added also fires for decodebin3's request SINK pads (our own
        // input links). Only source pads are output streams to route.
        if pad.direction() != gst::PadDirection::Src {
            return Ok(());
        }
        // Refuse pads while a downward transition holds the gate (see
        // `Inner::route_gate`). Hold it for the whole route so a teardown
        // cannot start mid-route either. The deferred-pads lock is held
        // ACROSS the try-lock so a failed attempt's push cannot slip in
        // after the concurrent holder's release already drained (which
        // would orphan the pad): with the lock held, either the drain sees
        // the push, or the push happens after the drain and this try-lock
        // succeeds.
        let gate = {
            let mut deferred = inner.deferred_pads.lock();
            match Inner::try_gate(inner) {
                Some(gate) => Some(gate),
                None => {
                    // The gate is held by a concurrent downward transition.
                    // A pad from the CURRENT core is the ACTIVE load losing
                    // a stream: DEFER it (the holder's release re-attempts
                    // it) rather than dropping it for good (the load-stall
                    // race). A pad from another core is teardown debris.
                    let from_current_core = inner
                        .core
                        .lock()
                        .as_ref()
                        .is_some_and(|c| pad.parent_element().as_ref() == Some(&c.db3));
                    if from_current_core {
                        deferred.push(pad.clone());
                        debug!(pad = %pad.name(), "deferring active-core pad past a teardown");
                    } else {
                        debug!(pad = %pad.name(), "ignoring pad exposed during a teardown");
                    }
                    None
                }
            }
        };
        let Some(_gate) = gate else {
            return Ok(());
        };
        // A pad from a superseded core (the previous load's decodebin3 can
        // still process queued selections while dying) must not be routed:
        // it would occupy the chain entry and wedge the next preroll.
        let (ssync, ssync_owner) = {
            let core = inner.core.lock();
            let Some(core) = core.as_ref() else {
                return Ok(());
            };
            if pad.parent_element().as_ref() != Some(&core.db3) {
                debug!(pad = %pad.name(), "ignoring pad from a superseded core");
                return Ok(());
            }
            (core.ssync.clone(), core.db3.clone())
        };
        // Pads appearing while the pipeline is at/heading to READY are
        // stragglers from a superseded load's teardown. Legitimate pads only
        // appear during a preroll (pending at least PAUSED) or in a settled
        // pipeline at PAUSED or above.
        let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
        if !decisions::pad_accepting(current, pending) {
            warn!(pad = %pad.name(), ?current, ?pending,
                   "ignoring stray pad from a superseded load");
            return Ok(());
        }
        let kind = Inner::stream_kind_of(pad)
            .ok_or_else(|| anyhow!("cannot determine stream kind of {}", pad.name()))?;
        // Runs on a STREAMING THREAD holding decodebin3's SELECTION_LOCK
        // (pad-added comes from `mq_slot_check_reconfiguration`, which took it),
        // so an unbounded blocking call here stops every SELECT_STREAMS, every
        // other slot's reconfiguration and decodebin3's own state changes, i.e.
        // the teardown that would release it. `ensure_video_chain` /
        // `ensure_audio_sink` `set_state` on the calling thread, and a
        // window-bound sink can spend seconds in Ready->Paused.
        //
        // So only the ACTIVATION moves, to `fpb-join`, with the stream held at
        // the ssync src pad meanwhile. The topology stays inline: the selection
        // dance needs it visible the moment `send_event(SELECT_STREAMS)`
        // returns, and this function also runs inline on `fpb-select` when
        // decodebin3 exposes the pad inside that send.
        //
        // Lever: `FCAST_INLINE_CHAIN_JOIN`.
        let deferred_join = std::env::var_os("FCAST_INLINE_CHAIN_JOIN").is_none();

        // Request a streamsynchronizer sink/src pair and link `pad` into it.
        // A/V only, TEXT bypasses ssync entirely (see `RoutedStream`).
        let attach_ssync = || -> Result<(gst::Pad, gst::Pad)> {
            let sink = ssync
                .request_pad_simple("sink_%u")
                .ok_or_else(|| anyhow!("streamsynchronizer gave no request pad"))?;
            // streamsynchronizer pairs sink_N with src_N.
            let src_name = sink.name().replace("sink_", "src_");
            let src = ssync
                .static_pad(&src_name)
                .ok_or_else(|| anyhow!("streamsynchronizer src pad {src_name} missing"))?;
            pad.link(&sink)
                .with_context(|| format!("linking {} into streamsynchronizer", pad.name()))?;
            Ok((sink, src))
        };

        // Set by the A/V arms when the chain's activation was deferred. The
        // job is sent at the very END of this function, once the routing entry
        // exists: `run_chain_join` refuses a join whose stream is no longer
        // routed, and the entry is pushed below.
        let mut queue_join = false;
        let mut join_hold: Option<(gst::Pad, gst::PadProbeId)> = None;

        let (ssync_sink, ssync_src, downstream, park_pad, park_sink) = match kind {
            // A video pad appearing while the crate's last dispatched
            // selection turned video OFF is decodebin3's collection-default
            // auto-select resurrecting the deselected stream (an attach
            // makes it re-select over the applied state). Rebuilding the
            // chain for it drops the pipeline into an async re-preroll that
            // the deselected stream may never feed again, and that
            // unfinished transition holds the receiver's pump gate closed
            // forever, so the engine's corrective re-assert can never run
            // (fuzz_buffering seed 300002, campaign 5). Park the pad the way
            // text parks instead. The parking sink consumes the stream
            // without any async element, the pipeline stays settled, the
            // engine notices the divergence and re-asserts on the next
            // pump, and decodebin3 drops the pad again. A genuine re-enable
            // flips the dispatched intent before its selection reaches
            // decodebin3, so it takes the rebuild arm below. The lever
            // restores the unconditional rebuild and gates this whole
            // change (the mirror it reads is inert without this read).
            StreamKind::Video
                if inner.video_deselected.load(Ordering::SeqCst)
                    && std::env::var_os("FCAST_ROUTE_DESELECTED_VIDEO").is_none() =>
            {
                info!(
                    pad = %pad.name(),
                    "parking a resurrected video pad the dispatched selection has off"
                );
                let (sink, park) = inner.park_stream(pad)?;
                (None, None, None, Some(park), Some(sink))
            }
            // A video pad re-routed for a group whose end has already
            // entered streamsynchronizer cannot preroll a fresh chain. The
            // stream's own end was consumed before the re-route (its EOS
            // either passed with the old chain or died with the dropped
            // slot), so the new branch will never see a buffer or an EOS.
            // The sink parks in Ready->Paused forever, the async transition
            // never completes, the receiver's pump gate never opens, and a
            // sibling EOS arriving after the splice parks its streaming
            // thread inside gst_stream_synchronizer_wait against the fresh
            // pad (fuzz_buffering seed 1600058, gdb captured: multiqueue
            // src in gststreamsynchronizer.c:480 behind the resurrected
            // pad, the video source task idle at EOS, see the findings
            // entry on the drained-resurrect park). This also enforces the
            // invariant the sibling-pass gate below states, that once one
            // EOS of a group entered ssync the group MUST complete, which a
            // fresh EOS-less pad makes impossible.
            //
            // Parking trades the permanent wedge for a silent video slot on
            // the drained remainder of the item. A flushing seek restarts
            // the streams and clears the mirror (see `Job::Seek`), so a
            // re-enable after a seek still builds the chain normally.
            // Lever: `FCAST_NO_DRAINED_RESURRECT_PARK` restores the
            // unconditional rebuild (the seek-side clear is gated on the
            // same lever).
            StreamKind::Video
                if std::env::var_os("FCAST_NO_DRAINED_RESURRECT_PARK").is_none() && {
                    // Two signals, either one marks the stream drained.
                    // The group mirror covers a stream whose end reached
                    // the OUTPUT before the re-route. The input-side set
                    // covers a stream whose end was consumed invisibly
                    // (its slot was dropped while deselected, so no output
                    // probe ever saw it, fuzz_buffering seed 1600008).
                    let sticky = pad.sticky_event::<gst::event::StreamStart>(0);
                    let group = sticky.as_ref().and_then(|event| event.group_id());
                    let group_passing = group.is_some() && group == *inner.passing_eos_group.lock();
                    let sid = sticky.map(|event| event.stream_id().to_string());
                    let input_drained = sid
                        .as_ref()
                        .is_some_and(|sid| inner.input_eos_sids.lock().contains(sid));
                    let rerouted = inner.video_unrouted_once.load(Ordering::SeqCst);
                    debug!(
                        pad = %pad.name(),
                        ?sid,
                        group_passing,
                        input_drained,
                        rerouted,
                        "drained-resurrect check"
                    );
                    (group_passing || input_drained) && rerouted
                } =>
            {
                info!(
                    pad = %pad.name(),
                    "parking a re-routed video pad of a drained stream"
                );
                let (sink, park) = inner.park_stream(pad)?;
                (None, None, None, Some(park), Some(sink))
            }
            StreamKind::Video => {
                let (ss_sink, ss_src) = attach_ssync()?;
                if deferred_join {
                    // Membership only: the chain has to be in the pipeline for
                    // the link below to be legal, but its ACTIVATION is what
                    // blocks, and that goes to `fpb-join`.
                    inner.attach_video_chain()?;
                } else {
                    // Put the video chain into the pipeline (also the recovery
                    // from a mid-item deselect's parked chain).
                    inner.ensure_video_chain()?;
                }
                let entry = inner
                    .video_sink
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("the video sink has no sink pad"))?;
                ss_src.link(&entry).context("linking video chain")?;
                if deferred_join {
                    // AFTER the link, which cannot race: the only thread that
                    // ever pushes on `ss_src` is the one inside this call.
                    join_hold = Inner::hold_chain_entry(&ss_src);
                    queue_join = true;
                } else {
                    inner.finish_preroll_token();
                }
                (Some(ss_sink), Some(ss_src), Some(entry), None, None)
            }
            StreamKind::Audio => {
                let (ss_sink, ss_src) = attach_ssync()?;
                if !deferred_join {
                    // Build this load's fresh audio sink (see
                    // `Inner::audio`). The prefix is already active.
                    inner.ensure_audio_sink()?;
                }
                let entry = inner
                    .audio_entry
                    .static_pad("sink")
                    .ok_or_else(|| anyhow!("fpb-aqueue sink missing"))?;
                ss_src.link(&entry).context("linking audio chain")?;
                if deferred_join {
                    join_hold = Inner::hold_chain_entry(&ss_src);
                    queue_join = true;
                } else {
                    inner.finish_preroll_token();
                }
                (Some(ss_sink), Some(ss_src), Some(entry), None, None)
            }
            StreamKind::Text => {
                // BYPASS streamsynchronizer (see `RoutedStream`): link the
                // decodebin3 text pad straight to its parking sink. Text
                // joins its consumer tail only via `poll_text_policy`, and
                // until then it drains into the parking sink (it must be
                // consumed, see `RoutedStream::park_pad`).
                //
                // The park KEEPS what it drains, and the join puts it back:
                // the window between here and `text_may_link` is the whole of
                // bring-up, and on a whole-period text Representation it is
                // the item's opening seconds (see
                // `Inner::parked_text_cues`).
                let (sink, park) = inner.park_text_stream(pad)?;
                (None, None, None, Some(park), Some(sink))
            }
        };

        // The post-streamsynchronizer half of the gapless EOS hold. An EOS
        // that entered streamsynchronizer (it slipped the output gate below
        // before a pre-arm armed it, see `Inner::passing_eos_group`) parks
        // its pushing thread there until the whole group is EOS, and
        // streamsynchronizer then re-emits EOS on every src pad. Those must
        // still not reach the sinks while a swap is in flight: drop them
        // here under the same conditions as the output gate. At a true end
        // of playback nothing is pending, the pad's group matches the
        // active one, and the EOS flows to the sinks normally.
        if let Some(ss_src) = &ssync_src {
            let weak = Arc::downgrade(inner);
            ss_src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |pad, info| {
                let Some(gst::PadProbeData::Event(event)) = &info.data else {
                    return gst::PadProbeReturn::Ok;
                };
                if !matches!(event.view(), gst::EventView::Eos(_)) {
                    return gst::PadProbeReturn::Ok;
                }
                let Some(inner) = weak.upgrade() else {
                    return gst::PadProbeReturn::Ok;
                };
                let pad_group = pad
                    .sticky_event::<gst::event::StreamStart>(0)
                    .and_then(|event| event.group_id());
                let (should_drop, pending, behind) = inner.gapless_eos_check_and_mark(pad_group);
                if should_drop {
                    debug!(
                        pad = %pad.name(),
                        pending,
                        behind,
                        "gapless: dropping a drained EOS after streamsynchronizer"
                    );
                    return gst::PadProbeReturn::Drop;
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Gapless EOS hold (the uridecodebin3 db_src_probe): while a
        // prepared next item is PENDING, any EOS coming out of decodebin3
        // is the drained current item's and must not reach the sinks (it
        // would end the pipeline between items). Activation clears the
        // pending state, so the new item's own end flows normally; a
        // straggler old-slot EOS emerging just after activation is
        // absorbed by streamsynchronizer (the other streams are active
        // again by definition of the activation trigger).
        pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, {
            let weak = Arc::downgrade(inner);
            move |pad, info| {
                let Some(gst::PadProbeData::Event(event)) = &info.data else {
                    return gst::PadProbeReturn::Ok;
                };
                let group = match event.view() {
                    gst::EventView::Eos(_) => None,
                    gst::EventView::StreamStart(stream_start) => Some(stream_start.group_id()),
                    _ => return gst::PadProbeReturn::Ok,
                };
                let Some(inner) = weak.upgrade() else {
                    return gst::PadProbeReturn::Ok;
                };
                match group {
                    None => {
                        // Drop the EOS while a swap is pending or while
                        // THIS pad still carries a previous group (see
                        // `Inner::gapless_eos_check_and_mark`). The pad's
                        // sticky stream-start at EOS time is the ending
                        // stream's, an authoritative fallback when the
                        // recorded group is missing.
                        let pad_group = {
                            let routing = inner.routing.lock();
                            routing
                                .routed
                                .iter()
                                .find(|r| &r.db3_src_pad == pad)
                                .and_then(|r| r.group)
                        }
                        .or_else(|| {
                            pad.sticky_event::<gst::event::StreamStart>(0)
                                .and_then(|event| event.group_id())
                        });
                        let av = matches!(kind, StreamKind::Video | StreamKind::Audio);
                        // Group consistency with streamsynchronizer: once
                        // one EOS of this group passed into ssync, its
                        // siblings MUST follow or ssync never completes the
                        // group and its parked thread (the multiqueue slot
                        // task of the stream that EOSed first) never wakes.
                        // The post-ssync gate consumes them instead (see
                        // `Inner::passing_eos_group`).
                        if av && pad_group.is_some() && pad_group == *inner.passing_eos_group.lock()
                        {
                            debug!(
                                pad = %pad.name(),
                                "gapless: passing a sibling EOS through to complete the group"
                            );
                            return gst::PadProbeReturn::Ok;
                        }
                        let (should_drop, pending, behind) =
                            inner.gapless_eos_check_and_mark(pad_group);
                        if should_drop {
                            debug!(
                                pad = %pad.name(),
                                pending,
                                behind,
                                "gapless: dropping the drained item's EOS"
                            );
                            return gst::PadProbeReturn::Drop;
                        }
                        // This EOS enters streamsynchronizer. Commit the
                        // whole group to passing so a pre-arm landing
                        // between now and the siblings' EOS cannot strand
                        // the group half-ended in ssync.
                        if av && let Some(group) = pad_group {
                            *inner.passing_eos_group.lock() = Some(group);
                        }
                    }
                    // STREAM_START: record the pad's group; a group change
                    // activates a pending prepared item.
                    Some(group) => {
                        if let Some(group) = group {
                            let mut routing = inner.routing.lock();
                            if let Some(routed) =
                                routing.routed.iter_mut().find(|r| &r.db3_src_pad == pad)
                            {
                                routed.group = Some(group);
                            }
                        }
                        inner.note_output_stream_start(group);
                    }
                }
                gst::PadProbeReturn::Ok
            }
        });

        debug!(pad = %pad.name(), ?kind, linked = downstream.is_some(), "routed decodebin3 pad");
        // Seed the pad's group from its sticky stream-start: the live event
        // may already have passed (it flows the moment the link above
        // completes, possibly before the probe existed). The sticky is
        // authoritative either way.
        let group = pad
            .sticky_event::<gst::event::StreamStart>(0)
            .and_then(|event| event.group_id());
        if let Some(group) = group {
            let mut active = inner.active_group.lock();
            if active.is_none() {
                *active = Some(group);
            }
        }
        // WHICH OF TWO SAME-SID TEXT PADS decodebin3 IS ACTUALLY FEEDING, and
        // the only signal that answers it (see [`RoutedStream::superseded`]).
        // decodebin3 re-points an existing text output at a different slot
        // with no pad-added and no message, so the crate cannot be TOLD; it
        // can only watch. A buffer crossing the pad is the observation, and it
        // is exact: the abandoned pad's slot has no input left and carries
        // none, while the re-pointed one carries the stream's every cue.
        //
        // TEXT ONLY. A/V pads are never duplicated per stream this way (they
        // hold their slot through a re-enable because their chain keeps the
        // stream requested), so the probe would be pure cost on the two pads
        // that carry the most buffers in the pipeline.
        let last_buffer = Arc::new(AtomicU64::new(0));
        // WHETHER decodebin3 HAS ENDED THE SLOT THIS OUTPUT GHOSTS (see
        // [`RoutedStream::saw_eos`]). The `last_buffer` probe answers "which of
        // two pads is being fed"; it cannot answer "is this pad finished",
        // because a rival that has not carried a buffer yet stamps the same
        // zero as a pad that never will. This one asks decodebin3 directly.
        //
        // TEXT ONLY, for the reason above: A/V outputs are not duplicated per
        // stream, so a dead one has no rival to lose the seat to.
        //
        // A SEPARATE PROBE from the gapless EOS gate a few lines up on the same
        // pad, deliberately. That gate DROPS the EOS it decides belongs to a
        // drained item, and this flag is about what decodebin3 SENT rather than
        // about what reached the sinks: a dropped EOS still means the slot
        // behind this ghost is over. Folding the two would tie an observation
        // to a decision that is not about it.
        // Lever: `FCAST_NO_TEXT_OUTPUT_EOS_TAP`.
        let saw_eos = Arc::new(AtomicBool::new(false));
        if kind == StreamKind::Text {
            // THE REUSE-SHAPE ASK. decodebin3 can re-point an EXISTING text
            // output at a fresh slot with no pad-added (the walk-back, made
            // COMMON by the outputless-slot patches: a re-enable whose old
            // input has drained reuses the cleared slot and keeps the output,
            // gstdecodebin3.c:3891). Every other ask keys on an edge that does
            // not exist here (no pad appears, nothing joins) so a receiver
            // that polls on events never re-runs the link policy and the
            // re-pointed pad pushes its whole re-fetched track into a park
            // while a dead branch keeps the seat.
            //
            // The edge that DOES exist is on this pad: the fresh STREAM_START
            // clears `saw_eos` (the probe below already logs exactly that
            // transition), and the slot's buffers follow. Both ask, because
            // they answer different halves: the alive edge lets the link loop
            // seat a pad nothing holds against, and the FIRST BUFFER after it
            // is the flow reclaim's evidence against a holder. A poll run
            // between the two finds the seat occupied and no ticket to order
            // the rival by, decides nothing, and nothing would ask again.
            //
            // ASKED for, not performed, from the slot's streaming thread
            // (`Inner::link_input_pad`'s tail is the precedent) and coalesced,
            // so a re-point costs at most two jobs.
            let poke_on_alive = Arc::new(AtomicBool::new(false));
            pad.add_probe(gst::PadProbeType::BUFFER, {
                let last_buffer = Arc::clone(&last_buffer);
                let poke_on_alive = Arc::clone(&poke_on_alive);
                let weak = Arc::downgrade(inner);
                move |_pad, _info| {
                    if let Some(inner) = weak.upgrade() {
                        // Relaxed throughout: the only consumer compares two
                        // of these for ORDER under the routing lock, and the
                        // fetch_add alone gives every stamp a distinct value.
                        let ticket = inner.text_flow_ticket.fetch_add(1, Ordering::Relaxed) + 1;
                        last_buffer.store(ticket, Ordering::Relaxed);
                        // AFTER the stamp, so the poll this queues can only
                        // run against a ticket that is already readable.
                        if poke_on_alive.swap(false, Ordering::SeqCst) {
                            Inner::request_text_policy_poll(&inner);
                        }
                    }
                    gst::PadProbeReturn::Ok
                }
            });
            if std::env::var_os("FCAST_NO_TEXT_OUTPUT_EOS_TAP").is_none() {
                pad.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, {
                    let saw_eos = Arc::clone(&saw_eos);
                    let poke_on_alive = Arc::clone(&poke_on_alive);
                    let weak = Arc::downgrade(inner);
                    move |pad, info| {
                        let Some(gst::PadProbeData::Event(event)) = &info.data else {
                            return gst::PadProbeReturn::Ok;
                        };
                        let ended = match event.view() {
                            gst::EventView::Eos(_) => true,
                            gst::EventView::FlushStop(_) | gst::EventView::StreamStart(_) => false,
                            _ => return gst::PadProbeReturn::Ok,
                        };
                        // Takes NO crate lock and waits for nothing: this runs
                        // on the slot's streaming thread, which the routing
                        // lock is held across pipeline surgery against. The
                        // ask below keeps that contract: an atomic swap and a
                        // coalesced job send.
                        if saw_eos.swap(ended, Ordering::SeqCst) != ended {
                            debug!(
                                pad = %pad.name(),
                                ended,
                                "a routed text output's decodebin3 slot changed liveness"
                            );
                            if !ended {
                                poke_on_alive.store(true, Ordering::SeqCst);
                                if let Some(inner) = weak.upgrade() {
                                    Inner::request_text_policy_poll(&inner);
                                }
                            }
                        }
                        gst::PadProbeReturn::Ok
                    }
                });
            }
        }
        let mut routing = inner.routing.lock();
        routing.routed.push(RoutedStream {
            db3_src_pad: pad.clone(),
            ssync_sink,
            ssync_src,
            downstream,
            park_pad,
            park_sink,
            tqueue: None,
            appsink: None,
            group,
            evicted_dead: false,
            superseded: false,
            ever_joined: false,
            last_buffer,
            saw_eos,
            kind,
        });
        drop(routing);

        // A new text stream may be linkable right away, and a (re)arriving video
        // stream may unblock a parked one. A DEFERRED video join runs it on
        // `fpb-join` instead, after the chain is up: text must not be spliced
        // into a chain that is still parked at READY, and this call is
        // itself pipeline surgery that has no business on a streaming thread.
        //
        // ASKED for, not performed - the last of the four foreign threads that
        // used to run the link policy. The surgery it decides is the one the
        // decider owns (see `Inner::request_text_policy_poll`); what this
        // thread keeps is the report that a pad now exists.
        //
        // This is the INSTANT-TEXT-IN-PAUSED path, so the hop is where the
        // latency risk concentrates, and the argument that it costs
        // nothing is specific: the link is gated on `text_may_link` (settled,
        // at least PAUSED, nothing pending), and a settle is a preroll ENDING,
        // so whenever the poll can do anything at all the decider is not
        // parked in one. Measured against the pre-move baseline, the switch
        // confirmation did not move (switch_latency_probe, in the ledger).
        // What buys the latency is the PARK before the send, not this call
        // running here (see `FcastPlaybin::dispatch_selection`).
        //
        // Deliberately NOT folded into the settled-PLAYING drain
        // (`Job::DrainTextWork`): the link must happen at settled PAUSED, or a
        // switch performed while paused stays invisible until resume.
        let poll_here = match kind {
            StreamKind::Text => true,
            StreamKind::Video => !deferred_join,
            StreamKind::Audio => false,
        };
        if poll_here {
            if inner.inline_route_text_poll {
                Inner::poll_text_policy(inner);
            } else {
                Inner::request_text_policy_poll(inner);
            }
        }
        // Last, so the routing entry above is already visible: the join
        // re-validates against it. The joiner would block on the route gate
        // this function still holds anyway, but nothing here should depend on
        // that.
        if queue_join {
            Inner::queue_chain_join(inner, &ssync_owner, pad, kind, join_hold);
        }
        Ok(())
    }

    /// Hold a routed stream at the streamsynchronizer src pad until its chain
    /// is activated (see [`ChainJoinJob`]).
    ///
    /// A BLOCKING probe, not a DROP one: no buffer may be lost, and the pad
    /// this blocks is DOWNSTREAM of decodebin3, so the streaming thread parked
    /// here holds no decodebin3 lock at all (`mq_slot_handle_stream_start`
    /// releases SELECTION_LOCK before the push continues, gstdecodebin3.c
    /// `beach:`). It holds the multiqueue src pad's and the ssync sink pad's
    /// stream locks, and nothing the join does needs either.
    ///
    /// Three independent things release it, so the postponed work cannot
    /// outlive its drain: the join itself, any flush (`FLUSH_START` broadcasts
    /// the block cond and is never itself blocked, gstpad.c
    /// `gst_pad_push_event_unchecked`), and any deactivation of the pad (a
    /// downward transition, or `release_request_pad`, which streamsynchronizer
    /// deliberately does src-pad-first for exactly this reason).
    fn hold_chain_entry(ss_src: &gst::Pad) -> Option<(gst::Pad, gst::PadProbeId)> {
        let id = ss_src.add_probe(gst::PadProbeType::BLOCK_DOWNSTREAM, |_pad, _info| {
            gst::PadProbeReturn::Ok
        })?;
        debug!(pad = %ss_src.name(), "holding a routed stream until its chain joins");
        Some((ss_src.clone(), id))
    }

    /// Queue the blocking half of a route (see [`ChainJoinJob`]). A failed
    /// send means the crate is going away, so the hold comes off here instead:
    /// nothing would ever release it.
    fn queue_chain_join(
        inner: &Arc<Inner>,
        db3: &gst::Element,
        pad: &gst::Pad,
        kind: StreamKind,
        hold: Option<(gst::Pad, gst::PadProbeId)>,
    ) {
        let job = ChainJoinJob {
            db3: db3.clone(),
            pad: pad.clone(),
            kind,
            hold: hands::JoinHold::new(hold),
        };
        if let Err(effect) = inner.enqueue_effect(Effect::ChainJoin(job)) {
            warn!("the chain joiner is gone; joining inline");
            // Computed before the destructure so that even the unreachable
            // arm below settles what the effect owes. A held stream nobody
            // releases is a freeze, and "unreachable" is not a release.
            let owed = hands::LaneFallback::of(&effect);
            let Effect::ChainJoin(ChainJoinJob { hold, kind, .. }) = effect else {
                hands::wrong_lane(Lane::Join);
                Inner::run_lane_fallback(inner, None, owed);
                return;
            };
            let joined = match kind {
                StreamKind::Video => inner.ensure_video_chain(),
                StreamKind::Audio => inner.ensure_audio_sink(),
                StreamKind::Text => Ok(()),
            };
            if let Err(err) = joined {
                warn!(?err, ?kind, "failed to join a chain after a lost join job");
            }
            hold.release("the join ran inline after a lost join job");
        }
    }

    /// Activate a routed stream's chain and let it flow (see
    /// [`ChainJoinJob`]). Runs on `fpb-join`, where BLOCKING IS ALLOWED.
    pub(crate) fn run_chain_join(inner: &Arc<Inner>, job: ChainJoinJob) {
        let ChainJoinJob {
            db3,
            pad,
            kind,
            hold,
        } = job;
        {
            // NOT the route gate (see `Inner::join_gate`): a chain must not
            // be activated into a descending pipeline, but a route must never
            // have to wait for a join. Blocking here is what this thread is
            // for.
            let _gate = Inner::join_hold(inner);
            // Re-checked UNDER the gate, because between the route and this
            // join the core can have been swapped, the stream deselected
            // again, or the pipeline taken down. Activating a chain for any of
            // those strands a sink in the pipeline with nothing to feed it,
            // which is the async-forever wedge `remove_video_chain` exists to
            // avoid. The hold still comes off below either way.
            let current_core = inner.core.lock().as_ref().is_some_and(|c| c.db3 == db3);
            let still_routed = inner
                .routing
                .lock()
                .routed
                .iter()
                .any(|r| r.db3_src_pad == pad);
            let (_, current, pending) = inner.pipeline.state(gst::ClockTime::ZERO);
            let accepting = decisions::pad_accepting(current, pending);
            if !current_core || !still_routed || !accepting {
                debug!(
                    pad = %pad.name(), ?kind, current_core, still_routed, ?current, ?pending,
                    "dropping a stale chain join"
                );
            } else {
                let joined = match kind {
                    StreamKind::Video => inner.ensure_video_chain(),
                    StreamKind::Audio => inner.ensure_audio_sink(),
                    StreamKind::Text => Ok(()),
                };
                match joined {
                    // The load's preroll is carried by a real sink from here
                    // on, exactly as it was when this ran inline.
                    Ok(()) => inner.finish_preroll_token(),
                    Err(err) => warn!(?err, ?kind, pad = %pad.name(), "failed to join a chain"),
                }
            }
            // Under the gate, so the chain is up and flowing as one step as
            // far as any teardown is concerned.
            hold.release("the chain join finished");
            // The video chain is only now out of its parked state, so this is
            // where a re-arriving video stream can take text back (see the
            // tail of `route_db3_pad`). Asked for rather than performed: the
            // join lane is not the decider, and this one used to splice the
            // text branch from here while the routing thread could be
            // deciding the same thing.
            if kind == StreamKind::Video {
                Inner::request_text_policy_poll(inner);
            }
        }
    }

    /// A decodebin3 output pad went away (stream deselected or input
    /// removed): unlink and release its streamsynchronizer pads.
    pub(crate) fn unroute_db3_pad(inner: &Arc<Inner>, pad: &gst::Pad) {
        let mut routing = inner.routing.lock();
        let Some(idx) = routing.routed.iter().position(|r| &r.db3_src_pad == pad) else {
            return;
        };
        let routed = routing.routed.remove(idx);
        drop(routing);

        let mut routed = routed;
        if routed.kind == StreamKind::Text {
            // This callback runs on a streaming thread, where the disposal's
            // blocking flush is forbidden, so it is handed to the worker.
            // The unlink itself still happens here and does not block. The
            // lever restores the previous inline dispatch for interleaved
            // A/B measurement and gates this whole change.
            //
            // F7 WARNING on the lever: running the disposal inline puts it on
            // a STREAMING thread, and the disposal reaches
            // `upstream_owns_selection` through `detach_text_parts` - a cached
            // `Selectable` peer_query. Querying a demuxer from its own
            // streaming thread is the blocking-call hazard the phase-3 F7 row
            // names. The default arm cannot reach it (the decider owns the
            // disposal); the lever is for measurement, not for shipping.
            let defer_disposal = std::env::var_os("FCAST_INLINE_UNROUTE_DISPOSAL").is_none();
            Inner::detach_text_branch(inner, &mut routed, defer_disposal);
        } else if let (Some(ssync_src), Some(downstream)) = (&routed.ssync_src, &routed.downstream)
        {
            let _ = ssync_src.unlink(downstream);
        }
        inner.unpark_stream(&mut routed);
        // The pad is leaving, so whatever its park was holding for a join has
        // no join coming. Dropping it here and not on a timer is what keeps
        // `parked_text_cues` bounded by the number of LIVE text pads.
        inner.forget_parked_text_cues(pad);
        // A/V held a streamsynchronizer request pad. Text bypassed ssync and
        // has none. Unlink and release only when present.
        if let Some(ssync_sink) = &routed.ssync_sink {
            let _ = pad.unlink(ssync_sink);
            // Release against the pad's OWN streamsynchronizer: after a core
            // swap this stream belongs to the previous instance.
            if let Some(ssync) = ssync_sink.parent_element() {
                ssync.release_request_pad(ssync_sink);
            }
        }
        debug!(pad = %pad.name(), kind = ?routed.kind, "unrouted decodebin3 pad");

        // Text is consumed synchronized against VIDEO buffers, so a text
        // stream that stays linked after video stops can never
        // drain and blocks decodebin3's reconfiguration until the next
        // flush. Park linked text when video unroutes, and the policy
        // brings it back once a video stream is routed again.
        if routed.kind == StreamKind::Video {
            // Read by the drained-resurrect park in `route_db3_pad`.
            inner.video_unrouted_once.store(true, Ordering::SeqCst);
            // Both halves are pipeline surgery and this runs on decodebin3's
            // pad-removed callback, so they go to the worker. See
            // [`Job::VideoChainGone`] for the wedge that taught us.
            //
            // A/B lever for bisecting regressions without a rebuild, like
            // FCAST_NO_SELECTION_REPLAY.
            if std::env::var_os("FCAST_INLINE_VIDEO_CHAIN_TEARDOWN").is_some() {
                Inner::park_text_streams(inner);
                inner.remove_video_chain();
            } else {
                inner.queue_job(Job::VideoChainGone);
            }
        }
    }

    /// Stream kind from the pad's sticky stream-start event (decodebin3
    /// stamps a GstStream on its output pads), with a caps fallback.
    fn stream_kind_of(pad: &gst::Pad) -> Option<StreamKind> {
        if let Some(stream) = pad.stream() {
            let ty = stream.stream_type();
            if ty.contains(gst::StreamType::VIDEO) {
                return Some(StreamKind::Video);
            }
            if ty.contains(gst::StreamType::AUDIO) {
                return Some(StreamKind::Audio);
            }
            if ty.contains(gst::StreamType::TEXT) {
                return Some(StreamKind::Text);
            }
        }
        let caps = pad.current_caps()?;
        decisions::kind_from_caps_name(caps.structure(0)?.name())
    }
}

impl FcastPlaybin {
    /// How many [`Inner::remove_input`] pads got the flush pair. Every one
    /// briefly de-PLAYs the pipeline, and the count is the size of the problem
    /// that has NOT been solved (see `Inner::remove_input`). Not
    /// part of the public API.
    #[doc(hidden)]
    pub fn remove_input_pairs_sent() -> u64 {
        REMOVE_INPUT_PAIRS_SENT.load(Ordering::SeqCst)
    }

    /// How many `remove_input` pads a quiescence probe let through with
    /// no pair. Zero unless `FCAST_REMOVE_INPUT_FLUSH_SKIP` is set. Not part
    /// of the public API.
    #[doc(hidden)]
    pub fn remove_input_pairs_skipped() -> u64 {
        REMOVE_INPUT_PAIRS_SKIPPED.load(Ordering::SeqCst)
    }

    /// Diagnostic: "kind:pad-name" for every currently-routed decodebin3
    /// stream. Compare against the media's stream collection to spot a
    /// selected stream whose pad never got routed.
    pub fn routed_summary(&self) -> Vec<String> {
        self.inner
            .routing
            .lock()
            .routed
            .iter()
            .map(|r| format!("{:?}:{}", r.kind, r.db3_src_pad.name()))
            .collect()
    }
}
