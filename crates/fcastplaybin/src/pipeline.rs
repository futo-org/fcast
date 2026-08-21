//! Pipeline construction and the caller-facing transport surface: build,
//! load, play/pause/stop, seek, volume, position and the debug readouts.

use std::sync::{
    Arc, OnceLock,
    atomic::{AtomicBool, AtomicU64, Ordering},
    mpsc,
};

use anyhow::{Context, Result};
use gst::prelude::*;
use parking_lot::Mutex;
use tracing::{debug, info, warn};

use crate::{
    Core, Counters, FcastPlaybin, Inner,
    api::{
        AudioSink, MediaInput, PlaybinEvent, Sinks, SourceDbg, StartOutcome, StartPoint,
        StreamIoStats,
    },
    decisions,
    flush::FlowStage,
    gapless::SwapGate,
    hands::{Hands, Lane},
    jobs::Job,
    routing::StreamKind,
    selection,
};

/// Bounded wait for a load's (re-)preroll. Bounded on purpose: an unbounded
/// `get_state(None)` here would wedge the caller's worker if preroll stalled.
pub(crate) const PREROLL_TIMEOUT: gst::ClockTime = gst::ClockTime::from_seconds(10);

/// Audio decoupling queue depth (`fpb-aqueue` `max-size-time`). Chosen purely
/// for decoupling headroom: it absorbs sink scheduling jitter so a busy CPU
/// never starves the audio sink. 1s is generous (audio decodes far faster than
/// realtime and upstream use-buffering handles the source). It must stay below
/// the gapless pre-arm lead so the EOS-hold at decodebin3's output still
/// catches a late pre-arm's outgoing EOS (the pre-arm arms seconds before the
/// end; 1s of race-ahead is well inside that). It does NOT set the gapless
/// transition seam any more: the user-facing item switch is HELD and released
/// when the new item's audio actually crosses this queue to the sink (see
/// [`Inner::held_activation`]), so the title/duration flip with the sound
/// regardless of this depth. Deep only while a video chain is present, to
/// absorb audio during a video re-preroll (the A/V mid-load deadlock the queue
/// exists for). See `aqueue` in `new`.
const AQUEUE_AUDIO_TIME_NS: u64 = 1_000_000_000;

const AQUEUE_VIDEO_TIME_NS: u64 = 30 * 1_000_000_000;

pub(crate) fn make(factory: &str, name: &str) -> Result<gst::Element> {
    gst::ElementFactory::make(factory)
        .name(name)
        .build()
        .with_context(|| format!("creating {factory} ({name})"))
}

/// The flushing seek event [`send_rate_seek`] sends (keyframe-landing, no
/// ACCURATE), so every other issuer of "the same seek" (the refresh flush,
/// the external-input forwarding) lands on exactly the same timeline instead
/// of re-deriving it.
/// `seqnum` stamps the event for callers that need to attribute the answer.
pub(crate) fn rate_seek_event(
    rate: f64,
    position: gst::ClockTime,
    seqnum: Option<gst::Seqnum>,
) -> gst::Event {
    let flags = decisions::seek_flags_for(rate);
    let builder = if rate >= 0.0 {
        gst::event::Seek::builder(
            rate,
            flags,
            gst::SeekType::Set,
            position,
            gst::SeekType::None,
            gst::ClockTime::NONE,
        )
    } else {
        gst::event::Seek::builder(
            rate,
            flags,
            gst::SeekType::Set,
            gst::ClockTime::ZERO,
            gst::SeekType::End,
            position,
        )
    };
    match seqnum {
        Some(seqnum) => builder.seqnum(seqnum).build(),
        None => builder.build(),
    }
}

/// A flushing keyframe seek to `position` at `rate`, handling reverse rates
/// (seek from the end).
pub(crate) fn send_rate_seek(
    pipeline: &gst::Pipeline,
    rate: f64,
    position: gst::ClockTime,
) -> std::result::Result<(), gst::glib::error::BoolError> {
    // `Element::seek` is new_seek + send_event, so send the shared builder's
    // event rather than re-deriving the same seek here.
    if pipeline.send_event(rate_seek_event(rate, position, None)) {
        Ok(())
    } else {
        Err(gst::glib::bool_error!("Failed to seek"))
    }
}

impl Inner {
    /// Build and install a fresh dynamic core (see `Core`): decodebin3 +
    /// streamsynchronizer, added to the pipeline at its current state, with
    /// the routing handlers connected.
    fn install_core(inner: &Arc<Inner>) -> Result<()> {
        let db3 = make("decodebin3", "fpb-decodebin")?;
        let ssync = make("streamsynchronizer", "fpb-ssync")?;
        inner
            .pipeline
            .add_many([&db3, &ssync])
            .context("adding the dynamic core")?;
        db3.sync_state_with_parent().context("syncing decodebin3")?;
        ssync
            .sync_state_with_parent()
            .context("syncing streamsynchronizer")?;

        // decodebin3 output pads appear per SELECTED stream. Route them
        // through streamsynchronizer into the chains.
        let pad_added_sig = db3.connect_pad_added({
            let inner = Arc::downgrade(inner);
            move |_, pad| {
                let Some(inner) = inner.upgrade() else { return };
                if let Err(err) = Inner::route_db3_pad(&inner, pad) {
                    warn!(?err, pad = %pad.name(), "failed to route decodebin3 pad");
                }
            }
        });
        let pad_removed_sig = db3.connect_pad_removed({
            let inner = Arc::downgrade(inner);
            move |_, pad| {
                let Some(inner) = inner.upgrade() else { return };
                Inner::unroute_db3_pad(&inner, pad);
            }
        });

        *inner.core.lock() = Some(Core {
            db3,
            ssync,
            pad_added_sig,
            pad_removed_sig,
        });
        Ok(())
    }

    /// Tear down the previous load's dynamic core: clean up any stream
    /// still routed through it, then NULL and drop decodebin3 and
    /// streamsynchronizer.
    fn teardown_core(inner: &Arc<Inner>) {
        let Some(core) = inner.core.lock().take() else {
            return;
        };
        core.db3.disconnect(core.pad_added_sig);
        core.db3.disconnect(core.pad_removed_sig);

        // Any pads deferred from THIS (now superseded) core are stale and
        // the drainer would only re-reject them. Drop them so a fresh load
        // starts clean.
        inner.deferred_pads.lock().clear();

        // Streams normally unroute via pad-removed when the inputs are
        // released. Clean up any straggler entry the same way.
        let leftover = std::mem::take(&mut inner.routing.lock().routed);
        for mut routed in leftover {
            if routed.kind == StreamKind::Text {
                Inner::detach_text_branch(inner, &mut routed, false);
            } else if let (Some(ssync_src), Some(downstream)) =
                (&routed.ssync_src, &routed.downstream)
            {
                let _ = ssync_src.unlink(downstream);
            }
            inner.unpark_stream(&mut routed);
        }

        for element in [&core.db3, &core.ssync] {
            let _ = element.set_state(gst::State::Null);
            let _ = inner.pipeline.remove(element);
        }
        debug!("dropped the previous load's dynamic core");
    }

    /// Build the current load's audio sink and wire it `volume ! sink` if it
    /// isn't up yet. Idempotent within a load. The sink joins the running
    /// pipeline at `join_state`. Its base_time comes from `gst_bin_add`,
    /// which stamps the bin's current one: valid for a steady join, and a
    /// mid-load join is re-stamped by the commit walk.
    pub(crate) fn ensure_audio_sink(&self) -> Result<()> {
        let mut slot = self.audio_sink.lock();
        if slot.is_some() {
            return Ok(());
        }
        let sink = match &self.audio {
            // No fixed name (auto-unique per load), so nothing keyed off the
            // element name can collide with the previous load's
            // still-finalizing sink.
            AudioSink::Auto => gst::ElementFactory::make("autoaudiosink")
                .build()
                .context("creating autoaudiosink")?,
            AudioSink::Factory(factory) => factory().context("building the audio sink")?,
        };
        self.pipeline.add(&sink).context("adding the audio sink")?;
        self.volume
            .link(&sink)
            .context("linking volume to the audio sink")?;
        sink.set_state(self.join_state())
            .context("syncing the audio sink")?;
        *slot = Some(sink);
        Ok(())
    }

