//! THE BASELINE for the flush-policy removals: which crate-origin flush pairs
//! fire today, named by reason.
//!
//! The exit criterion for removing a pair is "the mid-play reasons read zero",
//! and a zero is worth nothing unless the instrument that reports it
//! demonstrably moved BEFORE the removal. These tests were therefore written
//! against UNMODIFIED behaviour first and recorded the reasons firing
//! (`eager_branch` 1, `disposal_*` 2 each, `remove_input` 2 on the schedule
//! below). The removals then flipped `eager_branch` and `disposal_queue` to
//! zero, and every direction is asserted here against a lever:
//!
//! * [`a_mid_play_subtitle_schedule_fires_the_reasons_the_removals_touch`]
//!   drives attach / select / replace / off / detach at PLAYING and at PAUSED.
//!   The direct REPLACE PARKS, and the flush it used to be able to choose --
//!   with its `eager_text_flushes()` counter, its `eager_branch` reason and its
//!   `FCAST_EAGER_REPLACE_FLUSH` lever -- is gone. The `remove_input` pair is
//!   still SENT: the quiescence skip was measured to break the same-URL
//!   re-attach, so its de-PLAY survives and is RECORDED here rather than
//!   removed. The mid-play `disposal_queue` pair is gone, the branch being
//!   proved quiescent instead. The disposal pair moved: `disposal_seat` was
//!   subtitleoverlay's shared `subtitle_sink` and died with it, and
//!   `disposal_consumer` is the same pair on the branch's OWN appsink, asserted
//!   positive here. The teardown boundary's pairs are counted under their OWN
//!   reasons (`teardown_consumer`, `teardown_queue`) so the mid-play zeros are
//!   absolute rather than deltas, which is what makes them safe to assert while
//!   another test in this binary tears down.
//! * [`a_teardown_fires_the_teardown_reasons_and_splits_no_pair`] covers the
//!   boundary that STAYS, and the invariant that holds everywhere: no pair may
//!   straddle a pad deactivation, because gstpad.c discards a FLUSH_STOP on an
//!   inactive pad and the pad then flushes for good.
//!
//! # Why the counters are read as `FcastPlaybin::` associated functions
//!
//! They are process-global. The teardown pairs are sent from `Teardown::run`,
//! after `Inner` is gone, so a per-instance counter could not be read for the
//! very reasons that matter most. The consequence is that counts are
//! CUMULATIVE across a binary's tests, which is why every assertion here is
//! "this reason fired" or "this reason never fires anywhere" and never "fired
//! exactly N times".
//!
//! # Verification
//!
//! * Green: no env vars.
//! * `FCAST_REMOVE_INPUT_FLUSH_SKIP=1`: the `remove_input` assertions invert
//!   (the gate skips the pair). THIS ARM IS EXPECTED TO BREAK
//!   `external_subtitle_lifecycle`'s two re-attach tests - that failure is the
//!   measurement the skip is off for.
//! * `FCAST_NO_REMOVE_INPUT_FLUSH_SKIP=1`: restores v1's `remove_input`
//!   wholesale (old order, unconditional pair); the assertions are unchanged
//!   because the shipped half is behaviour-neutral.
//! * `FCAST_DISPOSAL_QUEUE_FLUSH=1`: the b2 assertion inverts (mid-play
//!   disposals flush their queue again).
//! * `FCAST_FLUSH_TAP=1`: adds one `info!` line per pair (pad, parent, reason)
//!   with `FCASTPLAYBIN_TEST_LOG=info`; the counts are unchanged.

