//! An EXHAUSTIVE model check of [`StateMachine`] against a stand-in for the
//! two things it actually talks to: the GStreamer pipeline underneath it and
//! the receiver's event loop above it.
//!
//! The hand-written cases in `regression_state_machine.rs` and in the module's
//! own unit tests each pin ONE captured wedge. What none of them can pin is the
//! property those wedges are all instances of: **from every reachable state,
//! feeding the pipeline's own faithful answers converges on a settled transport
//! that AGREES with the pipeline.** A machine that strands (`running()` stays
//! `None` forever, so the receiver's pump gate never opens again) or that
//! settles on the wrong side (`running() == Playing` at a PAUSED pipeline, so
//! the UI and the senders report a lie) is exactly the class of defect the
//! rescue in `state_changed` and the redispatch in `buffering` were added for,
//! one captured trace at a time.
//!
//! So: enumerate every op sequence up to [`depth()`] over the receiver's whole
//! transport alphabet, run each against the rig, and assert convergence and
//! agreement. Both commit shapes are covered ([`Commit`]), because the
//! stale-overshoot family only exists on the async one.
//!
//! # What makes the rig faithful, point by point
//!
//! * A `set_state` to the state the pipeline is ALREADY in posts NO
//!   `state-changed` (`gst_element_continue_state`'s "don't post silly messages
//!   with the same state"), and the crate synthesises the edge itself
//!   (`Job::SetState`, lever `FCAST_NO_SYNTHETIC_STATE_EDGE`). The rig
//!   synthesises it too, or every no-op dispatch would read as a wedge that
//!   production does not have.
//! * A seek is REFUSED anywhere but a settled PAUSED: `Job::Seek` hands it back
//!   as `PlaybinEvent::QueueSeek` and drives to PAUSED itself. The rig calls
//!   `queue_seek` for exactly that hand-back.
//! * A retarget mid-transition does not retract the edges already in flight, so
//!   the rig keeps them queued. That is the whole "stale upward overshoot"
//!   shape.
//! * A flushing seek at a settled PAUSED re-prerolls and posts one more
//!   `Paused`/`VoidPending`, which is the edge the machine settles seeks on.

use std::collections::VecDeque;

use fcastplaybin::state_machine::{
    BufferingStateResult, RunningState, Seek, StateChangeResult, StateMachine,
};

/// How long an op sequence may get. Five reaches the interleavings the captured
/// wedges are made of (a seek across a rebuffer across a retarget across a
/// re-preroll) and still runs in well under a second.
///
/// `FCAST_SM_MODEL_DEPTH` deepens it for a soak without an edit. The cost is
/// `|OPS|^depth()` traces, so 6 is ~530k and 7 is ~4.8M: minutes, not hours,
/// and worth running after any change to `state_changed` or `buffering`.
fn depth() -> usize {
    std::env::var("FCAST_SM_MODEL_DEPTH")
        .ok()
        .and_then(|v| v.parse().ok())
        .unwrap_or(5)
}

/// A ceiling on the rig's own settling loop. A healthy trace settles in a
/// handful of edges; anything near this is a ping-pong, which is a wedge of a
/// different shape and just as fatal.
const MAX_STEPS: usize = 200;

/// One thing the receiver (or the media) does to the machine.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Op {
    Play,
    Pause,
    Seek(u64),
    /// `use-buffering` posted a partial fill.
    BufferStart,
    /// ...and then a complete one.
    BufferDone,
    /// The pipeline refused the flushing seek it was handed.
    SeekFails,
    /// A bin-internal async re-preroll: a fresh sink activating under a
    /// PLAYING pipeline drops it through PAUSED with the climb still owed
    /// (`(Paused, pending Playing)`). Unlike every other op this one is the
    /// PIPELINE acting, so it posts an edge rather than asking for one.
    Repreroll,
    /// ...and the bin finishes that climb on its own. Split from
    /// [`Op::Repreroll`] precisely so the search can interleave user ops into
    /// the window between the dip and the commit, which is where every
    /// captured stale-overshoot lives.
    RepreollDone,
}

