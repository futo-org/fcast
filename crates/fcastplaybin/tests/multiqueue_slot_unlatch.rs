//! The slot latch, staged directly: a multiqueue slot latched by a flush pair
//! BELOW it, and what clears it.
//!
//! `dispose_text_branch_on` flushes two pads and both are below decodebin3,
//! the branch's appsink (pair D) and its queue's sink (pair E). A push caught
//! inside the FLUSH_START..STOP window returns `GST_FLOW_FLUSHING` into
//! `gst_single_queue_push_one`, which writes it to `sq->srcresult`
//! (gstmultiqueue.c:2498); from then on the slot's sink chain hands that result
//! back to upstream on every path including success (`:2643`). Our FLUSH_STOP
//! goes to the branch's queue, never to the multiqueue's SINK pad, which is the
//! only event that clears it.
//!
//! This file is the bench that makes that a fact rather than a reading of the
//! source, and it is where the repair was CHOSEN. The rig is a miniature
//! decodebin3, a bin ghosting a multiqueue's slot pads, exactly the geometry
//! `db_output_stream_setup_decoder` builds for a text stream with no decoder to
//! autoplug, fed by hand so every push is a known event:
//!
//! ```text
//!   feed(src pad)  ->  [ db3: sink_0 -> multiqueue -> src_0 ~ text_0 ]  ->  tqueue -> fakesink
//! ```
//!
//! [`the_latch_is_real_and_our_flush_stop_does_not_clear_it`] is the RED
//! anchor: without it, a green un-latch test would prove nothing, because a
//! rig that never latched would "recover" from anything.
//!
//! # The two candidates, and the measurement
//!
//! There are exactly two writes that clear `srcresult`, and both are driven
//! here through the same rig:
//!
//! * `gst_single_queue_flush (flush=FALSE)` reached from a FLUSH_STOP on the
//!   multiqueue's SINK pad (`:2784-2791`),
//!   [`a_flush_stop_at_the_slot_sink_clears_the_latch_and_flushes_downstream`].
//!   It clears the latch AND forwards the event downstream first (`:2787`),
//!   which the test measures rather than asserts away.
//! * src-pad re-activation (`:3020-3028`),
//!   [`re_activating_the_slot_src_pad_clears_the_latch`], which is what
//!   [`Inner::unlatch_db3_slot`] ships and what this test drives THROUGH the
//!   shipped entry point (`FcastPlaybin::unlatch_db3_slot_for_test`) so the
//!   bench and the product cannot drift apart.
//!
//! # The two triggers, and why the repair is not placed on either of them
//!
//! A flush pair is not the only way to put FLUSHING into a slot, and the field
//! capture says it was not the one that did.
//! [`a_join_to_an_unactivated_branch_latches_the_slot_with_no_flush_at_all`] is
//! the other trigger with no flush anywhere in it: a pad that has never been
//! activated is FLUSHING by construction, so LINKING a live slot to a branch
//! that is still coming up latches it just as thoroughly, and the branch
//! coming up a moment later does not revive it. Both tests then call the same
//! repair, which is the argument for placing that repair on the CONSEQUENCE (a
//! latched slot) rather than on any one cause.
//!
//! # The GStreamer warnings this file prints, attributed
//!
//! A run emits ten `Got data flow before segment event` warnings and they are
//! the rig's own geometry, not the repair's. Measured per test, each run alone:
//!
//! | test | warnings | source |
//! |---|---|---|
//! | [`a_flush_stop_at_the_slot_sink_clears_the_latch_and_flushes_downstream`] | 7 | THE DEMONSTRATION. That candidate's whole finding is that a FLUSH_STOP at the slot's sink is forwarded downstream first (`:2787`); the forwarded flush deletes the SEGMENT sticky off every pad it crosses, and the warnings ARE that damage being visible. Silencing them would delete the measurement. |
//! | [`re_activating_the_slot_src_pad_clears_the_latch`] | 3 | `Inner::send_flush_pair`'s own documented damage, `remove_event_by_type (pad, GST_EVENT_SEGMENT)` (gstpad.c:5919), landing on the branch that [`Rig::latch_the_slot`] flushes to stage the latch. All three are on BRANCH pads (`fpb-tqueue-text_0:sink`, `:src`, `fpb-text-sink:sink`); NONE is on the slot or on the multiqueue's sink pad, which the repair never touches. The rig then keeps that branch alive, which the product never does: a flushed branch is on its way to NULL. |
//! | the other five | 0 | none |
//!
//! Both are the same root cause, pinned deterministically by
//! [`the_crate_flush_pair_is_what_takes_the_branch_segment_away`]: a FLUSH_STOP
//! removes the SEGMENT sticky from the pad it lands on (gstpad.c:5919) and
//! nothing replays it on a BRANCH pad, because `flush_db3_sink_pads`'s scope
//! note records that widening the replay was measured wrong. The same cause
//! explains the bursts of three that `external_subtitle_lifecycle` prints on
//! passing tests, 70 per 10 runs before the repair landed and 63 after,
//! with zero heals fired either side, i.e. not ours and not new.
//!
//! The distinction is load-bearing rather than tidy, so it is asserted and not
//! merely written down: [`re_activating_the_slot_src_pad_clears_the_latch`]
//! checks that the slot's src pad has STREAM_START, CAPS and SEGMENT back
//! after the repair and that the multiqueue's SINK pad never lost its own, and
//! [`a_healed_slot_can_still_open_a_stream_on_a_branch_built_afterwards`]
//! builds the branch shape the product actually builds and requires all three
//! to arrive on it. That test emits no warnings at all, which is the evidence
//! that the three above are the rig's geometry and not a hole in the fix.
//!
//! # Verification
//!
//! * Green: no env vars, at any `--test-threads` (see [`init`]).
//! * `FCAST_NO_SLOT_UNLATCH=1`: four of the seven go RED,
//!   [`re_activating_the_slot_src_pad_clears_the_latch`],
//!   [`a_join_to_an_unactivated_branch_latches_the_slot_with_no_flush_at_all`]
//!   and [`a_healed_slot_can_still_open_a_stream_on_a_branch_built_afterwards`]
//!   because the latch stands, and [`a_healthy_slot_is_left_alone`] because the
//!   "left alone" verdict is not even recorded.
//!   [`the_latch_is_real_and_our_flush_stop_does_not_clear_it`],
//!   [`a_flush_stop_at_the_slot_sink_clears_the_latch_and_flushes_downstream`]
//!   and [`the_crate_flush_pair_is_what_takes_the_branch_segment_away`] are
//!   unaffected: they drive gstreamer directly rather than through the crate,
//!   which is what makes them a measurement of GStreamer and not of us.

use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use fcastplaybin::FcastPlaybin;
use gst::prelude::*;

/// How long any "did the streaming thread get there yet" wait is allowed to
/// take. Everything in this file is local pad surgery on a handful of tiny
/// buffers, so a second is already three orders of magnitude of slack.
const SETTLE: Duration = Duration::from_secs(1);

/// The suite asserts on the process-global repair counters
/// ([`FcastPlaybin::slot_unlatches`] and friends) as DELTAS, and four of these
/// seven tests heal a slot on purpose, so overlapping tests inflate each
/// other's numbers and `a_healthy_slot_is_left_alone`, whose whole claim is
/// that its delta is ZERO, reads a sibling's repair as its own. Measured:
/// green 5/5 alone, red under default parallelism on "left: 1, right: 0".
///
/// The `>` deltas in the healing tests are the same disease wearing the other
/// face, a sibling's repair can satisfy them, so they can pass VACUOUSLY,
/// which is worse than a red.
///
/// `init()` therefore hands every test the one lock: a parallel invocation is
/// merely serial now, not red and not falsely green. parking_lot, so a
/// panicking test cannot poison the rest.
fn init() -> parking_lot::MutexGuard<'static, ()> {
    static SERIAL: parking_lot::Mutex<()> = parking_lot::Mutex::new(());
    gst::init().expect("gstreamer init");
    SERIAL.lock()
}