use std::{
    sync::{Arc, Mutex, mpsc},
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use fcasttest::{
    scenario::ScenarioBuilder,
    sink::FTestSink,
    spec::{CueSpec, Pacing},
};
use gst::prelude::*;

#[path = "support/text_arm.rs"]
mod text_arm;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

fn cues(count: u32, step: gst::ClockTime, tag: &str) -> Vec<CueSpec> {
    (0..count)
        .map(|index| {
            let start = step * u64::from(index + 1);
            CueSpec::new(start, start + step / 2, format!("{tag}{index:02}"))
        })
        .collect()
}

/// The disposal pair, now narrowed to the teardown boundary.
const DISPOSAL_QUEUE: &str = "disposal_queue";
/// The disposal pair on the CONSUMER branch's own appsink sink pad, which is
/// what a mid-play disposal sends there instead of the seat pair.
const DISPOSAL_CONSUMER: &str = "disposal_consumer";

fn breakdown() -> String {
    format!("{:?}", FcastPlaybin::crate_flush_pair_breakdown())
}

struct Harness {
    playbin: Arc<FcastPlaybin>,
    events: mpsc::Receiver<PlaybinEvent>,
    video: fcasttest::sink::Recording,
    paused: std::cell::Cell<bool>,
    /// Cue payloads reaching the renderer, from [`Harness::tap_text`].
    text: std::cell::RefCell<Option<text_arm::CueTap>>,
}

impl Harness {
    fn new() -> Self {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, _generation| {
            let _ = tx.send(event);
        });
        // The cue FEED, established before anything can flow (an unsynced
        // external hands its whole file over the moment its branch links on
        // the consumer arm). Inert on the overlay arm, whose probe point is
        // not in the pipeline yet.
        text_arm::arm(&playbin);
        Self {
            playbin: Arc::new(playbin),
            events,
            video,
            paused: std::cell::Cell::new(false),
            text: std::cell::RefCell::new(None),
        }
    }

    fn pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(SelectionGate {
            quiet: true,
            paused: self.paused.get(),
            seekable: false,
        });
        while let Ok(event) = self.events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &event {
                panic!("pipeline error: {error}");
            }
        }
    }

    fn wait_for(&self, what: &str, mut done: impl FnMut() -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !done() {
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}; video buffers {}, pending disposals {}, \
                 text tail peers {:?}, flush pairs {}",
                self.video.buffer_count(),
                self.playbin.pending_text_disposals(),
                self.text_tail_peers(),
                breakdown(),
            );
            self.pump();
            thread::sleep(Duration::from_millis(5));
        }
    }

    /// What is wired into the text renderer right now, for a failure message.
    fn text_tail_peers(&self) -> Vec<String> {
        text_arm::text_tail_pads(&self.playbin)
            .iter()
            .filter_map(|pad| pad.peer())
            .map(|peer| peer.name().to_string())
            .collect()
    }

    /// Start reading cue payloads, so "delivering" is a cue and not merely a
    /// link. Installed once the branch is linked, which is where the overlay
    /// arm's probe point finally exists; the consumer arm loses nothing,
    /// because the tap backfills out of the feed armed at construction.
    fn tap_text(&self) {
        *self.text.borrow_mut() = Some(text_arm::tap_cue_payloads(&self.playbin));
    }

    fn cues_seen(&self, tag: &str) -> usize {
        self.text
            .borrow()
            .as_ref()
            .expect("the cue tap is installed")
            .lock()
            .expect("text tap")
            .iter()
            .filter(|(payload, _)| payload.trim_start().starts_with(tag))
            .count()
    }

    fn set_paused(&self, paused: bool) {
        if paused {
            self.playbin.pause().expect("pause");
        } else {
            self.playbin.play().expect("play");
        }
        self.paused.set(paused);
        let want = if paused {
            gst::State::Paused
        } else {
            gst::State::Playing
        };
        self.wait_for("the transport to settle", || {
            self.playbin.state_summary() == (want, gst::State::VoidPending)
                && !self.playbin.has_async_transition()
        });
    }

    fn shutdown(&self) {
        let (tx, rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = tx.send(());
        }));
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match rx.recv_timeout(Duration::from_millis(20)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the shutdown never finished");
                    self.pump();
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

/// A playing item plus a dense external subtitle, selected and delivering.
fn play_with_external(
    tag: &str,
    subs_tag: &str,
) -> (
    Harness,
    fcasttest::scenario::ScenarioHandle,
    fcasttest::scenario::ScenarioHandle,
    fcastplaybin::ExternalSubId,
) {
    let media = ScenarioBuilder::new(&format!("{tag}main"))
        .video("video_0")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(120))
        .pacing(Pacing::Realtime)
        .register();
    let subs = ScenarioBuilder::new(&format!("{tag}subs"))
        .text(
            "text_0",
            cues(600, gst::ClockTime::from_mseconds(100), subs_tag),
        )
        .duration(gst::ClockTime::from_seconds(120))
        .pacing(Pacing::AsFastAsPossible)
        .register();

    let harness = Harness::new();
    harness.playbin.load_async(
        MediaInput::Uri(media.uri()),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    {
        let mut loaded = false;
        harness.wait_for("the load to report Loaded", || {
            while let Ok(event) = harness.events.try_recv() {
                if let PlaybinEvent::Error { error, .. } = &event {
                    panic!("pipeline error during the load: {error}");
                }
                loaded |= matches!(event, PlaybinEvent::Loaded { .. });
            }
            loaded
        });
    }
    harness.set_paused(false);

    let id = harness
        .playbin
        .attach_subtitle(&subs.uri())
        .expect("attaching the external subtitle input");
    harness.wait_for("the external subtitle stream to materialize", || {
        !harness.playbin.subtitle_stream_ids(id).is_empty()
    });
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::ExternalSubtitle(id));
    harness.pump();
    harness.wait_for("the text branch to join the renderer", || {
        text_arm::text_branch_linked(&harness.playbin)
    });
    harness.tap_text();
    harness.wait_for("the first cue to reach the renderer", || {
        harness.cues_seen(subs_tag) > 0
    });
    (harness, media, subs, id)
}