    /// Drop the current load's audio sink (see `Inner::audio`): unlink
    /// `volume ! sink`, NULL it, remove it, drop the ref so its pulse
    /// context is fully released. Call only at a quiescent point (load
    /// reset under the route gate): NULLing a linked, streaming sink in
    /// place races its teardown and crashes.
    pub(crate) fn remove_audio_sink(&self) {
        let Some(sink) = self.audio_sink.lock().take() else {
            return;
        };
        self.volume.unlink(&sink);
        let _ = sink.set_state(gst::State::Null);
        let _ = self.pipeline.remove(&sink);
    }

    /// The state a dynamically (re)activated element is driven to so it
    /// joins the pipeline WITHOUT outrunning an in-flight async transition.
    ///
    /// NOT `sync_state_with_parent`: that targets the pipeline's TARGET
    /// state, so a sink activated during a load self-continues to PLAYING
    /// off its own preroll BEFORE the PAUSED->PLAYING commit distributes the
    /// new base_time. The sink then syncs against the previous load's
    /// base_time (or no clock at all on the first load) and every playback
    /// start opened with a QoS drop storm culling the first ~1s of video.
    /// Joining at PAUSED instead parks the preroll in the async set the
    /// commit already waits on, and the commit's child walk lifts the
    /// element to PLAYING with the freshly-selected base_time. If the walk
    /// races past an element mid-activation, its ASYNC_START makes the bin
    /// lose state and re-commit (the standard dynamic-sink dance), so
    /// nothing parks for good.
    ///
    /// With no transition in flight, match the pipeline exactly: the normal
    /// late-joining-sink case (but see the stamp in `ensure_video_chain`).
    pub(crate) fn join_state(&self) -> gst::State {
        let (_, current, pending) = self.pipeline.state(gst::ClockTime::ZERO);
        decisions::join_state(current, pending)
    }

    /// Put the video chain into the pipeline and bring it to the join state.
    /// Called from `route_db3_pad` when a video stream routes. Idempotent, and
    /// also the recovery from a mid-item deselect's parked chain (unlocks and
    /// re-joins it). The chain lives in the pipeline ONLY while the item has
    /// video: an absent chain cannot hang a video-less preroll and never
    /// counts in the bin's EOS/STREAM_START aggregation, by construction.
    /// The MEMBERSHIP half of [`Inner::ensure_video_chain`]: put the chain in
    /// the pipeline. Split out because a
    /// caller must be able to do this without the state half, which blocks:
    /// `gst_pad_link` refuses a link whose two pads have no common ancestor
    /// (`GST_PAD_LINK_CHECK_HIERARCHY`), so the chain has to be IN the
    /// pipeline before `route_db3_pad` can link a stream into it, even when
    /// the activation itself is deferred (see [`ChainJoinJob`]).
    ///
    /// The chain is `fpb-vqueue ! sink` (see `Inner::video_entry`). The
    /// internal edge is made on the first attach and kept across membership
    /// changes, the same treatment the deleted overlay's edge had.
    ///
    /// Nothing here blocks on a state or stream lock: `gst_bin_add` takes the
    /// bin's object lock and changes no child state.
    ///
    /// Under `video_chain_membership` because a route and a chain join now run
    /// concurrently (see [`Inner::join_gate`]): two threads both finding the
    /// chain out of the pipeline would both add it, and the loser's
    /// `gst_bin_add` fails, taking its whole route with it.
    pub(crate) fn attach_video_chain(&self) -> Result<()> {
        let _membership = self.video_chain_membership.lock();
        if self.video_sink.parent().is_some() {
            return Ok(());
        }
        self.pipeline
            .add_many([&self.video_entry, &self.video_sink])
            .context("adding the video chain")?;
        // First attach only: the `vqueue ! sink` edge is kept across
        // membership changes, like the deleted overlay's edge before it.
        if self
            .video_entry
            .static_pad("src")
            .is_some_and(|pad| pad.peer().is_none())
        {
            self.video_entry
                .link(&self.video_sink)
                .context("linking the video chain")?;
        }
        Ok(())
    }

    pub(crate) fn ensure_video_chain(&self) -> Result<()> {
        // A previous deselect's drop probe would silently eat the stream this
        // join exists to render, so it comes off first (see
        // `park_video_chain_for_deselect`).
        self.clear_video_park_probe();
        // A video chain can re-preroll mid-load and needs the deep audio
        // buffer to avoid the demuxer-stall deadlock (see `aqueue` in `new`).
        self.audio_entry
            .set_property("max-size-time", AQUEUE_VIDEO_TIME_NS);
        // Idempotent, and already done by a deferred route (see
        // `attach_video_chain`).
        self.attach_video_chain()?;
        let join = self.join_state();
        // Joining a steady PLAYING pipeline renders immediately, so stamp
        // the pipeline's current base_time first: the chain missed every
        // commit walk while it was out of the pipeline, so its own base_time
        // is stale, possibly by many loads.
        let base_time = (join == gst::State::Playing)
            .then(|| self.pipeline.base_time())
            .flatten();
        // The unlock undoes a mid-item deselect's park (see
        // `park_video_chain_for_deselect`).
        self.video_sink.set_locked_state(false);
        if let Some(base_time) = base_time {
            self.video_sink.set_base_time(base_time);
        }
        // Sink before queue (downstream up): the queue's task pushes the
        // moment it activates, and a push into a still-READY sink returns
        // FLUSHING, which parks the task with nothing to resume it.
        if let Err(err) = self.video_sink.set_state(join) {
            warn!(?err, element = %self.video_sink.name(), "failed to activate the video sink");
        }
        self.video_entry.set_locked_state(false);
        if let Err(err) = self.video_entry.set_state(join) {
            warn!(?err, element = %self.video_entry.name(), "failed to activate the video queue");
        }
        // Cluster (d) of the four surgery sites, relink half: both ends of the
        // freshly joined edge stay in the graph, so a FLUSHING latched on
        // either is a chain that will never render. The sink pad rides along:
        // the internal `vqueue ! sink` edge never unlinks, but a flush
        // latches it all the same.
        let relinked: Vec<gst::Pad> = self
            .video_entry
            .static_pad("sink")
            .into_iter()
            .flat_map(|pad| pad.peer().into_iter().chain(std::iter::once(pad)))
            .chain(self.video_sink.static_pad("sink"))
            .collect();
        Self::flow_census(FlowStage::EnsureVideoChain, &relinked);
        Ok(())
    }