/// Which sticky events a pad still carries.
///
/// The un-latch DEACTIVATES a pad, and deactivation calls gstpad's
/// `remove_events`; whether the repair puts them back is not a detail, it is
/// the difference between a healed slot and a capsless one that this crate's
/// own caps gate will refuse to build a branch on for the life of the item.
fn stickies_on(pad: &gst::Pad) -> Vec<gst::EventType> {
    let mut kinds = Vec::new();
    pad.sticky_events_foreach(|event| {
        kinds.push(event.type_());
        std::ops::ControlFlow::Continue(gst::EventForeachAction::Keep)
    });
    kinds
}

/// The miniature decodebin3 text output, plus the branch the crate hangs off
/// it.
struct Rig {
    pipeline: gst::Pipeline,
    /// The pad this test pushes from, standing in for parsebin's output.
    feed: gst::Pad,
    /// The bin's ghost src pad: the `text_%u` a `RoutedStream` would hold.
    db3_src_pad: gst::Pad,
    /// The multiqueue src pad the ghost targets, the slot itself.
    slot_src: gst::Pad,
    /// The multiqueue sink pad, whose `last_flow_result` is the latch read
    /// from the upstream side.
    slot_sink: gst::Pad,
    /// The branch's queue sink pad: what a disposal's pair E flushes.
    tqueue_sink: gst::Pad,
    tqueue: gst::Element,
    tail: gst::Element,
    /// Buffers that made it all the way through.
    delivered: Arc<AtomicUsize>,
    next: u64,
}

/// Whether the branch is up at the moment the slot is linked to it. The two
/// arms are the two trigger classes this file measures.
#[derive(Clone, Copy, PartialEq, Eq)]
enum BranchAtJoin {
    /// The driver's intent: elements synced, pads active, THEN the link.
    Up,
    /// The field's join: linked while the branch is still at NULL, so its sink
    /// pad is flushing for want of ever having been activated.
    Null,
}

impl Rig {
    /// The healthy rig: the branch is up before anything is pushed, which is
    /// what the driver's join intends (`sync_state_with_parent` on the tail and
    /// the queue, THEN the upstream link).
    fn new(name: &str) -> Self {
        let mut rig = Self::build(name, BranchAtJoin::Up);
        rig.open_the_stream();
        // The priming buffer, and it is an assertion rather than a warm-up:
        // every claim below is "this rig stopped delivering", which is only a
        // measurement once the rig has been seen delivering.
        assert_eq!(
            rig.push(),
            Ok(gst::FlowSuccess::Ok),
            "the rig refused its priming buffer"
        );
        rig
    }

    /// The JOIN-WINDOW rig: the slot is linked to a branch whose elements are
    /// still at NULL, so the branch's sink pad is FLUSHING for the ordinary
    /// reason that it has never been activated (gstpad.c:441).
    ///
    /// No priming push: the point is that the FIRST thing through the link is
    /// the thing that dies.
    fn new_with_a_branch_left_at_null(name: &str) -> Self {
        let mut rig = Self::build(name, BranchAtJoin::Null);
        rig.open_the_stream();
        rig
    }