/// WHAT THE PAIR CLAIMS, not just that it fired: every crate-origin
/// `FLUSH_STOP` must carry `reset_time = FALSE`.
///
/// A flush pair this crate injects exists to wake a parked push. It is not a
/// seek, nothing about the timeline moved, and `reset_time = TRUE` says
/// otherwise to everything below it:
///
/// * `gst_base_sink_flush_stop` re-inits the sink's segment to
///   `GST_FORMAT_UNDEFINED` (gstbasesink.c:3276-3280). A sink with an
///   UNDEFINED-format segment stops answering POSITION queries entirely
///   (`no_segment`, gstbasesink.c:5168-5169 -> 5378-5386, and it does not even
///   fall back to an upstream query), and feeds the unguarded
///   `gst_segment_to_stream_time (&dec->output_segment, GST_FORMAT_TIME, ...)`
///   in `gst_video_decoder_src_query_default` (gstvideodecoder.c:2071) the
///   GStreamer-CRITICAL `dash-embedded-subs-delayed.txt` carries.
/// * It posts `GST_MESSAGE_RESET_TIME` (gstbasesink.c:3291-3294), which
///   `GstPipeline` turns into `reset_start_time (pipeline, 0)`
///   (gstpipeline.c:619-628). What that costs a PAUSED pipeline is measured in
///   `regression_text_reconcile::a_replay_while_paused_does_not_restart_the_
///   pipeline_timeline`: the position restarts at zero and the video sink
///   stalls until the clock catches up with the frames it already has.
///
/// The flag is asserted HERE, on the event itself, rather than through a
/// pipeline-timeline test, and the reason is honest: every reachable
/// crate-injected pair fires at a settled PLAYING or at a teardown - a detach
/// requested while PAUSED is POSTPONED to the next PLAYING drain
/// (`Inner::run_deferred_text_work`) - and `reset_start_time` writes nothing
/// while PLAYING (gstpipeline.c:318). A timeline assertion around a detach
/// therefore passes with the flag either way, measured, so it would be a
/// vacuous test. The flag is the part that is decidable, and the cost of
/// getting it wrong is proven elsewhere.
///
/// A/B: `FCAST_FLUSH_STOP_RESETS_TIME=1` restores the v1 flag and fails here.
#[test]
fn every_crate_origin_flush_stop_leaves_the_running_time_alone() {
    init();
    let (harness, media, subs, id) = play_with_external("flushresettime", "R");

    // Installed only once the item is settled and delivering, so nothing the
    // LOAD did (a start seek's flush is upstream's and legitimately resets)
    // can be mistaken for a crate-origin pair.
    //
    // EVERY pad in the pipeline, not just decodebin3's sink pads. The crate
    // aims its pairs at four different places and a detach uses two of them
    // (`remove_input` on the decodebin3 sink pads, `disposal_consumer` on the
    // leaving branch's own appsink), and the one that matters most for the
    // flag is the one that ends at a `GstBaseSink`. Probing narrowly measured
    // ZERO here: a decodebin3 request pad can be inactive by the time the pair
    // reaches it, and `gst_pad_send_event_unchecked` discards a FLUSH_STOP on
    // an inactive pad without running probes (gstpad.c:5910-5911).
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let elements: Vec<gst::Element> = harness
        .playbin
        .pipeline()
        .iterate_recurse()
        .into_iter()
        .flatten()
        .collect();
    for element in &elements {
        for pad in element.pads() {
            let name = format!("{}:{}", element.name(), pad.name());
            let seen = seen.clone();
            // EVENT_FLUSH must be asked for explicitly; EVENT_DOWNSTREAM does
            // not imply it.
            pad.add_probe(gst::PadProbeType::EVENT_FLUSH, move |_pad, info| {
                if let Some(gst::PadProbeData::Event(event)) = &info.data
                    && let gst::EventView::FlushStop(stop) = event.view()
                {
                    seen.lock()
                        .expect("flush stop log")
                        .push((name.clone(), stop.resets_time()));
                }
                gst::PadProbeReturn::Ok
            })
            .expect("installing the flush-stop probe");
        }
    }

    // `remove_input` is the crate-injected pair a caller can reach on demand.
    harness
        .playbin
        .detach_subtitle(id)
        .expect("detaching the external");
    harness.wait_for("the external to leave routing", || {
        !harness.playbin.has_external_subtitles()
    });

    let log = seen.lock().expect("flush stop log").clone();
    assert!(
        !log.is_empty(),
        "no FLUSH_STOP crossed any pad in the pipeline across a detach, so this test \
         observed nothing and asserts nothing. Breakdown: {}",
        breakdown()
    );
    let resetting: Vec<&(String, bool)> = log.iter().filter(|(_, resets)| *resets).collect();
    assert!(
        resetting.is_empty(),
        "a crate-origin FLUSH_STOP asked every sink below it to reset its running time \
         ({resetting:?}); the crate did not move the timeline and must not say it did. \
         All FLUSH_STOPs seen: {log:?}"
    );

    media.release_all();
    subs.release_all();
    harness.shutdown();
    media.unregister();
    subs.unregister();
}