const OPS: [Op; 9] = [
    Op::Play,
    Op::Pause,
    Op::Seek(5),
    Op::Seek(30),
    Op::BufferStart,
    Op::BufferDone,
    Op::SeekFails,
    Op::Repreroll,
    Op::RepreollDone,
];

/// Whether the modelled pipeline announces an intermediate `pending != VOID`
/// edge on the way up (GStreamer's async preroll) or commits in one step.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Commit {
    Sync,
    Async,
}

/// One `state-changed` the pipeline posts. `old` is carried for realism only:
/// the machine ignores it.
#[derive(Debug, Clone, Copy)]
struct Edge {
    old: gst::State,
    new: gst::State,
    pending: gst::State,
}

struct Rig {
    sm: StateMachine,
    /// The pipeline's committed state, i.e. what a `set_state` lands on.
    state: gst::State,
    commit: Commit,
    /// Edges posted and not yet delivered. Non-empty means a transition is
    /// still in flight, which is what makes a seek illegal.
    bus: VecDeque<Edge>,
    /// The next flushing seek is refused (armed by [`Op::SeekFails`]).
    fail_next_seek: bool,
    /// The bin committed to an intermediate PAUSED and still owes its climb to
    /// PLAYING (see [`Op::Repreroll`]). An explicit `set_state` retargets the
    /// bin and cancels it, which is what GStreamer does too.
    owes_climb: bool,
    steps: usize,
    /// Everything that happened, for a failure message worth reading.
    log: Vec<String>,
}

impl Rig {
    /// A rig settled where a fresh load leaves the machine: prerolled at
    /// PAUSED, with the load's optimistic PLAYING target still recorded.
    fn loaded(commit: Commit) -> Self {
        let mut rig = Rig {
            sm: StateMachine::new(),
            state: gst::State::Ready,
            commit,
            bus: VecDeque::new(),
            fail_next_seek: false,
            owes_climb: false,
            steps: 0,
            log: Vec::new(),
        };
        rig.sm.begin_load();
        rig.log.push("begin_load".into());
        // The load prerolls: READY -> PAUSED.
        rig.set_state(gst::State::Paused);
        rig.settle();
        rig
    }

    /// Nothing left in flight anywhere: no undelivered edge and no climb the
    /// bin still owes itself.
    fn settled(&self) -> bool {
        self.bus.is_empty() && !self.owes_climb
    }

    fn set_state(&mut self, target: gst::State) {
        // An explicit request retargets the bin, so whatever it owed itself is
        // superseded by what is queued below.
        self.owes_climb = false;
        if self.state == target {
            // No `state-changed` from GStreamer; the crate synthesises it.
            self.log.push(format!("set_state({target:?}) [no-op edge]"));
            self.bus.push_back(Edge {
                old: target,
                new: target,
                pending: gst::State::VoidPending,
            });
            return;
        }
        self.log.push(format!("set_state({target:?})"));
        let old = self.state;
        // An upward commit that has to preroll announces its intermediate
        // FIRST: `(old, Paused, Playing)`. That is the shape every captured
        // overshoot is made of, and it is emitted from PAUSED too (a re-preroll
        // on the way up), where the edge reads `(Paused, Paused, Playing)`.
        if self.commit == Commit::Async && target == gst::State::Playing {
            self.bus.push_back(Edge {
                old,
                new: gst::State::Paused,
                pending: gst::State::Playing,
            });
            self.bus.push_back(Edge {
                old: gst::State::Paused,
                new: gst::State::Playing,
                pending: gst::State::VoidPending,
            });
        } else {
            self.bus.push_back(Edge {
                old,
                new: target,
                pending: gst::State::VoidPending,
            });
        }
        self.state = target;
    }

    fn do_seek(&mut self, seek: Seek) {
        if self.state != gst::State::Paused || !self.settled() {
            // `Job::Seek`'s hand-back: the caller re-parks it and the worker
            // drives to PAUSED.
            self.log.push(format!("seek {seek:?} refused -> QueueSeek"));
            self.sm.queue_seek(seek);
            self.set_state(gst::State::Paused);
            return;
        }
        if self.fail_next_seek {
            self.fail_next_seek = false;
            self.log.push(format!("seek {seek:?} FAILED"));
            let resume = self.sm.seek_failed();
            if let Some(state) = resume {
                self.set_state(state);
            }
            return;
        }
        self.log.push(format!("seek {seek:?} performed"));
        // A flushing seek re-prerolls and posts one more settled PAUSED.
        self.bus.push_back(Edge {
            old: gst::State::Paused,
            new: gst::State::Paused,
            pending: gst::State::VoidPending,
        });
    }

