//! The subtitle consumer transport: the per-stream `appsink` tail, the
//! sample-to-item conversion and the feed to the caller's consumer.

use std::sync::Arc;

use gst::prelude::*;
use tracing::{debug, error, warn};

use crate::{
    FcastPlaybin, Inner,
    api::{PlaybinEvent, SubtitleFeedItem, SubtitleTextFormat},
    decisions,
    flush::FlushReason,
    levers::{BitmapSubsEnabled, cue_ir_enabled},
    routing::{RoutedStream, StreamKind},
};

/// The consumer tail's queue depth. Bounded WITH `drop=true`: a consumer that
/// stops draining loses cues rather than stalling the text branch, which is
/// the property that lets pair D retire. Deep enough that real cue cadence
/// never reaches it (subtitles arrive seconds apart and this is 32 of them) so
/// a nonzero drop count means a genuinely stuck consumer, not a busy one.
const TEXT_CONSUMER_MAX_BUFFERS: u32 = 32;

/// Where one subtitle unit sits on the running-time line once its segment has
/// clipped it. See [`Inner::clipped_running_time`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ClippedTime {
    /// The unit's start, clamped forward to the segment when the segment began
    /// inside it.
    start_rt: gst::ClockTime,
    /// The unit's real end, or `None` for an open-ended unit.
    end_rt: Option<gst::ClockTime>,
    /// How much of the head the clip removed, in STREAM time, which is what a
    /// duration-carrying unit has to give back so it still names the time it
    /// has left.
    trimmed: gst::ClockTime,
}