/// One mid-play subtitle schedule, reaching every reason the removals touch.
///
/// The schedule:
///
/// 1. attach an external and select it (a branch links);
/// 2. attach a SECOND external and select that - the direct REPLACE, the one
///    dispatch shape that could ever have chosen the eager FLUSH over the park.
///    It PARKS, and now there is no other answer to choose;
/// 3. subtitles off at a settled PLAYING (an inline disposal);
/// 4. re-select, then subtitles off at a resting PAUSED and back to PLAYING (a
///    POSTPONED disposal, drained by `run_deferred_text_work`);
/// 5. detach both externals at a settled PLAYING - two QUIESCENT removals,
///    which send no `remove_input` pair.
///
/// Pair D is asserted POSITIVE under `disposal_consumer`: every mid-play
/// disposal here aims it at its own branch's appsink sink pad, so unlike the
/// overlay seat it could not be raced away by a replacement taking the pad.
#[test]
fn a_mid_play_subtitle_schedule_fires_the_reasons_the_removals_touch() {
    init();
    let (harness, media, first, first_id) = play_with_external("flushcensus", "A");

    let queue_before = FcastPlaybin::crate_flush_pairs_for(DISPOSAL_QUEUE);

    // (2) the direct REPLACE: a second external selected over a live one.
    let second = ScenarioBuilder::new("flushcensussubs2")
        .text("text_0", cues(600, gst::ClockTime::from_mseconds(100), "B"))
        .duration(gst::ClockTime::from_seconds(120))
        .pacing(Pacing::AsFastAsPossible)
        .register();
    let second_id = harness
        .playbin
        .attach_subtitle(&second.uri())
        .expect("attaching the second external");
    harness.wait_for("the second external to materialize", || {
        !harness.playbin.subtitle_stream_ids(second_id).is_empty()
    });
    harness.playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::ExternalSubtitle(second_id),
    );
    harness.pump();
    harness.wait_for("the replacement external to deliver a cue", || {
        harness.cues_seen("B") > 0
    });

    // THE EAGER FLUSH REMOVAL, completed. The direct replace was the one
    // dispatch shape whose eager text work could be a FLUSH, and that flush was
    // the trigger of the captured decider-versus-overlay deadlock. It became a
    // PARK with the flush kept reachable behind `FCAST_EAGER_REPLACE_FLUSH`,
    // and deleting subtitleoverlay took the flush, its intent counter, its
    // lever and its `eager_branch` census reason. Nothing is asserted here any more
    // because there is nothing left to assert AGAINST: an asserted zero on a
    // deleted reason would panic in `crate_flush_pairs_for`, and one on a
    // deleted counter cannot be written at all. What the removal is worth is
    // measured by the schedule continuing to work -- the replace below still
    // switches tracks, and `disposal_consumer` still fires for it.

    // (3) subtitles off at a settled PLAYING: an INLINE disposal.
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    harness.pump();
    harness.wait_for("the text branch to leave the renderer", || {
        !text_arm::text_branch_linked(&harness.playbin)
    });

    // (4) the POSTPONED disposal: re-select, then off at a resting PAUSED.
    harness.playbin.request_track(
        TrackSlot::Subtitle,
        TrackTarget::ExternalSubtitle(second_id),
    );
    harness.wait_for("the re-selected external to link again", || {
        text_arm::text_branch_linked(&harness.playbin)
    });
    harness.set_paused(true);
    harness
        .playbin
        .request_track(TrackSlot::Subtitle, TrackTarget::Stream(None));
    harness.pump();
    // WHICH disposal this is depends on the transport, and the difference is
    // deliberate. An OVERLAY branch's disposal is postponed at a resting
    // PAUSED (its flush cannot complete while the queue's task is parked
    // inside the renderer behind sinks in `wait_preroll`), and the drain
    // runs it on the resume. A CONSUMER branch's is carried out INLINE right
    // here: the pair flushes the appsink's own sink pad, which is exactly
    // what wakes its parked push, and postponing would leave the next
    // track's branch unbuildable (`Inner::detach_text_parts`). Both are
    // asserted rather than assumed, because a schedule that silently stopped
    // reaching a disposal at all would satisfy every zero below.
    harness.wait_for("the branch to be disposed of inline while paused", || {
        harness.playbin.pending_text_disposals() == 0
            && !text_arm::text_branch_linked(&harness.playbin)
    });
    harness.set_paused(false);

    // (5) detach both externals, at a settled PLAYING. Their branches are
    // already detached and their sources drained, so both removals are
    // quiescent: this is the common case.
    let skipped_before = FcastPlaybin::remove_input_pairs_skipped();
    harness
        .playbin
        .detach_subtitle(first_id)
        .expect("detaching the first external");
    harness
        .playbin
        .detach_subtitle(second_id)
        .expect("detaching the second external");
    harness.wait_for("both externals to leave routing", || {
        !harness.playbin.has_external_subtitles()
    });

    // RECORDED rather than removed. The quiescence skip was measured to break
    // the same-URL re-attach (see `Inner::remove_input`), so the pair still
    // goes out on every removal and the de-PLAY it causes is still there. What
    // did ship is the unlink ABOVE the pair,
    // which is behaviour-neutral here by design - so the pair count is
    // deliberately asserted to be UNCHANGED from v1.
    let skipped_after = FcastPlaybin::remove_input_pairs_skipped();
    if std::env::var_os("FCAST_REMOVE_INPUT_FLUSH_SKIP").is_some() {
        assert!(
            skipped_after > skipped_before,
            "FCAST_REMOVE_INPUT_FLUSH_SKIP turns the quiescence gate on, so two quiescent \
             detaches must skip a pair ({skipped_before} -> {skipped_after}); the gate is \
             wired to nothing. (This arm is expected to FAIL \
             external_subtitle_lifecycle's re-attach tests - that is the measurement the \
             skip is off for.)"
        );
    } else {
        assert!(
            FcastPlaybin::remove_input_pairs_sent() > 0,
            "a detach sent no `remove_input` pair, but the quiescence skip is off; the \
             pair is load-bearing for decodebin3 retiring the leaving input's src pads. \
             Breakdown: {}",
            breakdown()
        );
        assert_eq!(
            skipped_after, skipped_before,
            "a `remove_input` pair was skipped with FCAST_REMOVE_INPUT_FLUSH_SKIP unset"
        );
    }

    // THE b2 assertion. A mid-play disposal proves its branch is quiescent and
    // takes the queue to NULL without a pair; only the counted
    // quiesce-timeout fallback still sends one, and the teardown boundary's
    // pair is a DIFFERENT reason (`teardown_queue`) precisely so this can be
    // an absolute zero rather than a delta - a delta is not safe to assert
    // when another test in this binary may be tearing down beside us.
    let queue_after = FcastPlaybin::crate_flush_pairs_for(DISPOSAL_QUEUE);
    if std::env::var_os("FCAST_DISPOSAL_QUEUE_FLUSH").is_some() {
        assert!(
            queue_after > queue_before,
            "FCAST_DISPOSAL_QUEUE_FLUSH restores the unconditional v1 queue pair, so four \
             mid-play disposals must send one ({queue_before} -> {queue_after}); the lever \
             is wired to nothing. Breakdown: {}",
            breakdown()
        );
    } else {
        assert_eq!(
            queue_after,
            0,
            "{queue_after} mid-play `{DISPOSAL_QUEUE}` pair(s) were sent anywhere in this \
             binary, with {} quiesce timeout(s) recorded; the pair latches the slot \
             decodebin3 is about to reuse. Breakdown: {}",
            FcastPlaybin::disposal_quiesce_timeouts(),
            breakdown()
        );
        assert_eq!(
            FcastPlaybin::disposal_quiesce_timeouts(),
            0,
            "a mid-play branch would not quiesce inside its budget and fell back to the v1 \
             pair. Not a failure by itself - worst case equals v1 - but on this schedule, \
             where the detach-time time-uncap should have released every parked push, it \
             means the uncap is not doing its job"
        );
    }
    println!(
        "b2: disposal_queue {queue_before} -> {queue_after}, quiesce timeouts {}",
        FcastPlaybin::disposal_quiesce_timeouts()
    );
    // PAIR D.
    //
    // `disposal_seat` and `teardown_seat`, the pair aimed at subtitleoverlay's
    // shared `subtitle_sink`, were asserted zero on this transport. Deleting
    // the element took the REASONS with it rather than leaving them as
    // tombstones: `crate_flush_pairs_for` panics on an unknown name, so an
    // assertion naming a reason with no producer would be an assertion wired
    // to nothing, which is precisely the failure `FlushReason`'s own doc warns
    // about. What survives is the positive half, and it is the half that
    // cannot pass vacuously: the disposals this schedule performs are counted
    // under `disposal_consumer` (the same pair one element along, on the
    // branch's own appsink), and a schedule that silently stopped disposing of
    // anything fails here.
    assert!(
        FcastPlaybin::crate_flush_pairs_for(DISPOSAL_CONSUMER) > 0,
        "no `{DISPOSAL_CONSUMER}` pair was sent although this schedule disposes of four \
         mid-play branches. Breakdown: {}",
        breakdown()
    );
    println!("flush census after the mid-play schedule: {}", breakdown(),);

    // The invariants that hold everywhere.
    assert_eq!(
        FcastPlaybin::flush_pair_activity_transitions(),
        0,
        "a flush pair straddled a pad deactivation, so its FLUSH_STOP was discarded \
         and the pad is flushing for good"
    );
    // The flow census per stage, and EVERY stage now. The one that was nonzero
    // by construction, `dispose_text_branch`, surveyed subtitleoverlay's shared
    // `subtitle_sink` and went with the element. A per-stream tail leaves with
    // its branch, so no pad stays behind to latch.
    for stage in [
        "detach_text_parts",
        "remove_input",
        "remove_video_chain",
        "ensure_video_chain",
    ] {
        assert_eq!(
            FcastPlaybin::flow_census_flushing_for(stage),
            0,
            "a pad that stays in the graph read FLUSHING after the {stage} surgery. \
             Breakdown: {:?}",
            FcastPlaybin::flow_census_breakdown()
        );
    }
    println!("flow census: {:?}", FcastPlaybin::flow_census_breakdown());
    assert_eq!(
        FcastPlaybin::teardown_descent_stuck(),
        0,
        "a teardown descent blew its budget and was leaked"
    );

    media.release_all();
    first.release_all();
    second.release_all();
    harness.shutdown();
    media.unregister();
    first.unregister();
    second.unregister();
}