    /// Take the video chain out of the pipeline: UNLINK from upstream first
    /// (A-6, see the body - READY-ing elements that are still linked and
    /// flowing races gstbasetransform's unlocked `queued_buf` free into a
    /// double unref), then READY it (aborting any clock/preroll
    /// wait, which unwinds a blocked streaming thread out of the branch) and
    /// remove it. The caller's
    /// video sink is GL/window-bound and parks at READY outside the
    /// pipeline, never NULLed (playbin3's own treatment of it). Runs at the
    /// load reset and when a mid-item video deselect completes
    /// (`unroute_db3_pad`). Once removed, the bin's EOS aggregation can no
    /// longer wait on a sink that will never see data again.
    ///
    /// This also used to NULL subtitleoverlay so no
    /// caps/renderer state leaked into its next join (a stale subtitle
    /// renderer wedged the load after a VOBSUB selection). That element is
    /// gone; the never-NULL rule above is what is left, and it always applied
    /// to the sink.
    pub(crate) fn remove_video_chain(&self) {
        // The pad the deselect's probe sits on is about to be unlinked and
        // released; take the probe off while it still exists.
        self.clear_video_park_probe();
        // No video chain to deadlock: restore the shallow audio-only queue so
        // gapless holds the outgoing EOS near the sink boundary (see `aqueue`
        // in `new`). Unconditional: a load reset removes the chain and re-shallows.
        self.audio_entry
            .set_property("max-size-time", AQUEUE_AUDIO_TIME_NS);
        if self.video_sink.parent().is_none() {
            return;
        }
        // A-6: UNLINK FIRST, then descend.
        //
        // gstbasetransform frees its `queued_buf` unlocked behind an empty
        // STREAM_LOCK barrier, so READY-ing elements that are still linked and
        // still flowing races that free into a double unref. `gst_pad_unlink`
        // takes no stream lock and is safe at any state, and once the video
        // sink's peer is gone the transform upstream of it can only ever see
        // NOT_LINKED, which it handles. The old order (READY loop first, unlink
        // second) is the A3/A-6 double-unref hazard
        // (`ASSERTION-LANDMINES.md` section 2 A-6).
        //
        // Order-independent neighbours stay where they are: the deselect
        // probe comes off first (above; its pad is about to be released) and
        // the audio queue re-shallows unconditionally.
        //
        // The peer is read BEFORE the unlink, so the flow census below can
        // still name the pad the unlink removed.
        let entry = self.video_entry.static_pad("sink");
        let peer = entry.as_ref().and_then(|pad| pad.peer());
        // Unlink from upstream (the streamsynchronizer src, when a stream
        // is still routed into the sink).
        if let (Some(pad), Some(peer)) = (entry.as_ref(), peer.as_ref()) {
            let _ = peer.unlink(pad);
        }
        // Then READY: aborts any clock/preroll wait, unwinding a blocked
        // streaming thread out of the branch. Sink before queue: the sink's
        // READY returns the queue task's in-flight push as FLUSHING, so the
        // queue's pad deactivation is not left waiting on a thread parked
        // inside the sink's preroll.
        self.video_sink.set_locked_state(false);
        let _ = self.video_sink.set_state(gst::State::Ready);
        self.video_entry.set_locked_state(false);
        let _ = self.video_entry.set_state(gst::State::Ready);
        // Cluster (d) of the four surgery sites. The chain's own pads
        // leave the pipeline just below, but their PEER (a streamsynchronizer
        // src pad) stays, and a FLUSHING latched there is what the next
        // `ensure_video_chain` would relink into.
        let surveyed: Vec<gst::Pad> = peer.into_iter().collect();
        Self::flow_census(FlowStage::RemoveVideoChain, &surveyed);
        let _ = self
            .pipeline
            .remove_many([&self.video_entry, &self.video_sink]);
        debug!("removed the video chain from the pipeline");
    }

    /// The load's preroll is now carried by a real sink's async (the caller
    /// just activated a chain), so retire the token (see `Inner::token_src`).
    /// Repeats (the second routed chain, post-EOS pushes) are harmlessly
    /// rejected by appsrc.
    pub(crate) fn finish_preroll_token(&self) {
        let _ = self
            .token_src
            .emit_by_name::<gst::FlowReturn>("push-buffer", &[&gst::Buffer::new()]);
        let _ = self
            .token_src
            .emit_by_name::<gst::FlowReturn>("end-of-stream", &[]);
    }

    /// The new selection drops video entirely: park the chain at READY
    /// immediately, mid-item. Without this the video-disable reconfiguration
    /// can deadlock the whole pipeline: the selection change briefly hiccups
    /// audio, the audio-sink-provided pipeline clock freezes, the video sink
    /// sits in `wait_clock` forever, the decodebin3 video slot never goes
    /// IDLE, and the full slot backpressures into the demuxer, which never
    /// produces the audio that would restart the clock. The READY descent
    /// aborts the clock wait, letting the slot idle and the deactivation
    /// finish.
    ///
    /// Three constraints:
    /// - NOT a flush: basesink would post ASYNC_START and wedge the pipeline at
    ///   pending PAUSED on a re-preroll no data will finish.
    /// - NOT a READY descent, which is what this used to be. It aborts the
    ///   clock wait of the multiqueue slot task mid-push, and the resulting
    ///   GST_FLOW_FLUSHING is fatal to `gst_multi_queue_loop`: it parks the
    ///   demuxer on a FLUSH_STOP nobody will send, so the stream dies with no
    ///   EOS and no error and a later re-select gets a pad with no data
    ///   (`fuzz_buffering` 1600031). A DROP probe instead returns GST_FLOW_OK
    ///   (push probes run before the peer lookup), so the slot drains, the
    ///   clock keeps advancing and the thread leaves on its own, cutting the
    ///   backpressure cycle at its source. Buffers only, so ssync grouping and
    ///   the sink's EOS still work. The READY descent survives only as the
    ///   fallback for a chain with no feeding peer to probe.
    /// - The state LOCK holds until decodebin3 removes the pad, or a state
    ///   change walking its children would lift the dataless chain back up and
    ///   its sink would hold the pipeline async forever. `unroute_db3_pad` then
    ///   removes the chain entirely and a re-select rebuilds it.
    pub(crate) fn park_video_chain_for_deselect(&self) {
        if self.video_sink.parent().is_none() {
            return;
        }
        self.video_sink.set_locked_state(true);
        // Two deselect dispatches in a row would otherwise leak the first
        // probe onto a pad this one is about to stop tracking.
        self.clear_video_park_probe();
        // The chain's entry is the queue; its peer is the feeding ssync pad.
        let feeding = self
            .video_entry
            .static_pad("sink")
            .and_then(|pad| pad.peer());
        if let Some(feeding) = feeding {
            info!(
                pad = %feeding.name(),
                "selection drops video, dropping the data feeding the video chain"
            );
            let id = feeding.add_probe(
                gst::PadProbeType::BUFFER | gst::PadProbeType::BUFFER_LIST,
                |_pad, _info| gst::PadProbeReturn::Drop,
            );
            if let Some(id) = id {
                *self.video_park_probe.lock() = Some((feeding, id));
            }
            self.lift_deselected_video_sink();
            return;
        }
        info!("selection drops video, parking the video chain at READY");
        // The sink's READY aborts its clock/preroll wait, unwinding the
        // blocked streaming thread out of the branch.
        let _ = self.video_sink.set_state(gst::State::Ready);
    }

    /// Let the ONE video push already inside the chain finish, so decodebin3
    /// can complete the deactivation.
    ///
    /// The DROP probe above stops NEW data, and no probe can touch a push that
    /// is already past it. At a pipeline resting in PAUSED that push sits in
    /// the video sink's `gst_base_sink_wait_preroll`, whose only two exits
    /// gstbasesink.c:2438 names itself: "waiting in preroll for flush or
    /// PLAYING". Until it returns, the decodebin3 multiqueue slot's src pad is
    /// never idle, so the IDLE probe `handle_stream_switch` arms to run
    /// `mq_slot_unassign` (gstdecodebin3.c:4566) never fires,
    /// `is_selection_done` keeps bailing out with "Stream from previous
    /// selection still active" (:3358), and NO `STREAMS_SELECTED` is posted for
    /// the selection AT ALL. The caller's track change then never confirms:
    /// FAST `video_disable_while_paused_v4`, 18 s of silence and a receiver
    /// still waiting.
    ///
    /// Of the two exits, the flush is the one this crate must never take: it
    /// returns FLUSHING into the multiqueue, which latches it, which is exactly
    /// what the reverted READY descent did. So take the other one and lift the
    /// SINK to PLAYING. The parked push returns OK, the pad idles between
    /// pushes, and decodebin3 finishes the deactivation itself and posts the
    /// REAL message, so no crate bookkeeping is short-circuited.
    ///
    /// Only at a PAUSED pipeline: at PLAYING nothing needs preroll, and below
    /// PAUSED the chain is going down anyway. The audio chain never needed this
    /// because `fpb-aqueue` absorbs its slot's pushes, which is why only VIDEO
    /// deselects wedge here.
    ///
    /// Cost: the frames already past the probe (the parked one) render onto
    /// the paused frame just before video goes away.
    fn lift_deselected_video_sink(&self) {
        let (_, current, _) = self.pipeline.state(gst::ClockTime::ZERO);
        if current != gst::State::Paused {
            return;
        }
        let sink = &self.video_sink;
        debug!(
            sink = %sink.name(),
            "lifting the deselected video sink to PLAYING so its parked preroll push returns"
        );
        if let Err(err) = sink.set_state(gst::State::Playing) {
            warn!(?err, "failed to lift the deselected video sink");
        }
    }