    fn apply_state_result(&mut self, result: StateChangeResult) {
        match result {
            StateChangeResult::ChangeState(state) => self.set_state(state),
            StateChangeResult::Seek(seek) => self.do_seek(seek),
            StateChangeResult::NewPlaybackState(_) | StateChangeResult::Waiting => {}
        }
    }

    fn apply_buffering_result(&mut self, result: BufferingStateResult) {
        match result {
            BufferingStateResult::Started(state) => self.set_state(state),
            BufferingStateResult::Finished(Some(state)) => self.set_state(state),
            BufferingStateResult::FinishedWithSeek(seek) => self.do_seek(seek),
            BufferingStateResult::Finished(None)
            | BufferingStateResult::FinishedButWaitingSeek
            | BufferingStateResult::Buffering => {}
        }
    }

    /// Deliver every queued edge, following whatever the machine asks for,
    /// until nothing is left in flight.
    fn settle(&mut self) {
        while let Some(edge) = self.bus.pop_front() {
            self.steps += 1;
            assert!(
                self.steps < MAX_STEPS,
                "the machine and the pipeline ping-ponged without converging\n\
                 model: {}\nlog:\n  {}",
                self.sm.debug_model(),
                self.log.join("\n  ")
            );
            self.log.push(format!(
                "  <- state_changed({:?} -> {:?}, pending {:?})",
                edge.old, edge.new, edge.pending
            ));
            let result = self.sm.state_changed(edge.old, edge.new, edge.pending);
            self.log.push(format!("     => {result:?}"));
            self.apply_state_result(result);
        }
    }

    fn run(&mut self, op: Op) {
        self.log.push(format!("op {op:?}"));
        match op {
            Op::Play => {
                if let Some(state) = self.sm.set_playback_state(RunningState::Playing) {
                    self.set_state(state);
                }
            }
            Op::Pause => {
                if let Some(state) = self.sm.set_playback_state(RunningState::Paused) {
                    self.set_state(state);
                }
            }
            Op::Seek(position) => {
                let seek = Seek::new(Some(gst::ClockTime::from_seconds(position)), None);
                if let Some(seek) = self.sm.seek_internal(seek, None) {
                    self.do_seek(seek);
                }
            }
            Op::BufferStart => {
                let result = self.sm.buffering(0);
                self.apply_buffering_result(result);
            }
            Op::BufferDone => {
                let result = self.sm.buffering(100);
                self.apply_buffering_result(result);
            }
            // Arms the NEXT seek to fail. A seek already in flight fails right
            // away, which is the ordering the pipeline actually produces.
            Op::SeekFails => {
                if self.sm.is_seeking() {
                    let resume = self.sm.seek_failed();
                    self.log.push(format!("seek_failed => {resume:?}"));
                    if let Some(state) = resume {
                        self.set_state(state);
                    }
                } else {
                    self.fail_next_seek = true;
                }
            }
            // Only meaningful from a settled PLAYING: the dip lands the
            // pipeline at PAUSED with the climb still owed.
            Op::Repreroll => {
                if self.state == gst::State::Playing && self.settled() {
                    self.state = gst::State::Paused;
                    self.owes_climb = true;
                    self.bus.push_back(Edge {
                        old: gst::State::Playing,
                        new: gst::State::Paused,
                        pending: gst::State::Playing,
                    });
                }
            }
            Op::RepreollDone => {
                if self.owes_climb {
                    self.owes_climb = false;
                    self.state = gst::State::Playing;
                    self.bus.push_back(Edge {
                        old: gst::State::Paused,
                        new: gst::State::Playing,
                        pending: gst::State::VoidPending,
                    });
                }
            }
        }
        // Every op is followed by whatever the pipeline makes of it. The
        // receiver's loop is not batched: it acts on each message as it comes.
        self.settle();
    }