/// The boundary that STAYS: a teardown sends its text and decodebin3 pairs,
/// and no pair anywhere straddles a pad deactivation.
///
/// The teardown pairs are what three fuzz seeds pin, and this is the counter's
/// proof that they are under observation. The read happens after the playbin is
/// DROPPED, which is the whole reason the counters are process-global:
/// `Teardown::run` executes from `Inner`'s drop, with no handle left to ask.
#[test]
fn a_teardown_fires_the_teardown_reasons_and_splits_no_pair() {
    init();
    let (harness, media, subs, _id) = play_with_external("flushcensustd", "T");

    let text_before = FcastPlaybin::crate_flush_pairs_for("teardown_text");
    let db3_before = FcastPlaybin::crate_flush_pairs_for("teardown_db3");

    // `stop()` is `FcastPlaybin::teardown`'s boundary: drain the disposals,
    // flush the parked pushes, descend.
    harness.playbin.stop().expect("stop");
    harness.wait_for("the pipeline to reach READY", || {
        matches!(
            harness.playbin.state_summary().0,
            gst::State::Ready | gst::State::Null
        )
    });

    let text_after = FcastPlaybin::crate_flush_pairs_for("teardown_text");
    let db3_after = FcastPlaybin::crate_flush_pairs_for("teardown_db3");
    assert!(
        db3_after > db3_before,
        "a stop sent no `teardown_db3` pair ({db3_before} -> {db3_after}), so the boundary \
         that deliberately stays is not under observation. Breakdown: {}",
        breakdown()
    );
    println!(
        "teardown census: teardown_text {text_before} -> {text_after}, \
         teardown_db3 {db3_before} -> {db3_after}"
    );

    media.release_all();
    subs.release_all();
    harness.shutdown();
    drop(harness);
    media.unregister();
    subs.unregister();

    // The descent bound must not have fired anywhere, and no pair may have split.
    assert_eq!(
        FcastPlaybin::teardown_descent_stuck(),
        0,
        "a teardown descent blew its {:?} budget and was leaked",
        Duration::from_secs(15)
    );
    assert_eq!(
        FcastPlaybin::flush_pair_activity_transitions(),
        0,
        "a flush pair straddled a pad deactivation; at a teardown that is a \
         descent racing the wake in front of it"
    );
}
