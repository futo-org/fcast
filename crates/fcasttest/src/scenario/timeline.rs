//! Event-anchored actions. Every anchor is a bounded wait on something the
//! pipeline actually did: a push parking on a gate, buffers reaching a sink, an
//! event reaching a sink. No wall-clock sleep is ever a correctness wait, and no
//! anchor blocks forever.

use std::{fmt, sync::Arc, time::Duration};

use crate::{registry::Scenario, sink::Recording};

/// Bound every anchor waits under. Generous: a busy box must not flake, a wedge
/// must not hang the suite.
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(15);

/// An anchor that never happened within its bound.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineError {
    /// What was waited for, in the caller's terms.
    pub anchor: String,
    pub timeout: Duration,
}

impl fmt::Display for TimelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            f,
            "timeline anchor never reached within {:?}: {}",
            self.timeout, self.anchor
        )
    }
}

impl std::error::Error for TimelineError {}

/// Anchors and releases for one scenario. Plain blocking helpers, called in
/// sequence by the test, so the script reads in the order it executes.
#[derive(Clone)]
pub struct Timeline {
    scenario: Arc<Scenario>,
    timeout: Duration,
}

impl Timeline {
    pub fn new(scenario: Arc<Scenario>) -> Self {
        Self {
            scenario,
            timeout: DEFAULT_TIMEOUT,
        }
    }

    pub fn with_timeout(mut self, timeout: Duration) -> Self {
        self.timeout = timeout;
        self
    }

    pub fn timeout(&self) -> Duration {
        self.timeout
    }

    /// Blocks until a push has PARKED on the named sync point, so the next
    /// action provably lands while the source is mid-push. Returns immediately
    /// once anything has ever arrived at the gate.
    pub fn on_sync_point_arrival(&self, name: &str) -> Result<(), TimelineError> {
        self.on_sync_point_arrivals(name, 1)
    }

    /// Blocks until `count` pushes have parked on the named sync point in total.
    ///
    /// The counted form matters as soon as a schedule restarts: a flushing seek
    /// makes every stalled stream park on the same gate again, and
    /// [`on_sync_point_arrival`](Self::on_sync_point_arrival) is already
    /// satisfied by the FIRST park, so it returns instantly and the next action
    /// races the restarted push. It is also the anchor for a multi-stream
    /// scenario, where "both streams are parked" is two arrivals and not one.
    pub fn on_sync_point_arrivals(&self, name: &str, count: u64) -> Result<(), TimelineError> {
        let gate = self.scenario.sync_point(name);
        if gate.wait_for_arrivals(count, self.timeout) {
            return Ok(());
        }
        Err(self.err(format!(
            "{count} pushes parked on sync point {name:?}, only {} arrived",
            gate.arrivals()
        )))
    }

    /// Blocks until `recording` has seen `count` buffers. Prerolls do not count
    /// (see [`Recording::buffer_count`]).
    pub fn after_buffers(&self, recording: &Recording, count: usize) -> Result<(), TimelineError> {
        if recording.wait_for_buffers(count, self.timeout) {
            return Ok(());
        }
        Err(self.err(format!(
            "{count} buffers at a sink, only {} arrived",
            recording.buffer_count()
        )))
    }

    /// Blocks until `recording` has seen an event of `type_name` (see
    /// [`crate::sink::event_name`]).
    pub fn after_event(&self, recording: &Recording, type_name: &str) -> Result<(), TimelineError> {
        if recording.wait_for_event(type_name, self.timeout) {
            return Ok(());
        }
        Err(self.err(format!("a {type_name} event at a sink")))
    }

    /// Lets a parked push continue. Idempotent, and safe to call before anything
    /// has arrived (the gate stays open).
    pub fn release(&self, name: &str) {
        self.scenario.sync_point(name).release();
    }

    /// Releases every gate created so far.
    pub fn release_all(&self) {
        self.scenario.release_all_sync_points();
    }

    fn err(&self, anchor: String) -> TimelineError {
        TimelineError {
            anchor,
            timeout: self.timeout,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        registry,
        scenario::ScenarioBuilder,
        sink::{RecordEntry, event_name},
    };

    fn handle(key: &str) -> crate::scenario::ScenarioHandle {
        gst::init().unwrap();
        ScenarioBuilder::new(key).audio("audio_0").register()
    }

    #[test]
    fn unreached_anchors_time_out_with_a_readable_error() {
        let scenario = handle("tlmiss");
        let timeline = scenario.timeline().with_timeout(Duration::from_millis(20));
        assert_eq!(timeline.timeout(), Duration::from_millis(20));

        let err = timeline
            .on_sync_point_arrival("never")
            .expect_err("nothing parks on it");
        assert!(err.anchor.contains("never"), "{err}");
        assert!(err.to_string().contains("20ms"), "{err}");

        let recording = Recording::new();
        let err = timeline
            .after_buffers(&recording, 2)
            .expect_err("no buffers");
        assert!(err.anchor.contains("only 0 arrived"), "{err}");
        timeline
            .after_event(&recording, event_name::EOS)
            .expect_err("no eos");

        scenario.unregister();
    }

    #[test]
    fn anchors_return_once_the_pipeline_side_happens() {
        let scenario = handle("tlhit");
        let timeline = scenario.timeline().with_timeout(Duration::from_secs(5));
        let gate = scenario.sync_point("stall");

        let parked = {
            let gate = gate.clone();
            std::thread::spawn(move || gate.wait())
        };
        timeline
            .on_sync_point_arrival("stall")
            .expect("the push parks");
        timeline.release("stall");
        parked.join().expect("the parked push continues");

        let recording = Recording::new();
        let writer = recording.clone();
        let pushed = std::thread::spawn(move || {
            writer.push(RecordEntry::synthetic_buffer(gst::ClockTime::ZERO));
            writer.push(RecordEntry::synthetic_event(gst::EventType::Eos));
        });
        timeline
            .after_buffers(&recording, 1)
            .expect("the buffer arrives");
        timeline
            .after_event(&recording, event_name::EOS)
            .expect("the eos arrives");
        pushed.join().expect("writer thread");

        // release_all covers gates the test never named.
        timeline.release_all();
        assert!(registry::lookup("tlhit").is_some());
        scenario.unregister();
    }
}