impl Inner {
    /// Hand one item to the installed subtitle consumer (see
    /// [`FcastPlaybin::set_subtitle_consumer`]). A no-op until one is
    /// installed, and on the default arm nothing ever calls this.
    ///
    /// THE single funnel: every cue and every `Clear` in the crate goes
    /// through here, which is what makes the two guarantees below cheap to
    /// state and impossible to bypass.
    ///
    /// The `Arc` is CLONED OUT and the crate lock RELEASED before the callback
    /// runs. Not "no lock is taken" (one is, briefly, to read the slot) but
    /// no crate lock is held ACROSS foreign code, which is the property that
    /// matters on a streaming thread.
    ///
    /// A PANICKING consumer is caught. Cue delivery runs inside the appsink's
    /// `new_sample`/`new_preroll`, whose contract with this crate is that they
    /// always answer `FlowSuccess::Ok`; an unwind through the C frames between
    /// them and here would answer nothing at all and abort the process. The
    /// consumer must not panic (documented on `set_subtitle_consumer`); this
    /// makes the promise true rather than merely asked for.
    pub(crate) fn feed_subtitle(&self, item: SubtitleFeedItem) {
        let consumer = self.subtitle_consumer.lock().clone();
        if let Some(consumer) = consumer {
            let caught = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| consumer(item)));
            if caught.is_err() {
                error!(
                    "the subtitle consumer panicked; the cue is dropped and delivery continues.                      A consumer must not panic (see FcastPlaybin::set_subtitle_consumer)"
                );
            }
        }
    }

    /// Tell the consumer that everything it holds is stale.
    ///
    /// Sent from the driver side of the [`SubtitleFeedItem::Clear`] protocol:
    /// a consumer branch's disposal, a load/stop supersession, a switch to
    /// subtitles-off. The transport's own pad probe covers the flush and
    /// stream-restart cases the driver never sees.
    pub(crate) fn send_subtitle_clear(&self) {
        self.feed_subtitle(SubtitleFeedItem::Clear);
    }

    /// The memo key for a text stream whose branch could not be wired: its
    /// stream id where it has one, and its decodebin3 pad otherwise.
    ///
    /// A pad with no stream id has nothing to REPORT (the event names a
    /// stream), but the retry still has to stop, and the pad it hangs off is
    /// as stable a name as exists for it within one load.
    pub(crate) fn unwirable_key(sid: Option<&str>, pad: &gst::Pad) -> String {
        match sid {
            Some(sid) => sid.to_string(),
            None => format!("pad:{}", pad.name()),
        }
    }

    /// Report a selected subtitle stream whose caps the consumer arm cannot
    /// carry, at most once per (sid, generation). See
    /// [`PlaybinEvent::SubtitleTrackUnsupported`].
    pub(crate) fn report_unsupported_subtitle(&self, sid: &str, caps: &gst::Caps) {
        let generation = self.current_generation();
        let first = self
            .unsupported_text_reported
            .lock()
            .insert((sid.to_string(), generation));
        if !first {
            return;
        }
        warn!(
            sid,
            caps = %caps,
            "the selected subtitle stream carries caps the cue renderer cannot render; \
             the branch stays parked and nothing will be shown"
        );
        self.emit(PlaybinEvent::SubtitleTrackUnsupported {
            sid: sid.to_string(),
            caps: caps.clone(),
        });
    }

    /// Wake every parked text push before a downward state change. Two
    /// kinds of thread sit parked HOLDING pad locks the state change
    /// needs to deactivate pads, wedging `set_state` forever: a live
    /// text branch parked inside its own tail, and a mid-push input
    /// inside its byte-limited decodebin3 slot. The flush pairs wake
    /// both.
    /// Wake the blocked push on every LIVE text branch, and drop what it has
    /// queued. Needed before a subtitle REPLACE is dispatched: the outgoing
    /// text slot's multiqueue src pad sits inside `gst_pad_push` into
    /// [`RoutedStream::tqueue`], which is a plain `queue` whose default
    /// `max-size-time` of 1s counts the DEAD AIR between sparse cues
    /// (`gst_queue_apply_gap` advances the time level off GAP events), so it
    /// reports itself full while holding ZERO buffers and ZERO bytes.
    /// decodebin3's stream switch cannot deactivate a slot whose pad is
    /// mid-push, so the switch waits out the outgoing track's cue cadence:
    /// measured 1.6s at a 2s cue period and 4.6s at 4s, with essentially the
    /// whole latency in that one push. The flush also discards the outgoing
    /// backlog, so the new track's first cue renders instead of queueing behind
    /// seconds of the old one.
    /// Put every routed text branch back on the A/V branches' RUNNING-TIME
    /// timeline.
    ///
    /// Text deliberately BYPASSES streamsynchronizer (see [`RoutedStream`]), so
    /// it never receives the per-GROUP base streamsynchronizer stamps onto the
    /// A/V segments when a gapless swap moves to the next item. Measured at
    /// subtitleoverlay's own input pads, one swap into an 8s item:
    ///
    /// ```text
    /// video_sink    SEGMENT start=0 base=0:00:08.189219955  rt(pts 0.8s)=8.989s
    /// subtitle_sink SEGMENT start=0 base=0:00:00.000000000  rt(pts 0.8s)=0.800s
    /// ```
    ///
    /// subtitleoverlay composites by running time, so every cue of the new item
    /// lands ~8s in the past and NOTHING renders for the rest of the item: the
    /// selection confirms and the branch links, it is just dead. A pad offset
    /// on the decodebin3 text pad re-pushes its sticky segment with the
    /// missing base; the same run that rendered 0 cue-bearing buffers
    /// rendered 32 after.
    ///
    /// # The offset is the ORIGIN difference, not the base difference
    ///
    /// Running time is `rt(t) = (t - start)/|rate| + base`, so the pad offset
    /// that puts the text branch on the video's line is what the VIDEO segment
    /// says about the TEXT segment's own start, minus the base the text
    /// segment already carries for it:
    ///
    /// ```text
    /// offset = rt_video(start_text) - rt_text(start_text)
    ///        = (start_text - start_video)/|rate_video| + (base_video - base_text)
    /// ```
    ///
    /// The first term used to be missing: the code applied `base_video -
    /// base_text` alone, which is the full answer exactly when the two
    /// segments carry the SAME start. Loads, flushing seeks and gapless swaps
    /// all keep that true (every seek this crate issues carries
    /// `SeekFlags::FLUSH`, whose FLUSH_STOP also wipes the pad stickies this
    /// reads, so a stale start cannot be read across one), and on those
    /// transitions both formulas agree: base 0 on both branches computes 0,
    /// and only the gapless swap's stamped video base computes the +8.189 the
    /// measurement above shows.
    ///
    /// An adaptive demuxer's MID-STREAM track add does not keep it true.
    /// `dashdemux2` restarts a re-selected track at its global output
    /// position and emits `start ≈ base ≈ that position`, a segment already
    /// ON the pipeline's running-time line (`rt(t) = t`, the same line a
    /// video segment of `start=0 base=0` draws). The base-only formula read
    /// that base as drift to subtract: measured at `offset=-22.066s` applied to
    /// the re-added `text_3`, and every cue (including the two the park
    /// replayed) expired ~22 s in the past while the branch was finally,
    /// correctly, joined. With both terms the re-add computes 0 and touches
    /// nothing, which is the right answer measured rather than special-cased:
    /// the predicate is the segments themselves, not "embedded vs external".
    ///
    /// An EXTERNAL's segments reach this same code and are also covered by
    /// the full formula: its replay seek re-issues the pipeline seek at the
    /// video's own origin, so its post-replay segment carries the video's
    /// start and base 0 and both terms vanish; an external whose timeline
    /// genuinely lags a sought video (start 0 against a video start of P)
    /// now computes the `-P` that actually aligns it, where the base-only
    /// formula computed 0 and left it to the replay to repair.
    ///
    /// Idempotent: `gst_pad_set_offset` applies the offset on the way OUT to
    /// the peer, so the pad's own sticky segment keeps the raw base and the
    /// computed value is stable across repeated calls. `video_timeline`'s
    /// `origin` cannot see this divergence: it folds base into stream time
    /// with a `saturating_sub`, which pins both sides to 0.
    ///
    /// Takes the routing lock, so it must NOT run on a streaming thread: the
    /// probe in [`FcastPlaybin::new`] posts [`Job::SyncTextRunningTime`] rather
    /// than calling this directly, for the reason [`Job::FinishActivation`]
    /// documents.
    pub(crate) fn sync_text_running_time(&self) {
        // Everything `rt = (t - start)/|rate| + base` needs, in signed ns.
        let segment_of = |pad: &gst::Pad| -> Option<(i64, i64, f64)> {
            let event = pad.sticky_event::<gst::event::Segment>(0)?;
            let segment = event.segment().downcast_ref::<gst::ClockTime>()?;
            Some((
                segment.start().unwrap_or(gst::ClockTime::ZERO).nseconds() as i64,
                segment.base().unwrap_or(gst::ClockTime::ZERO).nseconds() as i64,
                segment.rate().abs(),
            ))
        };
        let Some((video_start, video_base, video_rate)) = self
            .video_sink
            .static_pad("sink")
            .and_then(|pad| segment_of(&pad))
        else {
            return;
        };
        let routing = self.routing.lock();
        // A branch whose realigning replay seek is still out has its timeline
        // DEFINED by that seek's target (the video's origin), where offset 0 is
        // correct by construction. Its current sticky segment is the one the
        // replay is replacing, so compensating for it computes an offset that
        // is wrong the moment the seek lands, and the replay's own cues then
        // clip negative and are dropped for good. The reset in
        // `FcastPlaybin::replay_subtitle` closes the same window from the other
        // end, this closes the poll that would re-open it.
        // Lever: `FCAST_NO_REPLAY_OFFSET_RESET` (the same one, so the pair is
        // one A/B).
        let awaiting: Vec<String> = if std::env::var_os("FCAST_NO_REPLAY_OFFSET_RESET").is_none() {
            Self::sids_awaiting_replay(&routing)
        } else {
            Vec::new()
        };
        for routed in routing.routed.iter().filter(|r| r.kind == StreamKind::Text) {
            let replaying = !awaiting.is_empty()
                && routed
                    .db3_src_pad
                    .stream_id()
                    .is_some_and(|sid| awaiting.contains(&sid.to_string()));
            if replaying {
                if routed.db3_src_pad.offset() != 0 {
                    debug!(
                        pad = %routed.db3_src_pad.name(),
                        previous = routed.db3_src_pad.offset(),
                        "a replay owns this text branch's timeline; clearing its offset"
                    );
                    routed.db3_src_pad.set_offset(0);
                }
                continue;
            }
            let Some((text_start, text_base, _)) = segment_of(&routed.db3_src_pad) else {
                debug!(
                    pad = %routed.db3_src_pad.name(),
                    video_start,
                    video_base,
                    "the text branch has no segment yet, so it cannot be aligned"
                );
                continue;
            };
            // The full origin difference. `rt_text(start_text)` is `base_text`
            // by definition, so no term needs the text rate. The arithmetic
            // itself lives in [`decisions::text_pad_offset`], where every case
            // this doc enumerates is a unit test.
            let offset = decisions::text_pad_offset(
                video_start,
                video_base,
                video_rate,
                text_start,
                text_base,
            );
            if routed.db3_src_pad.offset() != offset {
                debug!(
                    pad = %routed.db3_src_pad.name(),
                    offset,
                    video_start,
                    video_base,
                    text_start,
                    text_base,
                    rate = video_rate,
                    previous = routed.db3_src_pad.offset(),
                    linked = routed.downstream.is_some(),
                    "aligning the text branch's running time with the A/V branches'"
                );
                routed.db3_src_pad.set_offset(offset);
            }
        }
    }

    /// The live text branches' downstream pads, collected so the caller can
    /// flush them with the routing lock RELEASED.
    ///
    /// A flush must never be sent while holding that lock. `send_event` runs
    /// the whole downstream event chain inline on the calling thread, and a
    /// `FLUSH_START` reaching a multiqueue sink pad makes it
    /// `gst_pad_pause_task` its src pad, which blocks on that pad's stream
    /// lock until the streaming task returns. That task is very often inside
    /// one of this crate's own pad probes, and those take the routing lock.
    /// Holding it here inverts the order and deadlocks the process. The
    /// worker holds routing and waits for the stream lock while the streaming
    /// thread holds the stream lock and waits for routing. Observed as a
    /// hard wedge of the whole test binary, with the worker parked in
    /// `gst_pad_pause_task` under `flush_parked_text_pushes` and a
    /// `multiqueue:src` task parked in `route_db3_pad`'s probe.
    pub(crate) fn live_text_downstream_pads(&self) -> Vec<gst::Pad> {
        let routing = self.routing.lock();
        routing
            .routed
            .iter()
            .filter(|routed| routed.kind == StreamKind::Text)
            .filter_map(|routed| routed.downstream.clone())
            .collect()
    }

    /// WHICH STREAM IS LIVE, observed: the stream id of the text branch
    /// currently feeding the consumer, or `None` if none is.
    ///
    /// The observed twin of the `last_applied_subtitle` mirror. That field is
    /// a remembered write, and a remembered write is wrong exactly when it
    /// matters: after a gapless activation clears it, after a re-attach reuses
    /// a stream id, after any path that linked a branch without going through
    /// a confirmation. This asks the graph the same question the mirror was
    /// standing in for.
    ///
    /// Thread discipline, verbatim from `probe_routed_selection`: read-only
    /// and decider-only, takes ROUTING ALONE, and does nothing but sticky
    /// reads. It follows the same chain `verify_replay` walks - the routed
    /// Text entry whose queue feeds the seat - and reads that entry's
    /// decodebin3 pad StreamStart.
    /// Whether this routed Text entry's consumer branch is LIVE, asked of the
    /// graph rather than of the crate's bookkeeping.
    ///
    /// `appsink`/`downstream` being `Some` are remembered writes: they say a
    /// branch was built, not that it is still wired. The two reads that use
    /// this ([`Inner::observed_seat_occupant`],
    /// [`Inner::text_tail_segment`]) exist precisely to ask the graph what the
    /// mirrors were standing in for, and the divergence warning they feed has
    /// no power if they read a mirror themselves. So the fields locate the
    /// pads and the PADS answer the question: decodebin3's src pad is linked
    /// (the branch's head is attached) and the appsink's sink pad is linked
    /// (its tail is). A detach unlinks both before the entry is cleared, so a
    /// half-torn-down branch reads dead here even while its fields still hold
    /// handles.
    pub(crate) fn consumer_branch_is_live(routed: &RoutedStream) -> bool {
        routed.downstream.is_some()
            && routed.db3_src_pad.is_linked()
            && routed
                .appsink
                .as_ref()
                .and_then(|appsink| appsink.static_pad("sink"))
                .is_some_and(|pad| pad.is_linked())
    }

    /// # What the answer means
    ///
    /// There is no seat: every live text branch ends in its own appsink, and
    /// the driver links exactly one of them by construction
    /// (`poll_text_policy` links only the `subtitle_sid()` stream). So this
    /// reads the routed Text entry that HAS a live consumer tail and returns
    /// its StreamStart sid.
    ///
    /// THE QUESTION CHANGED with the transport, and the callers' use of the
    /// answer survived it: "which stream occupies the renderer's one seat"
    /// became "which stream is the driver's designated consumer branch". It is
    /// still derived from the graph (a link plus a sticky) rather than from a
    /// remembered write, which is the whole point of the function, and the
    /// mirror-vs-observed divergence check keeps its value. What it does not
    /// say is anything about what a renderer is actually showing. That state
    /// lives in the consumer, out of the driver's reach.
    pub(crate) fn observed_seat_occupant(&self) -> Option<String> {
        let routing = self.routing.lock();
        routing
            .routed
            .iter()
            .filter(|routed| routed.kind == StreamKind::Text)
            .find(|routed| Self::consumer_branch_is_live(routed))
            .and_then(|routed| {
                routed
                    .db3_src_pad
                    .sticky_event::<gst::event::StreamStart>(0)
                    .map(|event| event.stream_id().to_string())
            })
    }

    /// The sticky SEGMENT the live text branch's TAIL carries: the consumer
    /// branch's own appsink sink pad.
    fn text_tail_segment(&self) -> Option<gst::event::Segment<gst::Event>> {
        let routing = self.routing.lock();
        let entry = routing
            .routed
            .iter()
            .filter(|routed| routed.kind == StreamKind::Text)
            .find(|routed| Self::consumer_branch_is_live(routed))?;
        let pad = entry.appsink.as_ref()?.static_pad("sink")?;
        // Dropped before the sticky read for symmetry with every other
        // decider-side graph read: nothing below needs routing.
        drop(routing);
        pad.sticky_event::<gst::event::Segment>(0)
    }

    /// Whether the live text branch's TAIL carries the same running-time
    /// ORIGIN the video does. See [`Inner::text_tail_segment`], whose answer
    /// this reads.
    ///
    /// Delivery alone is not enough: an input that joined the branch WITHOUT a
    /// replay carries its own file-origin segment, and its cues render shifted
    /// whenever the video's origin moved (a started-at or sought item). Only
    /// aligned delivery needs no replay. Reads current stickies and current
    /// intent and remembers nothing, which is what lets both
    /// [`FcastPlaybin::verify_replay`] and the reconcile pass share it.
    pub(crate) fn subtitle_origin_matches_video(&self) -> bool {
        let (_, video_origin) = self.video_timeline();
        let text_origin = self.text_tail_segment().and_then(|event| {
            let segment = event.segment().downcast_ref::<gst::ClockTime>()?;
            Some(Self::segment_origin(segment))
        });
        text_origin == Some(video_origin)
    }

    /// `FCAST_NO_MISALIGNED_CUE_GATE` restores delivery of cues whose segment
    /// origin disagrees with the video's, at the consumer feed and at the
    /// park replay both (one rule, moved together).
    pub(crate) fn misaligned_cue_gate_off() -> bool {
        std::env::var_os("FCAST_NO_MISALIGNED_CUE_GATE").is_some()
    }

    /// The running-time ORIGIN of a segment: the stream position whose running
    /// time is zero, `start - base*|rate| - offset`.
    ///
    /// `offset` COUNTS. `gst_pad_set_offset` folds a pad offset into the
    /// segment through `gst_segment_offset_running_time`, which at base
    /// 0 lands the whole thing in `segment.offset` rather than in `base`
    /// (gstsegment.c). Reading start and base alone answered "aligned"
    /// for a branch whose every cue converted to a negative running
    /// time, blinding [`Self::subtitle_origin_matches_video`] to precisely the
    /// misalignment it exists to catch, so nothing re-asked and nothing
    /// repaired.
    pub(crate) fn segment_origin(
        segment: &gst::FormattedSegment<gst::ClockTime>,
    ) -> gst::ClockTime {
        let rate = segment.rate();
        let start = segment.start().unwrap_or(gst::ClockTime::ZERO);
        let base =
            (segment.base().unwrap_or(gst::ClockTime::ZERO).nseconds() as f64 * rate.abs()) as u64;
        let offset = segment.offset().unwrap_or(gst::ClockTime::ZERO).nseconds();
        gst::ClockTime::from_nseconds(start.nseconds().saturating_sub(base).saturating_sub(offset))
    }

    /// # A FLUSH_START-only variant here is WRONG, and was measured to be
    ///
    /// The teardown deadlock this flush sits in the middle of (see
    /// `tests/regression_teardown_flush.rs`) is caused by the flush PAIR. Its
    /// FLUSH_STOP re-arms the pad, and a source pushing as fast as it can
    /// re-blocks under the stream lock before the caller reaches its
    /// `set_state(Null)`. Sending FLUSH_START alone and leaving the pads
    /// flushing looks like the obvious answer, and it is not.
    ///
    /// `db3_sink_pads` covers EVERY stream of the input rather than just its
    /// text, so the unmatched FLUSH_START reaches audio and video too, and
    /// `teardown` also runs at READY for a stop-and-reload where the pipeline
    /// is reused afterwards. Measured on the `fuzz_scenarios` driver, seeds
    /// 500001, 500002 and 500010 pass with the pair and fail without it, on
    /// `flush_pairs_matched: flush-start never matched by a flush-stop` with
    /// entries still recorded after it. The wedge is real and stays open, but
    /// its fix has to leave the flush pairing intact.
    pub(crate) fn flush_parked_text_pushes(&self) {
        let pads = self.live_text_downstream_pads();
        let db3_sinks: Vec<gst::Pad> = {
            let routing = self.routing.lock();
            routing
                .inputs
                .iter()
                .flat_map(|input| input.db3_sink_pads.iter().cloned())
                .collect()
        };
        Self::flush_pads(&pads, FlushReason::TeardownText);
        Self::flush_db3_sink_pads(&db3_sinks);
    }

    /// Build the text branch's tail: `tqueue ! appsink`, with the
    /// appsink pulling cues out to the installed subtitle consumer.
    ///
    /// `None` is a failed build, and the caller unwinds the queue behind it.
    ///
    /// # Why this branch cannot block, which is pair D's death certificate
    ///
    /// `sync=false` so it never waits for a clock, `async=false` so it never
    /// gates a preroll, `drop=true` with `max-buffers` bounded so a slow
    /// consumer LOSES cues instead of stalling the branch, and
    /// `enable-last-sample=false` so no sample is retained. The `new_sample`
    /// callback computes a running time and hands off; it takes NO crate lock
    /// and the consumer contract forbids blocking
    /// ([`FcastPlaybin::set_subtitle_consumer`]). Nothing here can park a push
    /// the way a cue parked inside subtitleoverlay could, the geometry the
    /// disposal pair existed for at the shared seat and still exists for at
    /// PAUSED (see [`FlushReason::DisposalConsumer`]).
    ///
    /// # And why it is not a SINK-flagged child
    ///
    /// The `park_stream` treatment, for the same reasons: a GstBin folds
    /// POSITION and DURATION over its SINK-flagged children and aggregates
    /// their EOS. An unsynced text sink races to the item's end and would
    /// dominate the position fold, and would hold the pipeline's EOS on a
    /// sparse stream that has not drained.
    pub(crate) fn build_text_consumer_tail(
        inner: &Arc<Inner>,
        db3_src_pad: &gst::Pad,
        tqueue: &gst::Element,
        // The EXTERNAL input this branch serves, when it serves one. Gates
        // the misaligned-cue drop below on THAT input's in-flight replays:
        // an embedded stream's dropped cue is gone for good, and a replay in
        // flight for a DIFFERENT external re-delivers nothing here either.
        external: Option<crate::ExternalSubId>,
    ) -> Option<gst::Element> {
        let appsink = gst_app::AppSink::builder()
            .property("name", format!("fpb-textsink-{}", db3_src_pad.name()))
            // NO CAPS FILTER. It was here as "a second, structural gate behind
            // `decisions::consumer_stream_format`", and it does not work as one:
            // a filter on the tail constrains the whole BRANCH's negotiation,
            // and `gst_pad_link` compares `gst_pad_query_caps`, not the caps
            // that are actually flowing. Measured on an embedded DASH WebVTT
            // track: decodebin3's text src pad carries
            // `text/x-raw, format=pango-markup` (it parses internally, and
            // those are exactly the caps the renderer wants) while its caps
            // QUERY still answers `application/x-subtitle-vtt`, the unparsed
            // upstream shape. So the link was refused NOFORMAT for a stream
            // whose every buffer was renderable, and the branch never joined.
            //
            // The capability decision therefore lives in ONE place, where it
            // can be reported rather than merely fail: `consumer_stream_format`
            // on the pad's CURRENT caps at link time (the loud
            // `SubtitleTrackUnsupported` path), with `item_from_sample`
            // re-deciding per sample so a mid-stream renegotiation into
            // something unreadable drops buffers instead of feeding garbage.
            .sync(false)
            .async_(false)
            .drop(true)
            .max_buffers(TEXT_CONSUMER_MAX_BUFFERS)
            .enable_last_sample(false)
            .build();
        Self::assert_text_consumer_config(&appsink);
        appsink.unset_element_flags(gst::ElementFlags::SINK);

        // The pull side. Weak, so the callbacks can never keep `Inner` alive
        // (the pipeline owns the appsink, and `Inner` owns the pipeline).
        //
        // # BOTH callbacks, and why `new_preroll` is not optional
        //
        // basesink routes the FIRST buffer of every segment through its
        // preroll path while the pipeline is below PLAYING: it lands in
        // `gst_app_sink_preroll` -> `new_preroll`, and `new_sample` is NOT
        // called for it until playback resumes. With only `new_sample`
        // installed (and `emit-signals` false, so the signals fire into
        // nothing either) a cue delivered to a PAUSED pipeline reached no
        // consumer at all, which is precisely the paused track switch this
        // whole transport exists to make instant.
        //
        // The two are IDEMPOTENT against each other. The same buffer can be
        // seen once as a preroll and again as a sample when the state
        // advances; the cue carries identical text and identical running-time
        // bounds both times, and the renderer's single-active/latest-start
        // rule absorbs the duplicate submit without a flicker.
        let feed = move |sink: &gst_app::AppSink,
                         sample: gst::Sample,
                         weak: &std::sync::Weak<Inner>| {
            // A cue that arrives after its branch left the graph belongs to a
            // track the driver has already cleared. `detach_text_parts`
            // unlinks the appsink's sink pad BEFORE it sends its `Clear`, so
            // an unlinked pad here means exactly that: a buffer that was
            // already inside this chain call when the branch was severed, past
            // every DROP probe, and one cue-length of a stale track on screen
            // if it were delivered.
            if sink.static_pad("sink").is_some_and(|pad| !pad.is_linked()) {
                return;
            }
            if let Some(inner) = weak.upgrade()
                && let Some(item) = Inner::item_from_sample(&sample, inner.bitmap_subs)
            {
                // An EXTERNAL's cue on a FOREIGN timeline must not reach the
                // renderer WHILE THIS INPUT'S OWN REPLAY IS IN FLIGHT to
                // redeliver it. decodebin3 swaps a switched-to external onto
                // the already linked pad, so its file-origin burst flows here
                // BEFORE the realigning replay's flush (measured: `BBB00` at
                // origin 0 against a video at 5 s, under parallel load).
                // Scoped tightly on purpose: embedded tracks run sub-second
                // origin skews in steady state with no machinery to redeliver
                // a dropped cue (an unconditional gate blanked 4 suites), and
                // a replay in flight for a DIFFERENT external re-delivers
                // nothing here. Same rule as `take_parked_text_cues`' filter,
                // one lever for both.
                if let Some(id) = external
                    && let SubtitleFeedItem::Cue { origin, text, .. } = &item
                    && !Self::misaligned_cue_gate_off()
                    && inner
                        .replay_inflight
                        .lock()
                        .iter()
                        .any(|(rid, _)| *rid == id)
                {
                    let (_, video_origin) = inner.video_timeline();
                    if *origin != video_origin {
                        debug!(
                            %origin,
                            %video_origin,
                            text = %text.chars().take(24).collect::<String>(),
                            "dropped a cue delivered on another timeline; the in-flight replay re-delivers it"
                        );
                        return;
                    }
                }
                inner.feed_subtitle(item);
            }
        };
        let sample_weak = Arc::downgrade(inner);
        let preroll_weak = Arc::downgrade(inner);
        let preroll_feed = feed;
        appsink.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    // A malformed sample is DROPPED, never an error return: a
                    // non-OK flow here would latch the branch's queue and,
                    // through it, decodebin3's multiqueue slot. The zero
                    // timeout keeps even the pull itself off any wait: the
                    // buffer that triggered this callback is already queued.
                    if let Some(sample) = sink.try_pull_sample(gst::ClockTime::ZERO) {
                        feed(sink, sample, &sample_weak);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .new_preroll(move |sink| {
                    if let Some(sample) = sink.try_pull_preroll(gst::ClockTime::ZERO) {
                        preroll_feed(sink, sample, &preroll_weak);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );

        let entry = appsink.static_pad("sink").expect("appsink has a sink pad");
        // The transport's half of the `Clear` protocol. A FLUSH_STOP means
        // everything already delivered belongs to a timeline that no longer
        // exists (a seek, a rate change, a replay), and a STREAM_START means
        // the branch is carrying a different stream from the one whose cues
        // the consumer holds. Both are invisible to the driver, arriving on a
        // streaming thread from upstream, which is exactly why the
        // driver's own `Clear` sends cannot stand in for them.
        //
        // Takes no lock and does no work: `feed_subtitle` clones the callback
        // out and calls it, and the consumer must not block.
        // THE ZERO-LENGTH RECORD, and why it is dropped HERE rather than
        // decided about later.
        //
        // A PAUSED pipeline prerolls exactly ONE buffer per sink: basesink
        // hands the first one to `new_preroll` and blocks every buffer behind
        // it until PLAYING. So while paused, the branch delivers one record
        // and one only, and which record that is decides whether the frozen
        // frame gets a subtitle at all.
        //
        // Some subtitle tracks carry a ZERO-LENGTH twin before every real cue
        // -- same start, same text, `start == end` -- and a record that
        // occupies no time can never be shown: the renderer treats it as
        // expired at birth. Measured on the reporter's stream, whose WebVTT is
        // exactly this shape:
        //
        // ```text
        // 00:00:54.870 --> 00:00:54.870   the twin
        // 00:00:54.870 --> 00:00:56.800   the cue it stands in front of
        // ```
        //
        // A paused seek landing anywhere in the gap before 54.870 prerolled
        // the TWIN, spent the one slot on it, and left the real cue blocked in
        // the queue until playback resumed -- a blank frozen frame, and the
        // seek-lands-in-a-gap dependence is why it reads as intermittent.
        //
        // Dropping it before the sink is what makes the next record preroll
        // instead. Doing it in `item_from_sample` could not: by then basesink
        // has already prerolled, and the slot is spent whatever the callback
        // decides. `DURATION == 0` ONLY: an ABSENT duration means open-ended
        // ("show until something replaces it"), which is a normal and
        // showable cue, and dropping those would blank every format that
        // times itself in-band.
        //
        // A buffer that merely lies OUTSIDE the segment needs nothing here --
        // basesink clips it itself, before prerolling, and says so
        // ("dropping buffer, out of clipping segment", gstbasesink.c:4021).
        // A probe repeating that judgement was written, measured to change no
        // outcome, and removed; the case it was aimed at is handled by the GAP
        // drop further down.
        entry.add_probe(gst::PadProbeType::BUFFER, |_pad, info| {
            if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data
                && buffer.duration() == Some(gst::ClockTime::ZERO)
            {
                return gst::PadProbeReturn::Drop;
            }
            gst::PadProbeReturn::Ok
        });

        let weak = Arc::downgrade(inner);
        // The pad whose park may still be holding this branch's opening cues.
        // Cloned rather than borrowed: the probe outlives this call.
        let parked_on = db3_src_pad.clone();
        entry.add_probe(
            gst::PadProbeType::EVENT_DOWNSTREAM | gst::PadProbeType::EVENT_FLUSH,
            move |_pad, info| {
                // A GAP MUST NOT PREROLL THIS SINK, and it is what the field's
                // paused seek actually died on.
                //
                // basesink treats a GAP exactly like a buffer for preroll
                // purposes: `gst_base_sink_get_sync_times` gives it times,
                // `gst_base_sink_do_preroll` prerolls ON it
                // (gstbasesink.c:2485), and the sink then sits in
                // `gst_base_sink_wait_preroll` "waiting for flush or PLAYING"
                // (gstbasesink.c:2438). A GAP carries no cue by definition, so
                // a PAUSED pipeline -- which prerolls exactly one object per
                // sink -- spends its one and only slot on nothing and blocks
                // every real cue behind it until playback resumes.
                //
                // # Where the GAP comes from, and why only some transports
                //
                // `souphttpsrc` is push-only, so an MP4 fetched over HTTP
                // sends qtdemux down `gst_qtdemux_do_push_seek`, which asks
                // `gst_qtdemux_adjust_seek` for a byte offset with
                // `use_sparse = FALSE` (qtdemux.c:1351). The sparse text track
                // is therefore SKIPPED when the target offset is chosen
                // (qtdemux.c:1161): the byte seek lands on the VIDEO keyframe
                // before the target, the demuxer replays the mdat in FILE
                // order from there, and it fronts the text pad with a GAP to
                // carry the stream to the new segment start. The pull path
                // passes `use_sparse = TRUE` (qtdemux.c:1444), positions the
                // text stream on the covering sample and emits no such GAP --
                // which is why `file://` never showed this, and why DASH,
                // whose text is a separate whole-file push, never showed it
                // either.
                //
                // Measured on a generated tx3g MP4 over HTTP, PAUSED, seeking
                // to 4.190 s -- the reported gesture on the reported
                // transport:
                //
                // ```text
                // segment  start = 4.190
                // gap      timestamp = 4.190, duration = none  <- PREROLLS
                // buffer   pts 3.000 dur 1.000  <- stale; basesink clips it
                // buffer   pts 4.000 dur 1.000  <- THE COVERING CUE, blocked
                // ```
                //
                // The consumer saw the seek's `Clear` and then nothing at all
                // until resume, when the sink left preroll and the queue
                // drained -- the "a cue lands ~20 ms after resume" in the
                // capture. Dropping the GAP lets the covering buffer take the
                // slot instead, and the cue reaches the frozen frame.
                //
                // Safe because this sink is deliberately NOT a SINK-flagged
                // child (see this function's own note): the bin folds neither
                // its position nor its EOS, so nothing downstream of this pad
                // has any use for a GAP. The queue upstream still sees it and
                // still advances its own time level off it.
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && matches!(event.view(), gst::EventView::Gap(_))
                {
                    return gst::PadProbeReturn::Drop;
                }
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && matches!(
                        event.view(),
                        gst::EventView::FlushStop(_) | gst::EventView::StreamStart(_)
                    )
                    && let Some(inner) = weak.upgrade()
                {
                    // THE JOIN'S OWN WIPE IS NOT A WIPE. A fresh join links a
                    // LIVE decodebin3 pad to a brand-new queue, so every sticky
                    // is forwarded on the first push, including a STREAM_START
                    // that arrives here.
                    // What the consumer holds at that instant is not stale
                    // material from a track it has moved off: it is exactly
                    // the opening cues `Inner::take_parked_text_cues` just
                    // restored out of the park, and clearing them deletes the
                    // repair and nothing else.
                    //
                    // Measured on the field's own subtitle file (1022 cues,
                    // half of them zero-length twins, first showable cue
                    // 0.000-3.920): with the wipe the first covered frame is
                    // at 4.085 s, because only the SECOND replayed cue outlives
                    // it. Suppressed, the opening covers from the start.
                    //
                    // ONE Clear, and only one: the flag is armed by a replay
                    // that actually delivered something and consumed by the
                    // first Clear after it, so a seek's FLUSH_STOP, a real
                    // stream restart, and every later join still clear
                    // normally.
                    if inner.take_text_clear_suppression(&parked_on) {
                        debug!(
                            pad = %parked_on.name(),
                            "skipped the join's own clear, which would wipe the replayed opening"
                        );
                    } else {
                        inner.feed_subtitle(SubtitleFeedItem::Clear);
                    }
                }
                gst::PadProbeReturn::Ok
            },
        );

        let element = appsink.upcast::<gst::Element>();
        if let Err(err) = inner.pipeline.add(&element) {
            warn!(?err, "failed to add the text consumer sink");
            return None;
        }
        if tqueue.link(&element).is_err() {
            let _ = element.set_state(gst::State::Null);
            let _ = inner.pipeline.remove(&element);
            return None;
        }
        // TEST FAULT INJECTION (see [`Inner::stage_join_hold_ms`]): leave the
        // branch at NULL so the caller's upstream link lands on inactive pads,
        // which is the field's join window. The caller brings it up after the
        // hold.
        if inner
            .stage_join_hold_ms
            .load(std::sync::atomic::Ordering::SeqCst)
            > 0
        {
            return Some(element);
        }
        // Downstream first, so the queue never syncs into a tail that is still
        // at NULL.
        if element.sync_state_with_parent().is_err() || tqueue.sync_state_with_parent().is_err() {
            let _ = element.set_state(gst::State::Null);
            let _ = inner.pipeline.remove(&element);
            return None;
        }
        Some(element)
    }

    /// The consumer tail's configuration, asserted rather than merely written.
    ///
    /// WHAT THIS DOES AND DOES NOT CLAIM. It pins the properties that keep the
    /// branch wait-free AT PLAYING: unsynced, dropping rather than blocking
    /// once its bounded queue is full, retaining no sample. It does NOT make
    /// the branch wait-free in PAUSED: basesink prerolls regardless of
    /// `async` and parks the chain call before the appsink's own drop logic is
    /// ever consulted (the full argument is in
    /// [`Inner::dispose_text_branch_on`]). That case is covered by the flush
    /// pair at disposal, not by anything on this list.
    ///
    /// A future edit that flips one of these silently would retire the PLAYING
    /// half of the claim without retiring the reasoning built on it.
    fn assert_text_consumer_config(appsink: &gst_app::AppSink) {
        let sync: bool = appsink.property("sync");
        let async_: bool = appsink.property("async");
        let dropping: bool = appsink.property("drop");
        let max_buffers: u32 = appsink.property("max-buffers");
        let last_sample: bool = appsink.property("enable-last-sample");
        // NO caps filter, and this one is not about waiting. A filter here
        // constrains the whole branch's NEGOTIATION, and `gst_pad_link` is
        // decided on `gst_pad_query_caps` rather than on the caps that flow --
        // so a filter refuses streams whose every buffer is renderable
        // whenever the source's query answers with its unparsed shape (an
        // embedded DASH WebVTT track: parsed `text/x-raw, format=pango-markup`
        // buffers behind an `application/x-subtitle-vtt` query). The
        // capability decision belongs to `decisions::consumer_stream_format`,
        // which can report the refusal instead of merely failing to link.
        let filter: Option<gst::Caps> = appsink.property("caps");
        let ok = !sync
            && !async_
            && dropping
            && max_buffers == TEXT_CONSUMER_MAX_BUFFERS
            && !last_sample
            && filter.is_none();
        if !ok {
            error!(
                sync,
                async_,
                dropping,
                max_buffers,
                last_sample,
                filter = ?filter.map(|c| c.to_string()),
                "the text consumer sink is misconfigured: it must stay wait-free at PLAYING \
                 (or the text branch can block a streaming thread nothing is waiting to wake) \
                 and must not filter caps (or it refuses renderable streams at link time)"
            );
        }
        debug_assert!(
            ok,
            "the text consumer sink must stay wait-free at PLAYING and filter no caps"
        );
    }

    /// The running time a subtitle unit occupies, CLIPPED to its segment.
    ///
    /// # Why a clip and not a plain `to_running_time`
    ///
    /// A seek lands INSIDE a cue, and the demuxer faithfully resends the unit
    /// whose `[pts, pts+duration)` covers the target, with its ORIGINAL pts,
    /// which now precedes `segment.start`. `gst_segment_to_running_time`
    /// answers nothing for a position outside the segment, so a transport that
    /// drops on that `None` drops the one cue the viewer should be looking at.
    /// Measured, on both containers the receiver actually plays:
    ///
    /// ```text
    /// matroskademux  cue 1s..3s, seek to 2.0s -> resent pts=1.0s, segment.start=2.0s
    /// qtdemux (tx3g) cue 5s..6s, seek to 5.5s -> resent pts=5.0s, segment.start=5.5s
    /// ```
    ///
    /// Both symptoms follow from that one drop: PAUSED, the covering cue is
    /// the only one that will ever arrive, so the frozen frame stays bare;
    /// PLAYING, nothing shows until the NEXT cue starts, which reads as a
    /// delay after the seek.
    ///
    /// So clip, the way every other GStreamer consumer clips: a unit that
    /// OVERLAPS the segment starts at the segment's own running time and keeps
    /// its real end, and only a unit lying WHOLLY before the segment is
    /// dropped. That is `gst_segment_clip`'s rule verbatim, which also settles
    /// the duration-less case: a cue with no duration is open-ended, so it
    /// overlaps by definition and is clamped rather than dropped.
    fn clipped_running_time(
        segment: &gst::FormattedSegment<gst::ClockTime>,
        pts: gst::ClockTime,
        duration: Option<gst::ClockTime>,
    ) -> Option<ClippedTime> {
        // `checked_add`: a unit whose pts+duration overflows is a corrupt
        // timestamp, not a reason to abort a streaming thread.
        let end = duration.and_then(|duration| pts.checked_add(duration));
        let (start, stop) = segment.clip(pts, end)?;
        let start = start?;
        // A window that occupies NO time cannot be shown, so it is not a cue.
        // The zero-length twins are dropped ahead of the sink (see
        // `build_text_consumer_tail`, which has to, or they spend the paused
        // preroll slot); this is the same judgement stated where the window is
        // actually computed, so a clip that lands degenerate for any other
        // reason -- a unit ending exactly at `segment.start` -- is refused
        // rather than fed to a renderer as an already-expired cue.
        if stop == Some(start) {
            return None;
        }
        let start_rt = segment.to_running_time(start)?;
        let end_rt = stop.and_then(|stop| segment.to_running_time(stop));
        // ORDERED, because a REVERSE segment measures running time from `stop`
        // (`(stop - t)/|rate| + base`) and inverts the pair. A renderer expires
        // on `now >= end_rt`, so an inverted window is expired at birth and no
        // cue renders for a whole reverse play. The interval is the same either
        // way, only measured from the other end, and forward segments never
        // swap (`to_running_time` is monotone there). Reached through
        // `pipeline::rate_seek_event`, which builds a real reverse seek for a
        // negative rate.
        let (start_rt, end_rt) = match end_rt {
            Some(end_rt) if end_rt < start_rt => (end_rt, Some(start_rt)),
            end_rt => (start_rt, end_rt),
        };
        Some(ClippedTime {
            start_rt,
            end_rt,
            trimmed: start.saturating_sub(pts),
        })
    }

    /// Turn one appsink sample into a [`SubtitleFeedItem`].
    ///
    /// The sample carries buffer, caps and SEGMENT together, so every bound
    /// resolves to running time with nothing remembered between samples, and
    /// because `sync_text_running_time`'s pad offset has already rewritten the
    /// segment on the way out of the decodebin3 pad, that running time is
    /// already on the video's base.
    ///
    /// THE SPLIT IS THE GATE'S ANSWER, and nothing before it. A text stream
    /// takes the arm it always took, character for character: bounds, the
    /// UTF-8 hard-fail, then the cue-IR meta. A bitmap stream never reaches
    /// any of that (never mapped, never checked for UTF-8, never probed for a
    /// `CueIrMeta`) because its bytes are a decoder's problem and this
    /// thread does no work proportional to a payload.
    ///
    /// `None` for anything the consumer could not use: no buffer, no PTS, a
    /// segment that does not map, non-UTF-8 bytes in a TEXT stream, or caps
    /// that changed under the branch to something the gate refuses.
    pub(crate) fn item_from_sample(
        sample: &gst::Sample,
        enabled: BitmapSubsEnabled,
    ) -> Option<SubtitleFeedItem> {
        let buffer = sample.buffer()?;
        match decisions::consumer_stream_format(sample.caps()?, enabled)? {
            decisions::ConsumerStreamFormat::Text(format) => {
                let segment = sample.segment()?.downcast_ref::<gst::ClockTime>()?;
                let pts = buffer.pts()?;
                let clipped = Self::clipped_running_time(segment, pts, buffer.duration())?;
                let start_rt = clipped.start_rt;
                let end_rt = clipped.end_rt;
                let map = buffer.map_readable().ok()?;
                // THE UTF-8 CHECK IS NOT WEAKENED. A cue-IR buffer's payload is the
                // IR's own `plain_text()`, so it is valid UTF-8 by construction and
                // passes the very same gate; the meta only ADDS structure beside text
                // that is already renderable. A consumer that ignores the IR still
                // shows this string, which is why the split is here and not in front of
                // the check.
                let text = std::str::from_utf8(map.as_slice()).ok()?;
                // `text-format=cue-ir`: the styling rode along as a buffer meta, under
                // caps that are indistinguishable from plain utf8. Reading it is a
                // downcast on a meta this process registered, not a parse.
                let format = match cue_ir_enabled()
                    .then(|| buffer.meta::<gstrssubparse::cueir::CueIrMeta>())
                    .flatten()
                {
                    Some(meta) => SubtitleTextFormat::CueIr {
                        ir: Arc::new(meta.ir().clone()),
                        pts_start: Some(pts),
                    },
                    None => format,
                };
                Some(SubtitleFeedItem::Cue {
                    format,
                    text: text.to_string(),
                    start_rt,
                    end_rt,
                    origin: Self::segment_origin(segment),
                })
            }
            decisions::ConsumerStreamFormat::Bitmap(format) => {
                let segment = sample.segment()?.downcast_ref::<gst::ClockTime>()?;
                // The same clip as the text arm, for the same reason: VOBSUB's
                // self-contained units are exactly the ones a mid-cue seek
                // resends with a pre-target pts (the ledger's P12(b) residual).
                let clipped =
                    Self::clipped_running_time(segment, buffer.pts()?, buffer.duration())?;
                let rt = clipped.start_rt;
                // The setup bytes live in the CAPS, not in the stream: VOBSUB's
                // palette is the container's CodecPrivate. Read per sample so a
                // renegotiation arrives together with the packet it applies to,
                // and cloned by reference-count like the payload.
                let codec_data = sample
                    .caps()
                    .and_then(|caps| caps.structure(0))
                    .and_then(|structure| structure.get::<gst::Buffer>("codec_data").ok());
                Some(SubtitleFeedItem::Bitmap {
                    format,
                    data: sample.buffer_owned()?,
                    codec_data,
                    // The container's own duration when it gave one, less
                    // whatever the clip took off the head, so a unit the seek
                    // landed inside reports the time it has LEFT; these
                    // formats time themselves in-band and usually give none.
                    duration: buffer
                        .duration()
                        .map(|duration| duration.saturating_sub(clipped.trimmed)),
                    rt,
                })
            }
        }
    }
}

impl FcastPlaybin {
    /// Install the sink for subtitle cues.
    ///
    /// # What it replaced
    ///
    /// The text branch ends in an `appsink`, where it used to end in
    /// subtitleoverlay's shared `subtitle_sink`, and every cue it pulls
    /// arrives here as a [`SubtitleFeedItem::Cue`] already resolved to running
    /// time. Rendering then belongs to whoever installed this. In the
    /// receiver, `fcast-video`'s cue engine, which this crate deliberately
    /// cannot name (it has no dependency on `fcast-video`, and the driver must
    /// not grow one).
    ///
    /// A caller that installs NO consumer renders no subtitles at all. That is
    /// the whole of the policy: there is no compositor left in
    /// the pipeline to fall back to.
    ///
    /// # Contract on the callback
    ///
    /// IT MUST NOT BLOCK, AND MUST NOT PANIC.
    ///
    /// Cues arrive on the text branch's streaming thread, inside the appsink's
    /// `new_sample` or `new_preroll`; a `Clear` may additionally arrive on
    /// that branch's pad probe (a flush or a stream restart) or on the
    /// CALLER's own thread (a load or stop superseding the item, a track
    /// switch). So this runs on several threads, never concurrently with
    /// itself for one branch but with no ordering promise beyond that, and
    /// blocking any of them stalls something that was not waiting for a
    /// renderer. A panic is caught and logged rather than unwound through
    /// GStreamer's C frames, but the cue is lost.
    ///
    /// It must also not call back into this crate: no crate lock is held while
    /// it runs, but re-entering the driver from a streaming thread is the
    /// deadlock shape everything else here is written to avoid.
    ///
    /// Call at most once, before driving playback.
    pub fn set_subtitle_consumer(
        &self,
        consumer: impl Fn(SubtitleFeedItem) + Send + Sync + 'static,
    ) {
        *self.inner.subtitle_consumer.lock() = Some(Arc::new(consumer));
    }

    /// Which stream is OBSERVED to be the live text branch (see
    /// [`Inner::observed_seat_occupant`]), for a test that wants to compare
    /// the probe against the `last_applied_subtitle` mirror it replaces. Not
    /// part of the public API.
    #[doc(hidden)]
    pub fn observed_seat_occupant(&self) -> Option<String> {
        self.inner.observed_seat_occupant()
    }

    /// What the `last_applied_subtitle` MIRROR currently claims, so a test can
    /// show the two diverging. Not part of the public API.
    #[doc(hidden)]
    pub fn mirrored_seat_occupant(&self) -> Option<String> {
        self.inner.last_applied_subtitle.lock().clone()
    }
}