    fn build(name: &str, branch: BranchAtJoin) -> Self {
        let pipeline = gst::Pipeline::with_name(name);

        // The miniature decodebin3. A real one ghosts `text_%u` straight at
        // the multiqueue src pad when no decoder is needed
        // (`output->decoder_src = slot->src_pad`), and that identity is the
        // whole premise of `Inner::multiqueue_slot_behind`.
        let db3 = gst::Bin::with_name("fpb-decodebin");
        let mq = gst::ElementFactory::make("multiqueue")
            .name("multiqueue0")
            .build()
            .expect("multiqueue");
        db3.add(&mq).expect("adding the multiqueue");
        let slot_sink = mq.request_pad_simple("sink_%u").expect("a multiqueue slot");
        let slot_src = mq
            .static_pad(&slot_sink.name().replace("sink", "src"))
            .expect("the slot's src pad");
        let ghost_sink = gst::GhostPad::builder_with_target(&slot_sink)
            .expect("ghosting the slot sink")
            .name("sink_0")
            .build();
        let ghost_src = gst::GhostPad::builder_with_target(&slot_src)
            .expect("ghosting the slot src")
            .name("text_0")
            .build();
        db3.add_pad(&ghost_sink).expect("adding the ghost sink");
        db3.add_pad(&ghost_src).expect("adding the ghost src");

        // The branch the crate builds and disposes of.
        let tqueue = gst::ElementFactory::make("queue")
            .name("fpb-tqueue-text_0")
            .build()
            .expect("queue");
        let sink = gst::ElementFactory::make("fakesink")
            .name("fpb-text-sink")
            .property("sync", false)
            .property("async", false)
            .build()
            .expect("fakesink");

        pipeline
            .add_many([db3.upcast_ref::<gst::Element>(), &tqueue, &sink])
            .expect("adding the elements");
        tqueue.link(&sink).expect("linking the branch to its tail");
        if branch == BranchAtJoin::Null {
            // The slot goes to PLAYING WITHOUT the branch, which is the state
            // the driver's join finds itself in when it links a tail whose
            // `sync_state_with_parent` has not taken effect yet. `gst_bin_add`
            // does not activate anything; only READY->PAUSED does.
            db3.set_locked_state(false);
            tqueue.set_locked_state(true);
            sink.set_locked_state(true);
        }
        ghost_src
            .link(&tqueue.static_pad("sink").expect("the queue's sink"))
            .expect("linking the ghost to the branch");

        let delivered = Arc::new(AtomicUsize::new(0));
        let counter = Arc::clone(&delivered);
        sink.static_pad("sink")
            .expect("the tail's sink")
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                counter.fetch_add(1, Ordering::SeqCst);
                gst::PadProbeReturn::Ok
            })
            .expect("the delivery probe");

        // The feed. A parentless pad is enough to push from and keeps every
        // buffer in this test an explicit act.
        let feed = gst::Pad::builder(gst::PadDirection::Src)
            .name("feed")
            .build();
        feed.set_active(true).expect("activating the feed");
        feed.link(&ghost_sink).expect("linking the feed");

        pipeline
            .set_state(gst::State::Playing)
            .expect("the rig to reach PLAYING");

        Self {
            pipeline,
            feed,
            db3_src_pad: ghost_src.upcast(),
            slot_src,
            slot_sink,
            tqueue_sink: tqueue.static_pad("sink").expect("the queue's sink"),
            tqueue,
            tail: sink,
            delivered,
            next: 0,
        }
    }

    /// Bring a branch that was left at NULL up, the way the driver's
    /// `sync_state_with_parent` eventually does.
    fn bring_the_branch_up(&self) {
        self.tail.set_locked_state(false);
        self.tqueue.set_locked_state(false);
        self.tail
            .sync_state_with_parent()
            .expect("the tail to come up");
        self.tqueue
            .sync_state_with_parent()
            .expect("the queue to come up");
        assert!(
            self.tqueue_sink.is_active(),
            "the branch's sink pad is still inactive after it was brought up"
        );
    }

    /// The three stickies a multiqueue needs before it will carry anything.
    fn open_the_stream(&mut self) {
        assert!(
            self.feed
                .push_event(gst::event::StreamStart::new("slot-unlatch")),
            "stream-start refused"
        );
        assert!(
            self.feed.push_event(gst::event::Caps::new(
                &gst::Caps::builder("text/x-raw")
                    .field("format", "utf8")
                    .build()
            )),
            "caps refused"
        );
        assert!(
            self.feed
                .push_event(gst::event::Segment::new(&gst::FormattedSegment::<
                    gst::ClockTime,
                >::new())),
            "segment refused"
        );
    }

    /// Push one buffer and report what the SLOT'S SINK handed back, which is
    /// `sq->srcresult` verbatim (gstmultiqueue.c:2643), i.e. the latch read
    /// from where upstream reads it.
    fn push(&mut self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut buffer = gst::Buffer::from_slice(format!("cue {}", self.next).into_bytes());
        buffer
            .get_mut()
            .expect("a fresh buffer is writable")
            .set_pts(gst::ClockTime::from_mseconds(self.next * 100));
        self.next += 1;
        self.feed.push(buffer)
    }

    /// Swap the branch for a brand new one, the way a later
    /// `poll_text_policy` builds a branch onto a slot whose old one is gone.
    ///
    /// Returns the new branch's sink pad, so a test can ask what the slot
    /// handed it. This is the PRODUCTION geometry for the sticky question: the
    /// slot must still be able to open a stream on a branch that has never
    /// seen one.
    fn relink_a_fresh_branch(&mut self, name: &str) -> gst::Pad {
        if let Some(peer) = self.db3_src_pad.peer() {
            let _ = self.db3_src_pad.unlink(&peer);
        }
        let tqueue = gst::ElementFactory::make("queue")
            .name(format!("fpb-tqueue-{name}"))
            .build()
            .expect("queue");
        let tail = gst::ElementFactory::make("fakesink")
            .name(format!("fpb-text-sink-{name}"))
            .property("sync", false)
            .property("async", false)
            .build()
            .expect("fakesink");
        self.pipeline
            .add_many([&tqueue, &tail])
            .expect("adding the fresh branch");
        tqueue.link(&tail).expect("linking the fresh branch");
        let counter = Arc::clone(&self.delivered);
        tail.static_pad("sink")
            .expect("the fresh tail's sink")
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                counter.fetch_add(1, Ordering::SeqCst);
                gst::PadProbeReturn::Ok
            })
            .expect("the fresh delivery probe");
        // Up BEFORE the link, which is what the driver's join intends and what
        // `Inner::joins_into_an_inactive_branch` exists to catch it failing to
        // do.
        tail.sync_state_with_parent().expect("the fresh tail up");
        tqueue.sync_state_with_parent().expect("the fresh queue up");
        let entry = tqueue.static_pad("sink").expect("the fresh queue's sink");
        self.db3_src_pad
            .link(&entry)
            .expect("linking the slot to the fresh branch");
        self.tqueue_sink = entry.clone();
        self.tqueue = tqueue;
        self.tail = tail;
        entry
    }

    /// Wait for `count` buffers to reach the tail, or give up.
    fn wait_for_delivery(&self, count: usize) -> bool {
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            if self.delivered.load(Ordering::SeqCst) >= count {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.delivered.load(Ordering::SeqCst) >= count
    }

    /// Wait for the slot to latch, which happens on the multiqueue's own loop
    /// thread some time after the pair lands.
    fn wait_for_the_latch(&self) -> bool {
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            if self.slot_src.is_active()
                && matches!(
                    self.slot_src.last_flow_result(),
                    Err(gst::FlowError::Flushing)
                )
            {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// THE STAGING: a disposal's flush pair on the branch's queue, with a push
    /// in flight, and the buffer that the pair therefore kills.
    ///
    /// Bit-for-bit what `Inner::send_flush_pair` sends, by hand rather than
    /// through the crate, so the rig owns its own timing: the FLUSH_START, the
    /// push it catches, and only then the FLUSH_STOP.
    fn latch_the_slot(&mut self) {
        assert!(
            self.wait_for_delivery(1),
            "the rig never delivered its first buffer, so nothing below is a measurement"
        );
        let before = self.delivered.load(Ordering::SeqCst);

        assert!(
            self.tqueue_sink.send_event(gst::event::FlushStart::new()),
            "FLUSH_START refused by the branch's queue"
        );
        // The push the window catches. Its own return may still be OK, the
        // slot's sink chain reads `srcresult` BEFORE the loop thread has
        // pushed it downstream, which is exactly why the latch is read off
        // the src pad below and not off this call.
        let _ = self.push();
        assert!(
            self.wait_for_the_latch(),
            "the slot never latched; the rig cannot measure an un-latch it did not stage \
             (slot src last flow: {:?})",
            self.slot_src.last_flow_result()
        );
        assert!(
            self.tqueue_sink
                .send_event(gst::event::FlushStop::new(false)),
            "FLUSH_STOP refused by the branch's queue"
        );
        assert_eq!(
            self.delivered.load(Ordering::SeqCst),
            before,
            "the caught buffer must be lost; if it arrived, the window missed it"
        );
    }

    /// Is the branch below the slot healthy? The un-latch must not be credited
    /// for a rig whose queue is still flushing.
    fn branch_is_ready(&self) -> bool {
        !self
            .tqueue_sink
            .pad_flags()
            .contains(gst::PadFlags::FLUSHING)
    }

    fn shutdown(self) {
        let _ = self.feed.set_active(false);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// C8b's rig: the same miniature decodebin3, parked exactly as
/// `Inner::park_text_stream` parks a routed text stream, against a pipeline
/// that has NOT reached PLAYING.
///
/// The state is the subject. A park is created during a bring-up, so it is
/// synced to a parent at (or heading for) PAUSED, and PAUSED is where basesink
/// preroll-blocks. A rig at PLAYING would never reproduce it.
struct ParkRig {
    pipeline: gst::Pipeline,
    feed: gst::Pad,
    db3_src_pad: gst::Pad,
    park: Option<gst::Element>,
    park_pad: Option<gst::Pad>,
    unpark: Unpark,
    /// Cues the park CONSUMED, counted from both callbacks exactly as
    /// `park_text_stream` keeps them (a preroll is a keep too).
    parked: Arc<AtomicUsize>,
    /// Buffers that reached the joined branch's tail.
    delivered: Arc<AtomicUsize>,
    tail: Option<gst::Element>,
    tqueue: Option<gst::Element>,
    next: u64,
}

/// How the park is REMOVED, which is the entire subject of the two park tests.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Unpark {
    /// [`Inner::retire_parking_sink`] through its shipped test entry point,
    /// lever and all.
    Shipped,
    /// The pre-C8b unpark, by hand: straight to NULL. Staged rather than
    /// levered so the mechanism it demonstrates is recorded permanently,
    /// whatever the crate does later.
    StraightToNull,
}

impl ParkRig {
    fn new(name: &str) -> Self {
        Self::build(name, Unpark::Shipped)
    }

    fn new_unparked_straight_to_null(name: &str) -> Self {
        Self::build(name, Unpark::StraightToNull)
    }

    fn build(name: &str, unpark: Unpark) -> Self {
        let pipeline = gst::Pipeline::with_name(name);

        let db3 = gst::Bin::with_name("fpb-decodebin");
        let mq = gst::ElementFactory::make("multiqueue")
            .name("multiqueue0")
            .build()
            .expect("multiqueue");
        db3.add(&mq).expect("adding the multiqueue");
        let slot_sink = mq.request_pad_simple("sink_%u").expect("a multiqueue slot");
        let slot_src = mq
            .static_pad(&slot_sink.name().replace("sink", "src"))
            .expect("the slot's src pad");
        let ghost_sink = gst::GhostPad::builder_with_target(&slot_sink)
            .expect("ghosting the slot sink")
            .name("sink_0")
            .build();
        let ghost_src = gst::GhostPad::builder_with_target(&slot_src)
            .expect("ghosting the slot src")
            .name("text_0")
            .build();
        db3.add_pad(&ghost_sink).expect("adding the ghost sink");
        db3.add_pad(&ghost_src).expect("adding the ghost src");
        pipeline
            .add(db3.upcast_ref::<gst::Element>())
            .expect("adding the decodebin");

        // THE PARK, property for property what `Inner::park_text_stream`
        // builds. Nothing here is incidental: `sync=false`/`async=false` are
        // the properties that were BELIEVED to make it wait-free, and the
        // point of the rig is that they do not.
        let parked = Arc::new(AtomicUsize::new(0));
        let park = gst_app::AppSink::builder()
            .name("fpb-textpark-text_0")
            .sync(false)
            .async_(false)
            .drop(true)
            .max_buffers(64)
            .enable_last_sample(false)
            .build();
        park.unset_element_flags(gst::ElementFlags::SINK);
        let sample_count = Arc::clone(&parked);
        let preroll_count = Arc::clone(&parked);
        park.set_callbacks(
            gst_app::AppSinkCallbacks::builder()
                .new_sample(move |sink| {
                    if sink.try_pull_sample(gst::ClockTime::ZERO).is_some() {
                        sample_count.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .new_preroll(move |sink| {
                    if sink.try_pull_preroll(gst::ClockTime::ZERO).is_some() {
                        preroll_count.fetch_add(1, Ordering::SeqCst);
                    }
                    Ok(gst::FlowSuccess::Ok)
                })
                .build(),
        );
        let park: gst::Element = park.upcast();

        // PAUSED, not PLAYING: the bring-up state the field parks in.
        pipeline
            .set_state(gst::State::Paused)
            .expect("the rig to reach PAUSED");

        pipeline.add(&park).expect("adding the park");
        park.sync_state_with_parent().expect("syncing the park");
        assert_eq!(
            park.current_state(),
            gst::State::Paused,
            "the park must actually be at PAUSED, or none of this demonstrates anything"
        );
        let park_pad = park.static_pad("sink").expect("appsink has a sink pad");
        ghost_src
            .link(&park_pad)
            .expect("linking text into its park");

        let feed = gst::Pad::builder(gst::PadDirection::Src)
            .name("feed")
            .build();
        feed.set_active(true).expect("activating the feed");
        feed.link(&ghost_sink).expect("linking the feed");

        let mut rig = Self {
            pipeline,
            feed,
            db3_src_pad: ghost_src.upcast(),
            park: Some(park),
            park_pad: Some(park_pad),
            unpark,
            parked,
            delivered: Arc::new(AtomicUsize::new(0)),
            tail: None,
            tqueue: None,
            next: 0,
        };
        rig.open_the_stream();
        rig
    }

    fn open_the_stream(&mut self) {
        assert!(
            self.feed
                .push_event(gst::event::StreamStart::new("waitfree-park")),
            "stream-start refused"
        );
        assert!(
            self.feed.push_event(gst::event::Caps::new(
                &gst::Caps::builder("text/x-raw")
                    .field("format", "utf8")
                    .build()
            )),
            "caps refused"
        );
        let segment = gst::FormattedSegment::<gst::ClockTime>::new();
        assert!(
            self.feed.push_event(gst::event::Segment::new(&segment)),
            "segment refused"
        );
    }

    fn push(&mut self) -> Result<gst::FlowSuccess, gst::FlowError> {
        let mut buffer = gst::Buffer::from_slice(format!("cue {}", self.next).into_bytes());
        buffer
            .get_mut()
            .expect("a fresh buffer is writable")
            .set_pts(gst::ClockTime::from_mseconds(self.next * 100));
        self.next += 1;
        self.feed.push(buffer)
    }

    fn wait_for_park(&self, count: usize) -> bool {
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            if self.parked.load(Ordering::SeqCst) >= count {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.parked.load(Ordering::SeqCst) >= count
    }

    fn wait_for_delivery(&self, count: usize) -> bool {
        let deadline = Instant::now() + SETTLE;
        while Instant::now() < deadline {
            if self.delivered.load(Ordering::SeqCst) >= count {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        self.delivered.load(Ordering::SeqCst) >= count
    }

    /// The unlink half of `Inner::unpark_stream_for_join`. The sink stays in
    /// the pipeline, still holding whatever push is parked inside its preroll.
    fn unlink_the_park(&mut self) {
        if let Some(pad) = self.park_pad.take() {
            let _ = self.db3_src_pad.unlink(&pad);
        }
    }

    /// The retirement half. `Unpark::Shipped` drives
    /// `Inner::retire_parking_sink` through its test entry point, lever and
    /// all, so the rig cannot drift from the product;
    /// `Unpark::StraightToNull` is what the crate did before C8b.
    fn retire_the_park(&mut self) {
        if let Some(sink) = self.park.take() {
            match self.unpark {
                Unpark::Shipped => FcastPlaybin::retire_parking_sink_for_test(&sink),
                Unpark::StraightToNull => {
                    let _ = sink.set_state(gst::State::Null);
                }
            }
            let _ = self.pipeline.remove(&sink);
        }
    }

    /// The pre-C8b sequence: both halves before the branch exists.
    fn unpark(&mut self) {
        self.unlink_the_park();
        self.retire_the_park();
    }

    /// Buffering finishes and the item starts, which is the only thing that
    /// releases the JOINED branch's own tail: a fakesink at PAUSED prerolls one
    /// buffer and blocks the rest exactly as the park did (that is the whole
    /// subject of this file), so a delivery count taken before this reports the
    /// branch's preroll, not the slot's backlog.
    fn play(&self) {
        self.pipeline
            .set_state(gst::State::Playing)
            .expect("the rig to reach PLAYING");
    }

    /// The join `poll_text_policy` performs: queue plus tail, both up, then the
    /// upstream link.
    fn join(&mut self) -> gst::Pad {
        let tqueue = gst::ElementFactory::make("queue")
            .name("fpb-tqueue-text_0")
            .build()
            .expect("queue");
        let tail = gst::ElementFactory::make("fakesink")
            .name("fpb-text-sink")
            .property("sync", false)
            .property("async", false)
            .build()
            .expect("fakesink");
        self.pipeline
            .add_many([&tqueue, &tail])
            .expect("adding the branch");
        tqueue.link(&tail).expect("linking the branch");
        let counter = Arc::clone(&self.delivered);
        tail.static_pad("sink")
            .expect("the tail's sink")
            .add_probe(gst::PadProbeType::BUFFER, move |_pad, _info| {
                counter.fetch_add(1, Ordering::SeqCst);
                gst::PadProbeReturn::Ok
            })
            .expect("the delivery probe");
        tail.sync_state_with_parent().expect("the tail up");
        tqueue.sync_state_with_parent().expect("the queue up");
        let entry = tqueue.static_pad("sink").expect("the queue's sink");
        self.db3_src_pad
            .link(&entry)
            .expect("linking the slot to the branch");
        self.tqueue = Some(tqueue);
        self.tail = Some(tail);
        entry
    }

    fn shutdown(self) {
        let _ = self.feed.set_active(false);
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}

/// RED ANCHOR: the latch is permanent, and the crate's own FLUSH_STOP, which
/// goes to the BRANCH, one element below the slot, does not touch it.
///
/// Everything else in this file is a recovery claim, and a recovery claim
/// against a rig that never broke is worthless. This is the test that says the
/// rig breaks.
#[test]
fn the_latch_is_real_and_our_flush_stop_does_not_clear_it() {
    let _serial = init();
    let mut rig = Rig::new("latch-is-real");
    rig.latch_the_slot();

    assert!(
        rig.branch_is_ready(),
        "the branch's queue is still flushing, so the next push proves nothing about the slot"
    );
    let before = rig.delivered.load(Ordering::SeqCst);
    let flow = rig.push();

    assert_eq!(
        flow,
        Err(gst::FlowError::Flushing),
        "the slot's sink must hand FLUSHING back to upstream on every later push \
         (gstmultiqueue.c:2643), this is the return adaptivedemux2 discards a whole \
         whole-period text track on"
    );
    assert!(
        !rig.wait_for_delivery(before + 1),
        "a latched slot must deliver nothing; the branch below it is healthy and the pair \
         is long over, and it still delivers nothing, that is the dead track"
    );
    rig.shutdown();
}

/// The first un-latch: a FLUSH_STOP that reaches the multiqueue's SINK pad.
///
/// It works, and this test also records the reason it is NOT what ships.
/// `gst_multi_queue_sink_event` FORWARDS the event downstream before touching
/// the slot (gstmultiqueue.c:2787), so the repair travels into whatever is
/// hanging off the slot's src pad at that moment. In the rig that is visible
/// as the branch's queue being flushed by a repair aimed at the slot; in the
/// product that branch can be the INCOMING track's (decodebin3 recycles text
/// outputs in both directions) and the repair would throw away the cues
/// of the track that is still alive.
///
/// THE SEVEN `Got data flow before segment event` WARNINGS THIS TEST PRINTS
/// ARE THE FINDING. The forwarded FLUSH_STOP deletes the SEGMENT sticky off
/// every pad it crosses on its way down the branch, and the warnings are the
/// pads afterwards saying so. Do not silence them: a quiet version of this
/// test would assert the flush count and lose the reason the count matters.
#[test]
fn a_flush_stop_at_the_slot_sink_clears_the_latch_and_flushes_downstream() {
    let _serial = init();
    let mut rig = Rig::new("flush-stop-candidate");
    rig.latch_the_slot();

    // Something to lose: the branch's queue is healthy and holding data that
    // a downstream-forwarded flush would drop. `queue` will not hand it on
    // until its own task runs, which is why this is asserted as "delivered
    // before the repair" rather than "queued".
    assert!(
        rig.branch_is_ready(),
        "the branch must be healthy before the candidate runs"
    );

    let flushed_downstream = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&flushed_downstream);
    rig.tqueue_sink
        // EVENT_FLUSH, not EVENT_DOWNSTREAM: a flush is not a serialized
        // downstream event and an EVENT_DOWNSTREAM probe never sees one. The
        // first version of this measurement made that mistake and read the
        // forwarding as absent.
        .add_probe(gst::PadProbeType::EVENT_FLUSH, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data
                && event.type_() == gst::EventType::FlushStop
            {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            gst::PadProbeReturn::Ok
        })
        .expect("the downstream-flush probe");

    assert!(
        rig.slot_sink.send_event(gst::event::FlushStop::new(false)),
        "FLUSH_STOP refused by the multiqueue's sink pad"
    );

    let before = rig.delivered.load(Ordering::SeqCst);
    assert_eq!(
        rig.push(),
        Ok(gst::FlowSuccess::Ok),
        "the slot must accept data again after a FLUSH_STOP at its sink"
    );
    assert!(
        rig.wait_for_delivery(before + 1),
        "the buffer after the un-latch never arrived"
    );

    assert_eq!(
        flushed_downstream.load(Ordering::SeqCst),
        1,
        "THE COST, measured rather than assumed: the repair is forwarded downstream into \
         the branch before the slot is touched (gstmultiqueue.c:2787). That branch is the \
         reason this candidate lost"
    );
    rig.shutdown();
}

/// The second un-latch, and the one that ships: re-activating the slot's SRC
/// pad (`gst_multi_queue_src_activate_mode`, gstmultiqueue.c:3020-3028).
///
/// Driven through `FcastPlaybin::unlatch_db3_slot_for_test`, i.e. through the
/// SHIPPED `Inner::unlatch_db3_slot`, from the same handle the product has: the
/// decodebin3 output ghost pad a `RoutedStream` holds. That covers the target
/// resolution (ghost -> multiqueue src pad), the latched/clean gate, and the
/// re-activation itself in one go.
#[test]
fn re_activating_the_slot_src_pad_clears_the_latch() {
    let _serial = init();
    let mut rig = Rig::new("reactivation-candidate");
    rig.latch_the_slot();

    let repairs_before = FcastPlaybin::slot_unlatches();
    let flushed_downstream = Arc::new(AtomicUsize::new(0));
    let counter = Arc::clone(&flushed_downstream);
    rig.tqueue_sink
        // EVENT_FLUSH, not EVENT_DOWNSTREAM: a flush is not a serialized
        // downstream event and an EVENT_DOWNSTREAM probe never sees one. The
        // first version of this measurement made that mistake and read the
        // forwarding as absent.
        .add_probe(gst::PadProbeType::EVENT_FLUSH, move |_pad, info| {
            if let Some(gst::PadProbeData::Event(event)) = &info.data
                && event.type_() == gst::EventType::FlushStop
            {
                counter.fetch_add(1, Ordering::SeqCst);
            }
            gst::PadProbeReturn::Ok
        })
        .expect("the downstream-flush probe");

    FcastPlaybin::unlatch_db3_slot_for_test(&rig.db3_src_pad);

    assert_eq!(
        FcastPlaybin::slot_unlatch_failures(),
        0,
        "the re-activation must not fail"
    );
    assert!(
        FcastPlaybin::slot_unlatches() > repairs_before,
        "the repair must have RUN, not been skipped as clean, the rig staged a real latch \
         (clean: {})",
        FcastPlaybin::slot_unlatch_clean()
    );

    let before = rig.delivered.load(Ordering::SeqCst);
    assert_eq!(
        rig.push(),
        Ok(gst::FlowSuccess::Ok),
        "the slot must accept data again after its src pad is re-activated"
    );
    assert!(
        rig.wait_for_delivery(before + 1),
        "the buffer after the repair never arrived: the slot is still latched"
    );
    assert_eq!(
        flushed_downstream.load(Ordering::SeqCst),
        0,
        "THE WHOLE REASON THIS CANDIDATE WON: re-activation pushes no event anywhere, so a \
         branch hanging off the slot, in the product, possibly the INCOMING track's, is \
         untouched by the repair"
    );

    // THE STICKY RESTORE, asserted rather than inferred from a suite that
    // happened to stay green.
    //
    // Re-activation runs gstpad's `remove_events` on the way down, which
    // destroys the pad's STREAM_START, CAPS and SEGMENT. Upstream re-pushes
    // them on its next buffer, for a stream that HAS a next buffer. A
    // whole-period text Representation does not, so a repair that clears the
    // caps trades a latched slot for a capsless one and this crate's caps gate
    // then refuses to build a branch on it for the whole item. Shipping
    // without the snapshot-and-restore turned
    // `sink_subtitles::a_paused_disposal_frees_the_branch_for_the_next_link`
    // red on exactly that, and this is the assertion that would have caught it
    // here instead.
    let restored = stickies_on(&rig.slot_src);
    for wanted in [
        gst::EventType::StreamStart,
        gst::EventType::Caps,
        gst::EventType::Segment,
    ] {
        assert!(
            restored.contains(&wanted),
            "the repair did not put {wanted:?} back on the slot's src pad ({restored:?})"
        );
    }
    // And the SINK side is untouched, which is the other half of "this
    // candidate is contained": the repair never goes near the pad upstream
    // pushes into, so nothing there had to be restored in the first place.
    let upstream = stickies_on(&rig.slot_sink);
    assert!(
        upstream.contains(&gst::EventType::Segment),
        "the multiqueue's SINK pad lost its segment to a repair that should not touch it \
         ({upstream:?})"
    );
    rig.shutdown();
}

/// The production consequence of the sticky restore: a slot that has been
/// healed can still OPEN A STREAM on a branch built after the fact.
///
/// This is the whole-period case stated as a test. The demuxer pushed the
/// entire track once and will never push again, so every event that branch will
/// ever see has to come out of the slot's own sticky store, and the repair
/// deactivates the pad that holds it. If the restore were cosmetic (the pad
/// reads the events back but no longer forwards them) the slot would be healed
/// and the track still dead, which no counter in this crate would report.
///
/// It also pins the ATTRIBUTION of this file's remaining GStreamer warnings.
/// `re_activating_the_slot_src_pad_clears_the_latch` emits three "Got data flow
/// before segment event" lines, all on BRANCH pads (`fpb-tqueue-text_0:sink`,
/// `:src`, `fpb-text-sink:sink`) and none on the slot or the multiqueue's sink.
/// They are not the repair: they are `Inner::send_flush_pair`'s own documented
/// damage, `remove_event_by_type (pad, GST_EVENT_SEGMENT)` (gstpad.c:5919),
/// landing on a branch that the RIG then keeps alive. The product never does,
/// a flushed branch is on its way to NULL, which is why the fresh branch
/// below, the shape the product actually builds, comes up clean.
#[test]
fn a_healed_slot_can_still_open_a_stream_on_a_branch_built_afterwards() {
    let _serial = init();
    let mut rig = Rig::new("healed-then-relinked");
    rig.latch_the_slot();
    FcastPlaybin::unlatch_db3_slot_for_test(&rig.db3_src_pad);

    // The branch the driver builds next, onto the slot it just repaired.
    let entry = rig.relink_a_fresh_branch("afterwards");
    let before = rig.delivered.load(Ordering::SeqCst);
    assert_eq!(
        rig.push(),
        Ok(gst::FlowSuccess::Ok),
        "the healed slot refused data for the fresh branch"
    );
    assert!(
        rig.wait_for_delivery(before + 1),
        "the fresh branch never received the buffer"
    );

    let opened = stickies_on(&entry);
    for wanted in [
        gst::EventType::StreamStart,
        gst::EventType::Caps,
        gst::EventType::Segment,
    ] {
        assert!(
            opened.contains(&wanted),
            "the healed slot delivered data to a fresh branch WITHOUT {wanted:?} \
             ({opened:?}): the restore reads back but does not forward, so the track is \
             timed against nothing"
        );
    }
    rig.shutdown();
}

/// THE JOIN WINDOW: the same latch with no flush anywhere near it.
///
/// The corollary, mirrored. A pad that has never been activated is FLUSHING by
/// construction, gstpad sets it at construction (gstpad.c:441) and clears it
/// on activation, so linking a slot to a branch whose elements are still at
/// NULL and letting one push through latches that slot exactly as a flush pair
/// would. No disposal, no flush, no surgery: a LINK, and the first thing that
/// crosses it.
///
/// This is the shape the field capture forces. The text branch joined its
/// consumer tail at 35.093, mid state-transition; the window that follows is
/// silent, no disposal, no flush, no policy job, no seek, and the demuxer's
/// discard does not appear until 41.307, because on a whole-period text
/// representation nothing pushes through the slot in between. The discard is
/// not the moment of death. It is the first push through a slot that has been
/// dead since the join.
///
/// The repair does not care which of the two put the FLUSHING there, which is
/// the argument for placing it where a JOIN can reach it and not only where a
/// disposal can.
#[test]
fn a_join_to_an_unactivated_branch_latches_the_slot_with_no_flush_at_all() {
    let _serial = init();
    let mut rig = Rig::new_with_a_branch_left_at_null("join-window");

    // Not one flush event exists in this test. The only thing that happened is
    // a link.
    let before = rig.delivered.load(Ordering::SeqCst);
    let _ = rig.push();
    assert!(
        rig.wait_for_the_latch(),
        "the slot did not latch on a join to an inactive branch (slot src last flow: {:?})",
        rig.slot_src.last_flow_result()
    );

    // And now the branch comes up, exactly as the driver's
    // `sync_state_with_parent` would have brought it up a moment later. The
    // branch is healthy; the slot above it is not, and nothing in the graph
    // will ever say so again.
    rig.bring_the_branch_up();
    assert!(
        rig.branch_is_ready(),
        "the branch must be healthy before the claim below"
    );
    assert_eq!(
        rig.push(),
        Err(gst::FlowError::Flushing),
        "the slot must still hand FLUSHING back to upstream even though the branch under it \
         is now up: THIS is the dead track, and no later push can revive it"
    );
    assert!(
        !rig.wait_for_delivery(before + 1),
        "a slot latched at the join must deliver nothing afterwards"
    );

    // The same repair, aimed at the same slot, for a completely different
    // trigger.
    let repairs_before = FcastPlaybin::slot_unlatches();
    FcastPlaybin::unlatch_db3_slot_for_test(&rig.db3_src_pad);
    assert!(
        FcastPlaybin::slot_unlatches() > repairs_before,
        "the repair must have run against a join-latched slot too"
    );
    let before = rig.delivered.load(Ordering::SeqCst);
    assert_eq!(
        rig.push(),
        Ok(gst::FlowSuccess::Ok),
        "the slot must accept data again"
    );
    assert!(
        rig.wait_for_delivery(before + 1),
        "the buffer after the repair never arrived"
    );
    rig.shutdown();
}

/// THE PINNED CAUSE of every `Got data flow before segment event` this crate's
/// text suites print: a flush pair takes the SEGMENT off the pad it lands on,
/// and nothing puts it back on a BRANCH pad.
///
/// # Why this is a test and not a comment
///
/// `external_subtitle_lifecycle` prints these in bursts of three
/// (`fpb-tqueue-text_0:sink`, `:src`, `fpb-textsink-text_0:sink`) several times
/// a run, on PASSING tests, and they have been read as new damage more than
/// once. They are neither new nor damage: `gst_pad_send_event_unchecked` runs
/// `remove_event_by_type (pad, GST_EVENT_SEGMENT)` on FLUSH_STOP
/// (gstpad.c:5919), so the branch is segmentless until upstream sends another
/// one, and the `ftestsrc` harness pushes again immediately, where a real
/// source re-segments first. Measured across the repair landing: 70 warnings
/// per 10 runs before it, 63 after, and zero heals fired in any of them.
///
/// # And widening the segment restore to cover branch pads is REFUSED, measured
///
/// `Inner::flush_db3_sink_pads` replays the sticky the pair removes, and its
/// SCOPE note records what happened when that was tried everywhere: THIS suite
/// went to "16 passed / 3 failed in 127 s against 19 passed in 8 s, all three
/// on 'no FSTA cue reached the renderer': a restored segment is stale where a
/// branch is about to be re-linked or released." So the branch pads keep the
/// bare pair on purpose, and these warnings are the visible price of that
/// decision.
///
/// Pinning the CAUSE rather than a count, deliberately. The count is timing
/// noise (3 to 18 per run of that suite, either side of the repair); the
/// mechanism is deterministic. If someone later teaches the crate to replay a
/// branch pad's segment, this assertion flips and the warnings should be gone,
/// which is exactly when somebody wants to be told.
#[test]
fn the_crate_flush_pair_is_what_takes_the_branch_segment_away() {
    let _serial = init();
    let mut rig = Rig::new("segment-warnings");
    assert!(
        rig.wait_for_delivery(1),
        "the rig never delivered its first buffer"
    );
    assert!(
        stickies_on(&rig.tqueue_sink).contains(&gst::EventType::Segment),
        "the branch had no segment to lose, so this test proves nothing"
    );

    // Bit-for-bit `Inner::send_flush_pair`, which is what a disposal sends.
    assert!(rig.tqueue_sink.send_event(gst::event::FlushStart::new()));
    assert!(
        rig.tqueue_sink
            .send_event(gst::event::FlushStop::new(false))
    );

    let after = stickies_on(&rig.tqueue_sink);
    assert!(
        !after.contains(&gst::EventType::Segment),
        "FLUSH_STOP no longer removes the branch pad's SEGMENT ({after:?}). If that is \
         because the crate now replays it, the text suites' 'Got data flow before segment \
         event' bursts should be gone and this test should be retired, check, do not just \
         delete the assertion"
    );
    // STREAM_START and CAPS survive: only the segment is taken, which is why
    // the branch keeps flowing and merely loses its timeline.
    assert!(
        after.contains(&gst::EventType::Caps),
        "the pair took the CAPS too, which would be a different and much louder defect \
         ({after:?})"
    );
    rig.shutdown();
}

/// C8b, THE QUEUE-CONTENT TRAP: what a latched slot is HOLDING when the repair
/// arrives does not survive it.
///
/// The repair's own doc has always said it is not free: it drops what the slot
/// holds. This is the size of that price, measured, because a captured failure
/// turned it from a footnote into the whole defect: a slot latched during a
/// bring-up has the entire preroll window to accumulate the item's opening
/// behind it, and the heal that rescues the slot deletes exactly that.
///
/// The mechanism, in the source the patched build compiles:
/// `gst_multi_queue_src_activate_mode` with `active=TRUE` calls
/// `gst_single_queue_flush (mq, sq, FALSE, full=TRUE)` (gstmultiqueue.c:3023),
/// whose `else` arm runs `gst_single_queue_flush_queue (sq, full)`
/// (`:1460`), which pops every item off the data queue and calls
/// `sitem->destroy (sitem)` on it (`:3513-3538`), with the sticky rescue at
/// `:3530` skipped because `full` is TRUE. There is no non-destructive
/// alternative in multiqueue: the FLUSH_STOP candidate lands in the same
/// `gst_single_queue_flush (FALSE)` (`:2789`) and the sink-pad deactivation
/// calls `gst_data_queue_flush` outright (`:2698`).
///
/// So this test is not a claim about the repair being wrong. It is the claim
/// that the repair is a LOSS, and therefore that the only real fix is upstream
/// of it, do not let a slot latch while data is piling up behind it (see
/// [`the_park_is_wait_free_so_a_bring_up_leaves_nothing_queued_behind_a_latch`]
/// and `Inner::bring_up_parking_sink`).
///
/// Lever-independent on purpose: it stages the latch with a LINK, drives
/// gstreamer directly, and would read the same on any build.
#[test]
fn a_slot_latched_with_data_queued_behind_it_loses_all_of_it_to_the_heal() {
    let _serial = init();
    let mut rig = Rig::new_with_a_branch_left_at_null("queue-content-trap");

    // The first push latches the slot (the join-window trigger, already pinned
    // by `a_join_to_an_unactivated_branch_latches_the_slot_with_no_flush_at_all`).
    let _ = rig.push();
    assert!(
        rig.wait_for_the_latch(),
        "the slot did not latch, so there is no trap to measure (slot src last flow: {:?})",
        rig.slot_src.last_flow_result()
    );

    // And now the bring-up keeps going, which is what the field does: the
    // demuxer pushes the item's opening into a slot that cannot forward it.
    // Four, deliberately: multiqueue's `max-size-buffers` default is 5, and a
    // fifth would block THIS thread inside `gst_data_queue_push` rather than
    // queue.
    const QUEUED: usize = 4;
    for _ in 0..QUEUED {
        assert_eq!(
            rig.push(),
            Err(gst::FlowError::Flushing),
            "a latched slot hands FLUSHING back on every push (gstmultiqueue.c:2643), and \
             takes the buffer into its queue anyway, which is the whole trap"
        );
    }

    // The branch comes up and the repair runs, exactly as `poll_text_policy`
    // would run it: everything downstream is now healthy.
    rig.bring_the_branch_up();
    let repairs_before = FcastPlaybin::slot_unlatches();
    FcastPlaybin::unlatch_db3_slot_for_test(&rig.db3_src_pad);
    assert!(
        FcastPlaybin::slot_unlatches() > repairs_before,
        "the repair must have run"
    );

    // THE PRICE. The slot works again...
    let before = rig.delivered.load(Ordering::SeqCst);
    assert_eq!(
        rig.push(),
        Ok(gst::FlowSuccess::Ok),
        "the healed slot must accept data again"
    );
    assert!(
        rig.wait_for_delivery(before + 1),
        "the buffer pushed AFTER the repair never arrived, so the repair did not work and \
         nothing below is a measurement of its cost"
    );
    // ...and everything it was holding is gone. `before` is the count taken
    // after the heal and before the one push above, so it IS the count of
    // queued buffers that survived: zero.
    assert_eq!(
        before, 0,
        "the heal preserved {before} of the {QUEUED} buffers queued behind the latch. If \
         gst_single_queue_flush_queue has learned to keep them, C8b's whole premise (and \
         Inner::bring_up_parking_sink's reason to exist) needs re-reading, check, do not \
         just update the number"
    );
    rig.shutdown();
}

/// THE FIELD, STAGED END TO END: a park at PAUSED, the bring-up it swallows,
/// the latch its removal creates, and the opening the heal then destroys.
///
/// `nope.txt` is this test with a real DASH stream in it. Park at 28.377, join
/// at 28.638, "a decodebin3 multiqueue slot is latched FLUSHING ...
/// re-activating it" 0.15 ms later, no replay line, no upstream discard,
/// and the first cue on screen is the file's THIRD (6.450) even though its
/// first two open at 0.000 and 3.980. Every one of those five observations is
/// asserted below.
///
/// # Why the park is staged by hand rather than levered
///
/// `FCAST_NO_WAITFREE_TEXT_PARK` would do it today, but this test is a record
/// of what GStreamer does to a park at PAUSED, and that record should outlive
/// the lever. It also keeps the pair honest: this one must stay green while its
/// twin below flips, which is only meaningful if they cannot both be turned off
/// by the same switch.
#[test]
fn a_park_at_paused_swallows_the_opening_and_its_removal_latches_the_slot() {
    let _serial = init();
    let mut rig = ParkRig::new_unparked_straight_to_null("paused-park");

    const OPENING: usize = 4;
    for _ in 0..OPENING {
        let _ = rig.push();
    }

    // ONE. basesink prerolls the first buffer and blocks the slot's loop
    // thread inside the second (`gst_base_sink_wait_preroll`), which `async=
    // false` does not prevent and `drop=true` never gets a chance to. In the
    // field that one buffer was the file's zero-length twin, which
    // `item_from_sample` correctly refuses, which is why `replayed` was 0 and
    // the join printed no replay line at all.
    assert!(
        rig.wait_for_park(1),
        "the park did not even preroll one buffer, so the rig is not the field's shape"
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(
        rig.parked.load(Ordering::SeqCst),
        1,
        "a park at PAUSED must keep exactly ONE buffer. If it now keeps more, basesink has \
         stopped preroll-blocking and C8b's premise needs re-reading"
    );

    // THE UNPARK: unlink, then NULL. PAUSED->READY sets basesink flushing,
    // the parked push returns GST_FLOW_FLUSHING, and
    // `gst_single_queue_push_one` writes it to `sq->srcresult`
    // (gstmultiqueue.c:2498).
    assert_eq!(
        FcastPlaybin::slot_reads_latched_for_test(&rig.db3_src_pad),
        Some(false),
        "the slot was already latched before the unpark, so the unpark cannot be credited"
    );
    rig.unpark();
    assert_eq!(
        FcastPlaybin::slot_reads_latched_for_test(&rig.db3_src_pad),
        Some(true),
        "removing a park that was holding the slot's loop thread must latch the slot: THIS is \
         the field's 28.639 warning, and nothing in the graph else went near it"
    );

    // The join, and the heal that runs in the same poll.
    rig.join();
    let repairs_before = FcastPlaybin::slot_unlatches();
    FcastPlaybin::unlatch_db3_slot_for_test(&rig.db3_src_pad);
    assert!(
        FcastPlaybin::slot_unlatches() > repairs_before,
        "the heal must have run against the latch the unpark created"
    );

    // THE LOSS. Everything the bring-up delivered after the park's one preroll
    // was inside decodebin3's single queue, and the repair emptied it
    // (gstmultiqueue.c:3513-3538).
    let survivors = rig.delivered.load(Ordering::SeqCst);
    assert_eq!(
        survivors, 0,
        "{survivors} of the queued cues survived the heal. That would be good news and it \
         would retire this whole defect, check gst_single_queue_flush_queue before believing \
         it"
    );
    // And the track is alive again from here on, which is exactly why the
    // field saw the THIRD cue and everything after it.
    let before = rig.delivered.load(Ordering::SeqCst);
    assert_eq!(
        rig.push(),
        Ok(gst::FlowSuccess::Ok),
        "the healed slot must accept data again"
    );
    assert!(
        rig.wait_for_delivery(before + 1),
        "the buffer pushed after the heal never arrived"
    );
    rig.shutdown();
}

/// C8b's FIX, driven through the shipped retirement: the unpark releases the
/// push parked in the sink's preroll with OK instead of FLUSHING, so no latch
/// forms and the bring-up's backlog REACHES THE BRANCH.
///
/// This is the assertion that matters, and it is stronger than "no latch": the
/// buffers decodebin3 queued while the park held its loop thread are still
/// there after the unpark, and the join drains them into the consumer. Under
/// the pre-fix unpark they are latched behind a dead slot and then destroyed by
/// the heal, the twin above measures exactly that, on the same rig, with the
/// same numbers.
///
/// # Verification
///
/// * Green as shipped.
/// * `FCAST_NO_WAITFREE_UNPARK=1` (the pre-C8b unpark) turns it RED on the
///   latch assertion, and would turn it red again on the delivery count below.
#[test]
fn the_unpark_releases_the_parked_push_so_the_backlog_reaches_the_branch() {
    let _serial = init();
    let mut rig = ParkRig::new("waitfree-unpark");

    // The item's opening, arriving while the pipeline is still coming up. Four,
    // deliberately: the slot's loop is parked inside the park's preroll, so
    // anything beyond multiqueue's `max-size-buffers` default of 5 would block
    // THIS thread inside `gst_data_queue_push` rather than queue.
    const OPENING: usize = 4;
    for _ in 0..OPENING {
        let _ = rig.push();
    }
    // ONE, and that is not the defect: it is the shape the defect needs. The
    // park prerolls the first buffer and the rest wait inside decodebin3,
    // which is fine as long as they are still there when the branch arrives.
    assert!(
        rig.wait_for_park(1),
        "the park did not preroll anything, so the rig is not the field's shape"
    );

    // THE UNPARK, bit-for-bit `Inner::unpark_stream_for_join`: unlink, LINK
    // THE BRANCH, and only then retire the park. The parked push is released
    // by the retirement, so the branch has to be in place first or that push
    // lands on an unlinked pad.
    rig.unlink_the_park();
    let entry = rig.join();
    rig.retire_the_park();
    assert_eq!(
        FcastPlaybin::slot_reads_latched_for_test(&rig.db3_src_pad),
        Some(false),
        "the unpark latched the slot. The park was holding the slot's loop thread and the \
         retirement released that push with FLUSHING instead of OK (C8b), so everything \
         decodebin3 queued during the bring-up is one heal away from being destroyed"
    );

    // AND THE BACKLOG ARRIVES. The single queue has been holding the opening
    // since the park took its loop thread; released into a branch that is
    // already linked, it delivers the lot once the item starts. Under the
    // pre-fix unpark the same buffers are latched and then destroyed by the
    // heal, the twin above measures exactly that, on the same rig, with the
    // same numbers.
    //
    // OPENING - 1, and the one that is missing is not lost: it is the buffer
    // the park PREROLLED and kept, which in the product is what
    // `Inner::take_parked_text_cues` replays into the consumer at this exact
    // moment. Between the two, the whole opening is accounted for.
    rig.play();
    assert!(
        rig.wait_for_delivery(OPENING - 1),
        "only {} of the {} cues left queued behind the park reached the branch. They were \
         never destroyed (no latch, so no heal), so if they are not arriving the slot is \
         stalled some other way",
        rig.delivered.load(Ordering::SeqCst),
        OPENING - 1
    );
    assert!(
        stickies_on(&entry).contains(&gst::EventType::Segment),
        "the joined branch was opened without a segment"
    );

    // And it keeps working from here.
    let before = rig.delivered.load(Ordering::SeqCst);
    assert_eq!(
        rig.push(),
        Ok(gst::FlowSuccess::Ok),
        "the slot refused data after the park was removed"
    );
    assert!(
        rig.wait_for_delivery(before + 1),
        "the buffer pushed after the join never arrived"
    );
    rig.shutdown();
}

/// The gate: a slot that is NOT latched must be left alone, because the repair
/// is not free (`gst_single_queue_flush (FALSE, full=TRUE)` drops what the slot
/// holds and re-inits its segments).
#[test]
fn a_healthy_slot_is_left_alone() {
    let _serial = init();
    let mut rig = Rig::new("healthy-slot");
    assert!(
        rig.wait_for_delivery(1),
        "the rig never delivered its first buffer"
    );

    let repairs_before = FcastPlaybin::slot_unlatches();
    let clean_before = FcastPlaybin::slot_unlatch_clean();
    // One probe, no wait: `unlatch_db3_slot` never polls (see there, the
    // poll it used to do cost `external_subtitle_lifecycle` a whole suite).
    FcastPlaybin::unlatch_db3_slot_for_test(&rig.db3_src_pad);

    assert_eq!(
        FcastPlaybin::slot_unlatches(),
        repairs_before,
        "a healthy slot must not be re-activated"
    );
    assert!(
        FcastPlaybin::slot_unlatch_clean() > clean_before,
        "and it must be COUNTED as clean, so a zero repair count is not confused with a \
         repair that never ran"
    );

    let before = rig.delivered.load(Ordering::SeqCst);
    assert_eq!(rig.push(), Ok(gst::FlowSuccess::Ok), "the slot still flows");
    assert!(
        rig.wait_for_delivery(before + 1),
        "the healthy slot stopped delivering after the un-latch looked at it"
    );
    rig.shutdown();
}