    /// Lift a mid-item video deselect's DROP probe (see
    /// `park_video_chain_for_deselect`). Called wherever the video chain
    /// changes hands, so a re-selected video stream can never be dropped by a
    /// previous deselect's probe. A no-op when nothing is parked.
    fn clear_video_park_probe(&self) {
        if let Some((pad, id)) = self.video_park_probe.lock().take() {
            debug!(pad = %pad.name(), "lifting the deselected video chain's drop probe");
            pad.remove_probe(id);
        }
    }

    /// The timeline text is rendered against: the rate and the stream
    /// position that running time is measured from, read off the segment the
    /// VIDEO SINK is showing frames against. A flushing seek
    /// moves that origin to its target (segment start, base 0), so it is
    /// only zero while nothing has sought yet. Text whose own segment
    /// starts elsewhere renders shifted by the difference. Falls back to
    /// (1.0, ZERO) before the first segment arrives.
    ///
    /// Read one element further down than it used to be (the
    /// deleted overlay's `video_sink`), off the same event travelling the same
    /// branch. Which is now also the honest place for it: the cue engine that
    /// decides what is on screen lives in that sink.
    pub(crate) fn video_timeline(&self) -> (f64, gst::ClockTime) {
        // No segment at the sink yet (fresh load, or a start seek's flush
        // cleared it): the recorded intent is the best truth available, and
        // zero is only right when nothing ever sought.
        let fallback = *self.intended_timeline.lock();
        let Some(event) = self
            .video_sink
            .static_pad("sink")
            .and_then(|pad| pad.sticky_event::<gst::event::Segment>(0))
        else {
            return fallback;
        };
        let Some(segment) = event.segment().downcast_ref::<gst::ClockTime>() else {
            return fallback;
        };
        let rate = segment.rate();
        let start = segment.start().unwrap_or(gst::ClockTime::ZERO);
        // Running time is (position - start) / |rate| + base, so the origin
        // (the position whose running time is zero) sits base * |rate| of
        // stream time below `start`.
        let base =
            (segment.base().unwrap_or(gst::ClockTime::ZERO).nseconds() as f64 * rate.abs()) as u64;
        let origin = gst::ClockTime::from_nseconds(start.nseconds().saturating_sub(base));
        // A reverse segment cannot be expressed as a forward replay.
        if rate > 0.0 {
            (rate, origin)
        } else {
            (1.0, origin)
        }
    }

    /// Send `seek` straight into the load's MAIN input, on every source pad
    /// it has exposed. This is the SOURCE-side twin of a pipeline seek, and
    /// the only delivery that does not depend on the output graph: a
    /// `pipeline.seek()` is an upstream event, so `GstBin` hands it to the
    /// sink children present AT THAT INSTANT and to nothing else (see
    /// `gst_bin_send_event`), and every branch that has no sink downstream
    /// yet is simply skipped. Since the load's chains join one at a time (a
    /// chain per routed stream) and the preroll token retires on the FIRST of
    /// them, "the pipeline is prerolled" does not mean "every stream has a
    /// sink". Delivering at the input instead reaches each elementary
    /// stream's source no matter what the output side looks like: a branch
    /// whose chain has not joined still gets its post-seek segment as a
    /// sticky event, which replays downstream the moment the chain links.
    ///
    /// Returns whether the seek was delivered at all. An input with no source
    /// pads yet, or one that refused every pad, returns false so the caller
    /// falls back to the pipeline broadcast rather than silently skipping the
    /// start position. A pad that refuses individually is NOT worth the
    /// fallback: the broadcast would travel up that same branch and be refused
    /// by the same element, so it would only buy a second flush.
    pub(crate) fn seek_main_input(&self, event: &gst::Event) -> bool {
        let generation = self.current_generation();
        let targets: Vec<gst::Pad> = {
            let routing = self.routing.lock();
            routing
                .inputs
                .iter()
                // The main input only: externals have their own forwarding
                // below, and a PREPARED next item (registered under its
                // future generation) must not be dragged onto this item's
                // timeline.
                .filter(|i| i.external.is_none() && i.generation == generation)
                .flat_map(|i| i.element.src_pads())
                .collect()
        };
        if targets.is_empty() {
            return false;
        }
        let mut accepted = 0usize;
        for pad in &targets {
            if pad.send_event(event.clone()) {
                accepted += 1;
            } else {
                warn!(pad = %pad.name(), "the main input refused the start seek");
            }
        }
        debug!(
            pads = targets.len(),
            accepted,
            // A pad not yet linked into decodebin3 is one the pad-added
            // handler is still wiring; measured over a suite soak this is
            // always zero here, which is why the delivery does not need to
            // exclude them.
            unlinked = targets.iter().filter(|pad| !pad.is_linked()).count(),
            "start seek: delivered to the main input's source pads"
        );
        accepted > 0
    }
}