    /// The two properties every trace has to end on.
    fn assert_converged(&self, trace: &[Op]) {
        let context = || {
            format!(
                "trace {trace:?} ({:?} commits)\nmodel: {}\nlog:\n  {}",
                self.commit,
                self.sm.debug_model(),
                self.log.join("\n  ")
            )
        };
        assert!(
            self.settled(),
            "the rig left edges undelivered\n{}",
            context()
        );
        // CONVERGENCE. `running()` is what the receiver's pump gate and its
        // whole seek queue are keyed on: `None` forever is a dead player.
        let running = self.sm.running();
        assert!(
            running.is_some(),
            "the machine never settled: no seek is in flight and no transition is \
             pending, yet running() is None\n{}",
            context()
        );
        assert!(
            !self.sm.is_seeking(),
            "a seek is still tracked at a fully settled pipeline; nothing will \
             ever dispatch or clear it\n{}",
            context()
        );
        // AGREEMENT. A machine that settles on the wrong side reports a
        // transport state the pipeline is not in, which is what reaches the
        // UI and every connected sender.
        let expected = match self.state {
            gst::State::Playing => RunningState::Playing,
            gst::State::Paused => RunningState::Paused,
            other => panic!("the rig left the pipeline at {other:?}\n{}", context()),
        };
        assert_eq!(
            running,
            Some(expected),
            "the machine settled on a transport state the pipeline is not in\n{}",
            context()
        );
        assert_eq!(
            self.sm.current_state,
            self.state,
            "the machine's mirror of the pipeline state drifted\n{}",
            context()
        );
    }
}

/// Every trace of length `<= depth()`, both commit shapes.
///
/// Returns how many traces ran, which the callers assert on: a search that
/// silently stopped exploring would pass every assertion vacuously, and this
/// file's whole value is that it explores.
fn check_all(commit: Commit) -> usize {
    let mut trace = Vec::new();
    let mut checked = 0usize;
    fn walk(commit: Commit, trace: &mut Vec<Op>, depth: usize, max: usize, checked: &mut usize) {
        if !trace.is_empty() {
            *checked += 1;
            let mut rig = Rig::loaded(commit);
            for op in trace.iter() {
                rig.run(*op);
            }
            // A trace can legitimately end mid-buffer or mid-re-preroll: the
            // receiver is not wedged there, it is waiting on the media and on
            // the bin. Let both finish so the terminal state is comparable.
            // Buffering first, because its completion can retarget the bin and
            // then there is no climb left to owe.
            rig.run(Op::BufferDone);
            rig.run(Op::RepreollDone);
            rig.assert_converged(trace);
        }
        if depth == max {
            return;
        }
        for op in OPS {
            trace.push(op);
            walk(commit, trace, depth + 1, max, checked);
            trace.pop();
        }
    }
    walk(commit, &mut trace, 0, depth(), &mut checked);
    checked
}

/// `sum(|OPS|^k for k in 1..=depth())`, i.e. every trace the walk must reach.
fn expected_traces() -> usize {
    (1..=depth()).map(|k| OPS.len().pow(k as u32)).sum()
}

#[test]
fn every_transport_trace_converges_with_synchronous_commits() {
    assert_eq!(check_all(Commit::Sync), expected_traces());
}

#[test]
fn every_transport_trace_converges_with_async_commits() {
    assert_eq!(check_all(Commit::Async), expected_traces());
}

/// The rig's own instrument check: without the parked-seek rescue in
/// `state_changed`, a seek parked at a pipeline that then settles at PLAYING
/// waits for an edge nobody will post again. The lever is process-global, so
/// this runs as its own test and only asserts what it can prove locally.
#[test]
fn the_rescue_is_what_makes_a_parked_seek_survive_a_playing_settle() {
    let mut rig = Rig::loaded(Commit::Sync);
    rig.run(Op::Play);
    assert_eq!(rig.sm.running(), Some(RunningState::Playing));

    // A seek the pipeline hands back while PLAYING parks, and the drive to
    // PAUSED is what dispatches it.
    rig.run(Op::Seek(30));
    rig.assert_converged(&[Op::Seek(30)]);
    assert_eq!(
        rig.sm.running(),
        Some(RunningState::Playing),
        "a seek issued while playing must come back to playing"
    );
}