impl FcastPlaybin {
    pub fn new(sinks: Sinks) -> Result<Self> {
        let pipeline = gst::Pipeline::builder().name("fcastplaybin").build();

        // The fake sink is a test/spike convenience, not a headless mode:
        // video still fully decodes into it. Callers that want to skip video
        // work deselect the video stream instead.
        let video_sink = match sinks.video {
            Some(sink) => {
                // playsink parity: QoS events from the video sink drive
                // decoder frame-skipping and fimagedec's schedule rebase.
                // Without them a slow decode path silently drops at the sink
                // with no way for upstream to adapt.
                if sink.has_property_with_type("qos", bool::static_type()) {
                    sink.set_property("qos", true);
                }
                sink
            }
            None => {
                let sink = make("fakesink", "fpb-fake-vsink")?;
                sink.set_property("sync", true);
                sink
            }
        };

        let aconv = make("audioconvert", "fpb-aconv")?;
        let aresample = make("audioresample", "fpb-aresample")?;
        // Pitch-preserving rate change. `fcastaudiostretch` (PICOLA) replaced
        // `scaletempo` (SOLA), which buzzed on speech at non-1.0 rates and
        // needed `scaletempo-s16-overlap-overflow.patch` for a click on the
        // first buffer after engaging. Both consume the segment rate
        // identically, so the swap was a drop-in.
        let stretch = make("fcastaudiostretch", "fpb-audiostretch")?;
        let volume = make("volume", "fpb-volume")?;
        // Decoupling queue at the head of the audio branch. Without it, a
        // paused audio sink (parked in wait_preroll during a mid-load
        // re-preroll) backpressures through streamsynchronizer into
        // decodebin3's multiqueue and stalls the single demuxer thread,
        // which then can't feed VIDEO, so the video sink never re-prerolls
        // and the whole pipeline deadlocks. The queue absorbs the audio that
        // piles up during the video re-preroll window. Bounded by TIME (the
        // default 1s cap is what bites, VIDEO depth caps memory to a few MB
        // of PCM) with no min-threshold, so it adds no playback latency.
        //
        // The depth is video-conditional (see `AQUEUE_*_TIME_NS`): the deep
        // buffer is ONLY needed while a video chain can re-preroll. For
        // audio-only playback a deep queue is actively harmful to gapless: it
        // carries the outgoing item's decoded EOS past the gapless EOS-hold
        // (at decodebin3's output, upstream of this queue) tens of seconds
        // before the speakers reach it, so a pre-arm that lands later (the
        // field: the sender queues the next track seconds before the end) is
        // too late and the item is skipped. A shallow audio-only queue keeps
        // the hold near the sink boundary. `ensure_video_chain` deepens it.
        let aqueue = make("queue", "fpb-aqueue")?;
        aqueue.set_property("max-size-time", AQUEUE_AUDIO_TIME_NS);
        aqueue.set_property("max-size-bytes", 0u32);
        aqueue.set_property("max-size-buffers", 0u32);

        // Head of the video chain, playsink's video-queue parity (see
        // `Inner::video_entry`). 3 buffers like playsink's, so it holds at
        // most 3 decoded frames of pool surfaces; time cap 0 is what makes
        // the latency query answer max=unlimited. NOT added to the pipeline
        // here: it joins and leaves with the video sink
        // (`attach_video_chain`/`remove_video_chain`).
        let vqueue = make("queue", "fpb-vqueue")?;
        vqueue.set_property("max-size-time", 0u64);
        vqueue.set_property("max-size-bytes", 0u32);
        vqueue.set_property("max-size-buffers", 3u32);

        let token_src = make("appsrc", "fpb-token-src")?;
        token_src.set_property_from_str("format", "time");
        let token_sink = make("fakesink", "fpb-token-sink")?;
        token_sink.set_property("sync", false);
        token_sink.set_property("enable-last-sample", false);
        // The token must carry the load's ASYNC (a message-level mechanism)
        // but stay invisible to everything GstBin routes through SINK-flagged
        // children: seeking queries (appsrc answers "not seekable" and
        // poisons the pipeline's seekability), seek events (a flushing seek
        // would flush the token's preroll away and hang waiting for it), and
        // EOS aggregation.
        token_sink.unset_element_flags(gst::ElementFlags::SINK);

        // The video sink is NOT added here: it lives in the pipeline only
        // while the item has a routed video stream
        // (`ensure_video_chain`/`remove_video_chain`), exactly like
        // the per-load audio sink. An absent chain cannot hang a video-less
        // preroll and cannot swallow the bin's EOS/STREAM_START aggregation,
        // by construction (this replaces the old locked-state + SINK-flag
        // deactivation games).
        pipeline.add_many([
            &aqueue,
            &aconv,
            &aresample,
            &stretch,
            &volume,
            &token_src,
            &token_sink,
        ])?;
        token_src.link(&token_sink)?;

        // Static links. Everything upstream of these is dynamic. The video
        // sink links DIRECTLY to streamsynchronizer, no converter in between:
        // the receiver's sink negotiates DMA-BUF/zero-copy caps that a
        // videoconvert would reject, and accepts plain raw video too.
        // Callers with a pickier sink wrap it in a bin with a converter.
        // The audio sink is built and linked per load (`ensure_audio_sink`).
        gst::Element::link_many([&aqueue, &aconv, &aresample, &stretch, &volume])?;

        let (work_tx, work_rx) = mpsc::channel();
        // One channel per hands lane, carrying `Envelope`s to `lane_loop`.
        let (select_tx, select_rx) = mpsc::channel();
        let (replay_tx, replay_rx) = mpsc::channel();
        let (join_tx, join_rx) = mpsc::channel();
        // `fpb-tick`'s channel carries no work at all: it exists only so that
        // dropping the sender with `Inner` hangs the thread up (the
        // `fpb-tdrescue` pattern, minus the join).
        let (tick_tx, tick_rx) = mpsc::channel::<()>();

        let inner = Arc::new(Inner {
            video_sink,
            audio: sinks.audio,
            audio_sink: Mutex::default(),
            pipeline,
            core: Mutex::default(),
            token_src,
            route_gate: Mutex::default(),
            join_gate: Mutex::default(),
            deferred_pads: Mutex::default(),
            deferred_text_disposal: Mutex::default(),
            deferred_input_removal: Mutex::default(),
            replaying_externals: Mutex::default(),
            external_cues_fed: Mutex::default(),
            pending_timers: Mutex::default(),
            drain_poke_parked: AtomicBool::default(),
            video_deselected: AtomicBool::default(),
            video_unrouted_once: AtomicBool::default(),
            teardown_poisoned: AtomicBool::default(),
            generation: AtomicU64::default(),
            next_generation: AtomicU64::default(),
            // The audio branch's head is the decoupling queue. ssync links here.
            audio_entry: aqueue,
            volume,
            // The video branch's head. ssync links here (see `Inner::video_entry`).
            video_entry: vqueue,
            events: Mutex::default(),
            subtitle_consumer: Mutex::default(),
            text_degradations: Mutex::default(),
            parked_text_cues: Mutex::default(),
            suppress_text_clear: Mutex::default(),
            // TEST FAULT INJECTION, left empty. Nothing allocates it until a
            // `stage_*` setter runs (see `TestStaging`).
            staging: std::sync::OnceLock::new(),
            counters: Counters::default(),
            work_tx,
            queue_epoch: AtomicU64::default(),
            text_flow_ticket: AtomicU64::default(),
            poll_queued: AtomicBool::default(),
            decider: OnceLock::new(),
            hands: Hands::new(select_tx, replay_tx, join_tx),
            tick_tx,
            tick_count: AtomicU64::default(),
            routing: Mutex::default(),
            selection: Mutex::new(selection::SelectionEngine::new()),
            last_applied_subtitle: Mutex::default(),
            upstream_selection: Mutex::default(),
            last_upstream_ids: Mutex::default(),
            intended_timeline: Mutex::new((1.0, gst::ClockTime::ZERO)),
            db3_pad_request: Mutex::default(),
            deadlines: Mutex::default(),
            prepared: Mutex::default(),
            swap_gate: SwapGate::default(),
            active_group: Mutex::default(),
            retired_group: Mutex::default(),
            passing_eos_group: Mutex::default(),
            input_eos_sids: Mutex::default(),
            held_activation: Mutex::default(),
            video_park_probe: Mutex::default(),
            video_chain_membership: Mutex::default(),
            level_probes: Mutex::default(),
            // Nothing walked yet, so the first `buffered_ahead` walks.
            level_probes_dirty: AtomicBool::new(true),
        });

        // The graph edges that retire `buffered_ahead`'s probe list (see
        // `LevelProbes`). DEEP, because the elements that hold the levels are
        // urisourcebin's and decodebin3's, added and removed inside their own
        // bins all load long. Both handlers run on whatever thread changed the
        // graph, streaming threads included, so each one only stores a bit; the
        // walk belongs to the polling caller.
        {
            let weak = Arc::downgrade(&inner);
            inner
                .pipeline
                .connect_deep_element_added(move |_bin, _sub_bin, _element| {
                    if let Some(inner) = weak.upgrade() {
                        inner.invalidate_level_probes();
                    }
                });
            let weak = Arc::downgrade(&inner);
            inner
                .pipeline
                .connect_deep_element_removed(move |_bin, _sub_bin, _element| {
                    if let Some(inner) = weak.upgrade() {
                        inner.invalidate_level_probes();
                    }
                });
        }

        Inner::install_core(&inner)?;

        // The sink-boundary release for a held gapless activation (see
        // `Inner::held_activation`). A STREAM_START leaving the decoupling
        // queue means the item behind it has drained and the NEXT item's audio
        // is now reaching the sink: exactly one STREAM_START crosses per audio
        // item, and a hold is only ever armed at a gapless boundary, so the
        // first STREAM_START after an arm is that boundary. No group-id
        // bookkeeping needed (and none possible: streamsynchronizer may rewrite
        // group ids downstream). The initial load's STREAM_START finds no hold
        // and is a no-op.
        if let Some(src) = inner.audio_entry.static_pad("src") {
            let weak = Arc::downgrade(&inner);
            src.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && matches!(event.view(), gst::EventView::StreamStart(_))
                    && let Some(inner) = weak.upgrade()
                {
                    inner.release_held_activation();
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Trigger #2 for the text running-time alignment (see
        // `Inner::sync_text_running_time`): a SEGMENT reaching the VIDEO SINK
        // is the ONLY event that changes what the alignment should
        // be, and it is the only one a REUSED text slot produces at a gapless
        // boundary (nothing re-links, so `poll_text_policy` is never re-entered,
        // and receiver-core's `async_done` never fires). Over a full run this
        // sees exactly two: the load's (base 0, a no-op) and the swap's.
        //
        // The pad moved one element down with the overlay's deletion and the
        // event did not: a SEGMENT crossing the deleted
        // overlay's `video_sink` reached this pad in the same push.
        //
        // The probe runs on the VIDEO streaming thread and therefore takes NO
        // lock: it only posts the job, and the worker does the work.
        if let Some(sink) = inner.video_sink.static_pad("sink") {
            let weak = Arc::downgrade(&inner);
            sink.add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && matches!(event.view(), gst::EventView::Segment(_))
                    && let Some(inner) = weak.upgrade()
                {
                    inner.queue_job(Job::SyncTextRunningTime);
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Volume notifies become events. The dedicated element makes them
        // deterministic (see `set_volume`).
        inner.volume.connect_notify(Some("volume"), {
            let weak = Arc::downgrade(&inner);
            move |volume, _pspec| {
                if let Some(inner) = weak.upgrade() {
                    inner.emit(PlaybinEvent::VolumeChanged(
                        volume.property::<f64>("volume"),
                    ));
                }
            }
        });

        // The worker holds only a Weak: it never keeps the pipeline alive,
        // and it exits when the last handle drops (the channel closes).
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("fcastplaybin".to_owned())
            .spawn(move || Inner::worker_loop(weak, work_rx))
            .context("spawning the fcastplaybin worker")?;

        // The three hands lanes (see the `hands` module): the SELECT_STREAMS
        // sender (`FcastPlaybin::select_streams`), the replay-seek sender
        // (`ReplayJob`) and the chain joiner (`ChainJoinJob`). One body for all
        // three, so the lane is data. Same Weak/channel lifetime as the worker,
        // and the thread names come from `Lane::thread_name`: they are
        // load-bearing strings (`Inner::drop` gates its inline descent on
        // `thread::current().name()`, and the teardown and two tests read them).
        for (lane, rx) in [
            (Lane::Select, select_rx),
            (Lane::Replay, replay_rx),
            (Lane::Join, join_rx),
        ] {
            let weak = Arc::downgrade(&inner);
            std::thread::Builder::new()
                .name(lane.thread_name().to_owned())
                .spawn(move || Inner::lane_loop(weak, lane, rx))
                .with_context(|| {
                    format!("spawning the fcastplaybin {} lane", lane.thread_name())
                })?;
        }

        // The periodic tick (see `Inner::run_tick`). One permanent thread that
        // replaces a one-shot sleeper thread per armed timer, so in steady
        // churn the crate spawns strictly fewer.
        let weak = Arc::downgrade(&inner);
        std::thread::Builder::new()
            .name("fpb-tick".to_owned())
            .spawn(move || Inner::tick_loop(weak, tick_rx))
            .context("spawning the fcastplaybin tick")?;

        Ok(Self { inner })
    }

    /// The bus, for callers that self-serve their messages (the spike).
    /// Unusable once [`set_event_handler`](Self::set_event_handler) installs
    /// its sync handler: every message is consumed there.
    pub fn bus(&self) -> gst::Bus {
        self.inner
            .pipeline
            .bus()
            .expect("pipeline always has a bus")
    }

    pub fn pipeline(&self) -> &gst::Pipeline {
        &self.inner.pipeline
    }

    /// Load a new media input, replacing the previous one (and any attached
    /// external subtitles). The pipeline ends in READY with the new input
    /// wired. Call [`Self::play`]/[`Self::pause`] to start. The returned
    /// outcome carries the load's generation (see
    /// [`load_async`](Self::load_async)).
    pub fn load(&self, input: MediaInput, start: StartPoint) -> Result<StartOutcome> {
        // This load bypasses the queue entirely, so anything already in it was
        // formed for the item about to be replaced.
        self.inner.supersede_queued_work();
        let generation = self.inner.allocate_generation();
        self.load_with_generation(input, start, generation)
    }

    pub(crate) fn load_with_generation(
        &self,
        input: MediaInput,
        start: StartPoint,
        generation: u64,
    ) -> Result<StartOutcome> {
        // Sanitized HERE, the one entry every load funnels through, so the
        // tainted rate can neither panic the start seek nor get recorded in
        // `intended_timeline` (where refresh seeks would replay it later).
        let start = match start {
            StartPoint::Seek { position, rate } => StartPoint::Seek {
                position,
                rate: decisions::sanitize_start_rate(rate),
            },
            live => live,
        };
        let inner = &self.inner;
        {
            // No routes during the reset (see `Inner::route_gate`).
            let _gate = Inner::gate(inner);
            // A pending prepared next input is superseded by this load; its
            // element is in `inputs` and leaves with the rest. Wake any of
            // its threads parked on the swap gate BEFORE the state change,
            // which joins streaming threads.
            inner.swap_gate.abort();
            *inner.prepared.lock() = None;
            inner
                .pipeline
                .set_state(gst::State::Ready)
                .context("pipeline to READY for load")?;
            Inner::remove_all_inputs(inner);

            // Fresh dynamic core per load (see `Core`).
            Inner::teardown_core(inner);
            Inner::install_core(inner)?;

            // Drop the previous load's audio sink at this quiescent point
            // (pipeline at READY, under the gate) so the next audio route
            // builds a fresh one (see `Inner::audio`).
            inner.remove_audio_sink();

            // The video chain leaves the pipeline between items. Routing
            // re-adds it iff the item has video (see `Inner::video_chain`).
            inner.remove_video_chain();
        }

        // Everything after this point belongs to the new load: events emitted
        // earlier (teardown stragglers) still carry the previous generation.
        inner.generation.store(generation, Ordering::SeqCst);
        // The previous item ends here. THE list of what that clears is
        // `Inner::reset_item_state`, shared with the stop so the two cannot
        // drift apart; the generation store above stays ahead of it.
        inner.reset_item_state();

        let element = match input {
            MediaInput::Uri(uri) => Inner::make_urisourcebin(&uri, true)?,
            MediaInput::Element(element) => element,
        };
        Inner::add_input(inner, element, generation, None)?;

        // Drive to PAUSED here to (a) detect a live source and (b) apply the
        // start position/rate seek while still PAUSED. The caller then just
        // plays and the first audio out is already at the target rate, so
        // there is no 1.0x-to-Nx seam.
        let change = inner
            .pipeline
            .set_state(gst::State::Paused)
            .context("pipeline to PAUSED for load")?;
        if change == gst::StateChangeSuccess::NoPreroll {
            return Ok(StartOutcome {
                live: true,
                generation,
            });
        }

        // A plain load (start-of-stream at 1.0x) needs no seek, so only a
        // real position/rate start pays the preroll wait.
        if let StartPoint::Seek { position, rate } = start
            && (rate != 1.0 || position != gst::ClockTime::ZERO)
        {
            // Recorded BEFORE the seek: an external subtitle attached during
            // this load can join (and replay) at any point of the start
            // dance, and the replay must aim at the timeline this item is
            // heading for even when the video sink has no segment yet.
            *inner.intended_timeline.lock() = (rate, position);
            Self::apply_start_seek(inner, position, rate);
        }
        Ok(StartOutcome {
            live: false,
            generation,
        })
    }

    /// Apply the start position/rate as a single flushing seek in PAUSED.
    /// Waits for preroll before seeking and for the flush's re-preroll after,
    /// both bounded so a stalled source degrades to "played at 1.0x" instead
    /// of a wedged worker. A non-seekable source is left as-is.
    ///
    /// The start seek is delivered at the INPUT, not through the pipeline,
    /// because at this point in a load the output graph is still being built.
    /// A `pipeline.seek()` is an upstream event, so `GstBin` hands it to the
    /// sink children in the pipeline AT THAT INSTANT and to nothing else. The
    /// load's chains join one at a time (a chain per routed stream) and the
    /// preroll token retires on the FIRST of them, so the pipeline reports a
    /// finished preroll while later streams are still being exposed; with
    /// `urisourcebin parse-streams=true` every elementary stream is its own
    /// decodebin3 input, so a seek that travels up one branch never reaches
    /// the others. A video chain joining after such a seek renders the item
    /// from the PRE-SEEK segment, and since `Inner::video_timeline` reads
    /// that very pad, everything aligned off it (external subtitles above
    /// all) is shifted by the start offset for the item's whole length.
    /// Measured under a parallel-suite soak, that lost 4/20 rounds; the
    /// failing ones logged `routed=[Audio] video_chain_in_pipeline=false`.
    ///
    /// `Inner::seek_main_input` has no such dependency on the output side, so
    /// that is the primary delivery and the pipeline broadcast is only the
    /// fallback for an input with no source pads to seek.
    fn apply_start_seek(inner: &Arc<Inner>, position: gst::ClockTime, rate: f64) {
        let (res, _, _) = inner.pipeline.state(PREROLL_TIMEOUT);
        if res.is_err() {
            return;
        }
        let mut q = gst::query::Seeking::new(gst::Format::Time);
        if !inner.pipeline.query(q.query_mut()) || !q.result().0 {
            return;
        }
        // One lock at a time: nothing else pairs these two, so do not be the
        // first to establish an order between them.
        let routed: Vec<StreamKind> = inner.routing.lock().routed.iter().map(|r| r.kind).collect();
        let audio_sink = inner.audio_sink.lock().is_some();
        debug!(
            ?routed,
            video_chain_in_pipeline = inner.video_sink.parent().is_some(),
            audio_sink,
            "start seek: chains present at seek time"
        );
        let event = rate_seek_event(rate, position, None);
        let sought = inner.seek_main_input(&event)
            || send_rate_seek(&inner.pipeline, rate, position).is_ok();
        if sought {
            // An external subtitle that joined during the load sits on the
            // pre-seek timeline: the seek travels the MAIN input only, so
            // align the side inputs exactly like a user seek does.
            inner.forward_seek_to_live_externals(rate, position);
            let _ = inner.pipeline.state(PREROLL_TIMEOUT);
        }
        debug!(
            origin = ?inner.video_timeline().1,
            sought,
            "start seek: the video timeline after the seek"
        );
    }

    pub fn play(&self) -> Result<()> {
        self.inner
            .pipeline
            .set_state(gst::State::Playing)
            .context("pipeline to PLAYING")?;
        Ok(())
    }

    pub fn pause(&self) -> Result<()> {
        self.inner
            .pipeline
            .set_state(gst::State::Paused)
            .context("pipeline to PAUSED")?;
        Ok(())
    }

    pub fn stop(&self) -> Result<()> {
        self.inner.supersede_queued_work();
        self.teardown(gst::State::Null)
    }

    /// Change the pipeline state. Callers driving the pipeline through a
    /// caller-owned handle must use this instead of `set_state` on the
    /// pipeline element: DOWNWARD transitions take the route gate so no
    /// stream gets routed (and no chain activated) into the descending
    /// pipeline (see `Inner::route_gate`).
    pub fn set_pipeline_state(
        &self,
        state: gst::State,
    ) -> std::result::Result<gst::StateChangeSuccess, gst::StateChangeError> {
        let _gate = (state < gst::State::Paused).then(|| Inner::gate(&self.inner));
        self.inner.pipeline.set_state(state)
    }

    // Worker-thread entry points: queued, ordered, safe from any thread
    // (including the event callback and async executors).

    /// Set the volume (clamped to `0.0..=1.0`). Confirmation arrives as
    /// [`PlaybinEvent::VolumeChanged`]. Setting the current value again emits
    /// no notify. The skip is this crate's, not GObject's (a plain
    /// `set_property` notifies unconditionally, measured by
    /// `volume_idempotent_set_emits_no_event`); callers whose protocol needs
    /// a confirmation anyway use [`renotify_volume`](Self::renotify_volume).
    ///
    /// Volume lives on a dedicated `volume` element, NOT the audio sink: the
    /// sink is rebuilt per load, many resolved sinks expose no volume
    /// property at all, and sink-proxied volume notifies
    /// non-deterministically. playsink ships a dedicated volume element for
    /// the same reasons.
    pub fn set_volume(&self, volume: f64) {
        let target = volume.clamp(0.0, 1.0);
        let current: f64 = self.inner.volume.property("volume");
        if current == target {
            return;
        }
        self.inner.volume.set_property("volume", target);
    }

    /// The current volume (`0.0..=1.0`).
    pub fn volume(&self) -> f64 {
        self.inner.volume.property("volume")
    }

    /// Re-emit [`PlaybinEvent::VolumeChanged`] at the current value, for
    /// callers whose protocol expects a confirmation even for an idempotent
    /// set.
    pub fn renotify_volume(&self) {
        self.inner.volume.notify("volume");
    }

    /// The current item's own stream time, asked of the OUTPUT sink.
    ///
    /// A correct gapless swap resets the segment, so the sink already reports
    /// the current item's own stream time (no cross-item rebase needed). This
    /// is the sink-anchored source of truth: the user-facing item switch
    /// (title/duration) is held until the new item's audio reaches the sink
    /// (see `Inner::held_activation`), so it lands in step with this position
    /// flipping to the new item's 0-based time, not a decoupling-queue ahead
    /// of it.
    ///
    /// Asking the PIPELINE instead would break that: `GstBin` folds POSITION
    /// with MAX over every SINK-flagged child, so an unsynced text parking
    /// sink racing toward the item's end can dominate the answer, and a `-1`
    /// DURATION from any folded sink poisons `duration()` outright.
    pub fn position(&self) -> Option<gst::ClockTime> {
        self.query_timeline(|element| element.query_position::<gst::ClockTime>())
    }

    /// The current item's duration, asked of the same sink `position()` is
    /// anchored on (so the two answers describe one item).
    pub fn duration(&self) -> Option<gst::ClockTime> {
        self.query_timeline(|element| element.query_duration::<gst::ClockTime>())
    }

    /// Run a timeline query against the authoritative element: the per-load
    /// audio sink (the pipeline clock and the held-activation anchor), else
    /// the video sink, else the pipeline as a whole. One helper so
    /// `position()` and `duration()` cannot drift onto different items.
    ///
    /// A candidate outside the pipeline is skipped (`ensure_audio_sink` has
    /// not built one yet, or `remove_video_chain` took the chain out for an
    /// audio-only item), as is one that cannot answer.
    fn query_timeline(
        &self,
        query: impl Fn(&gst::Element) -> Option<gst::ClockTime>,
    ) -> Option<gst::ClockTime> {
        // Cloned out first: a query takes element and pad locks and can block
        // behind a streaming thread, and holding `audio_sink` across that
        // would stall every load's sink teardown.
        let audio_sink = self.inner.audio_sink.lock().clone();
        let video_sink = Some(self.inner.video_sink.clone());
        for candidate in [audio_sink, video_sink].into_iter().flatten() {
            if candidate.parent().is_none() {
                continue;
            }
            if let Some(value) = query(&candidate) {
                return Some(value);
            }
        }
        query(self.inner.pipeline.upcast_ref::<gst::Element>())
    }

    /// Whether the pipeline is settled: the last state change succeeded and
    /// no transition is pending (non-blocking query). NOT the complement of
    /// [`has_async_transition`](Self::has_async_transition), since a FAILED
    /// last change is neither settled nor async.
    pub fn is_settled(&self) -> bool {
        let (res, _, pending) = self.inner.pipeline.state(gst::ClockTime::ZERO);
        res.is_ok() && pending == gst::State::VoidPending
    }

    /// Whether an async state change (re-preroll, a flushing seek's preroll)
    /// is in progress (non-blocking query). Asking the pipeline beats
    /// predicting from the kind of operation: mispredictions are what used
    /// to wedge callers' serialization logic.
    pub fn has_async_transition(&self) -> bool {
        let (res, _, pending) = self.inner.pipeline.state(gst::ClockTime::ZERO);
        matches!(res, Ok(gst::StateChangeSuccess::Async)) || pending != gst::State::VoidPending
    }

    pub fn seek(&self, position: gst::ClockTime) -> Result<()> {
        self.inner
            .pipeline
            .seek_simple(gst::SeekFlags::FLUSH, position)
            .context("seek")?;
        Ok(())
    }

    pub fn dump_dot(&self, name: &str) {
        self.inner
            .pipeline
            .debug_to_dot_file_with_ts(gst::DebugGraphDetails::ALL, name);
    }

    /// Cumulative parsed-byte counters for every live input stream (all
    /// streams, selected or not: decodebin3 keeps consuming deselected
    /// inputs). Poll and diff to plot per-stream bitrate, and correlate
    /// with the stream collection via `stream_id` and with track selection
    /// via the caller's selected ids.
    pub fn stream_io_stats(&self) -> Vec<StreamIoStats> {
        let routing = self.inner.routing.lock();
        routing
            .inputs
            .iter()
            .flat_map(|input| {
                input.taps.iter().map(|tap| StreamIoStats {
                    stream_id: tap.pad.stream_id().map(|sid| sid.to_string()),
                    external: input.external.as_ref().map(|e| e.id),
                    bytes: tap.bytes.load(Ordering::Relaxed),
                    caps: tap.pad.current_caps(),
                })
            })
            .collect()
    }

    /// Inspector: every live input's element factory, its `uri` property
    /// when the element has one, and whether it is an external subtitle
    /// input.
    pub fn source_summaries(&self) -> Vec<SourceDbg> {
        self.inner
            .routing
            .lock()
            .inputs
            .iter()
            .map(|input| {
                let factory = input.element.factory().map(|f| f.name().to_string());
                SourceDbg {
                    external: input.external.as_ref().map(|e| e.id),
                    // A directly-constructed wrapper bin's factory is just
                    // "bin", its NAME carries the source kind
                    // (fcast-whep-source, fcast-fwebrtc-source, ...).
                    factory: match factory.as_deref() {
                        None | Some("bin") => input.element.name().to_string(),
                        Some(name) => name.to_string(),
                    },
                    uri: input
                        .element
                        .find_property("uri")
                        .and_then(|_| input.element.property::<Option<String>>("uri")),
                }
            })
            .collect()
    }

    /// Diagnostic: the pipeline's current + pending state (a stalled load
    /// sits with an unfinished async transition, `pending != VoidPending`).
    pub fn state_summary(&self) -> (gst::State, gst::State) {
        let (_, current, pending) = self.inner.pipeline.state(gst::ClockTime::ZERO);
        (current, pending)
    }

    /// The recursive element walk the state diagnostics share. `line` formats
    /// the elements it wants and answers None for the ones to skip.
    fn walk_element_states(
        &self,
        line: impl Fn(
            &gst::Element,
            Result<gst::StateChangeSuccess, gst::StateChangeError>,
            gst::State,
            gst::State,
        ) -> Option<String>,
    ) -> Vec<String> {
        let mut out = Vec::new();
        let mut iter = self.inner.pipeline.iterate_recurse();
        while let Ok(Some(elem)) = iter.next() {
            let (ret, cur, pend) = elem.state(gst::ClockTime::ZERO);
            if let Some(text) = line(&elem, ret, cur, pend) {
                out.push(text);
            }
        }
        out
    }

    /// Diagnostic: every pipeline element's `name (current -> pending)`, to
    /// spot which element is stuck below the pipeline's target at a stall.
    pub fn element_states(&self) -> Vec<String> {
        self.walk_element_states(|elem, ret, cur, pend| {
            Some(format!("{}({:?}->{:?} {:?})", elem.name(), cur, pend, ret))
        })
    }

    /// Diagnostic: elements with an unfinished state transition (`pending !=
    /// VoidPending`). Normally empty, the interesting subset of
    /// [`element_states`](Self::element_states) at inspector poll rates.
    pub fn unsettled_elements(&self) -> Vec<String> {
        self.walk_element_states(|elem, _, cur, pend| {
            (pend != gst::State::VoidPending).then(|| format!("{}({cur:?}->{pend:?})", elem.name()))
        })
    }

    /// The caller video sink's base-sink `stats` structure (rendered/dropped
    /// buffer counts), when a video sink is configured. `stats` is a
    /// `GstBaseSink` property, so it is absent on a bin sink (autovideosink);
    /// `None` then rather than panicking.
    pub fn video_sink_stats(&self) -> Option<gst::Structure> {
        let sink = &self.inner.video_sink;
        sink.find_property("stats")
            .map(|_| sink.property::<gst::Structure>("stats"))
    }

    /// The caller's video sink, for tests that need the pad every displayed
    /// frame crosses (the crate's video-timeline anchor now
    /// deleted subtitleoverlay, which used to be that pad's owner). Not part
    /// of the public API.
    #[doc(hidden)]
    pub fn video_sink(&self) -> gst::Element {
        self.inner.video_sink.clone()
    }

    /// The per-load audio sink's negotiated caps and base-sink `stats`
    /// structure, while one exists. `stats` is a `GstBaseSink` property, so it
    /// is `None` on a bin sink (autoaudiosink) that lacks it.
    pub fn audio_sink_health(&self) -> Option<(Option<gst::Caps>, Option<gst::Structure>)> {
        let slot = self.inner.audio_sink.lock();
        let sink = slot.as_ref()?;
        let caps = sink.static_pad("sink").and_then(|pad| pad.current_caps());
        let stats = sink
            .find_property("stats")
            .map(|_| sink.property::<gst::Structure>("stats"));
        Some((caps, stats))
    }

    /// Re-assert the pipeline's in-flight target after a prepared input's
    /// failure may have aborted an async commit. Worker-thread only.
    pub(crate) fn recommit_pipeline_state(&self) {
        let current = self.inner.pipeline.current_state();
        let pending = self.inner.pipeline.pending_state();
        let target = if pending != gst::State::VoidPending {
            pending
        } else {
            current
        };
        debug!(
            ?current,
            ?pending,
            ?target,
            "re-committing the pipeline state"
        );
        if target > gst::State::Ready {
            let _ = self.inner.pipeline.set_state(target);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn init() {
        static ONCE: std::sync::Once = std::sync::Once::new();
        ONCE.call_once(|| gst::init().unwrap());
    }

    fn seek_fields(
        event: &gst::Event,
    ) -> (
        f64,
        gst::SeekFlags,
        gst::SeekType,
        Option<gst::ClockTime>,
        gst::SeekType,
        Option<gst::ClockTime>,
    ) {
        let gst::EventView::Seek(seek) = event.view() else {
            panic!("rate_seek_event built something other than a seek");
        };
        let (rate, flags, start_type, start, stop_type, stop) = seek.get();
        let time = |value: gst::GenericFormattedValue| match value {
            gst::GenericFormattedValue::Time(t) => t,
            other => panic!("a non-time bound in the seek: {other:?}"),
        };
        (rate, flags, start_type, time(start), stop_type, time(stop))
    }

    /// A forward rate anchors the START at the position and leaves the stop
    /// open, and the caller's seqnum stamps the event.
    #[test]
    fn a_forward_rate_seek_is_set_anchored_at_the_position() {
        init();
        let seqnum = gst::Seqnum::next();
        let position = gst::ClockTime::from_seconds(5);
        let event = rate_seek_event(1.5, position, Some(seqnum));
        assert_eq!(event.seqnum(), seqnum);
        let (rate, flags, start_type, start, stop_type, stop) = seek_fields(&event);
        assert_eq!(rate, 1.5);
        assert_eq!(flags, gst::SeekFlags::FLUSH);
        assert_eq!(start_type, gst::SeekType::Set);
        assert_eq!(start, Some(position));
        assert_eq!(stop_type, gst::SeekType::None);
        assert_eq!(stop, gst::ClockTime::NONE);
    }

    /// A reverse rate builds the End-anchored pair: play [0, position]
    /// backwards, with the stop naming the position and trickmode on.
    #[test]
    fn a_reverse_rate_seek_is_the_end_anchored_pair() {
        init();
        let position = gst::ClockTime::from_seconds(7);
        let event = rate_seek_event(-1.0, position, None);
        let (rate, flags, start_type, start, stop_type, stop) = seek_fields(&event);
        assert_eq!(rate, -1.0);
        assert_eq!(flags, gst::SeekFlags::FLUSH | gst::SeekFlags::TRICKMODE);
        assert_eq!(start_type, gst::SeekType::Set);
        assert_eq!(start, Some(gst::ClockTime::ZERO));
        assert_eq!(stop_type, gst::SeekType::End);
        assert_eq!(stop, Some(position));
    }
}
