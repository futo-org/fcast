//! The never-fires census: one assertion over every counter whose healthy
//! delta is zero.
//!
//! # Why this exists
//!
//! A counter that says "this silent failure happened" is worth nothing until
//! something reads it. Several of the crate's counters had NO reader at all -
//! `rescue_disarm_timeouts` is literally described as the counter that says the
//! wedge happened and the worker survived it, and nothing ever looked. This
//! module is the reader, and it reads all of them at once so a new counter
//! joins the check by declaring itself `zero` in `src/stats.rs` rather than by
//! someone remembering to write an assertion.
//!
//! # How to use it
//!
//! Take [`baseline`] where the scenario starts and call [`assert_flat`] where
//! it ends. Deltas, not totals: a test binary's process-global counts are
//! cumulative across its tests, so an absolute zero would only ever hold for
//! whichever test ran first.
//!
//! A scenario that legitimately moves one of them names it in `allowed`, with
//! the reason in a comment at the call site. Nothing else is skippable, which
//! is the point.

#![allow(dead_code)]

use fcastplaybin::{FcastPlaybin, GlobalStats};

/// The counters as they stand right now, to subtract from later.
pub fn baseline() -> GlobalStats {
    FcastPlaybin::global_stats()
}

/// Fail if any zero-verdict counter moved since `before`.
///
/// `what` names the scenario in the failure message. `allowed` names counters
/// this scenario is expected to move, so a suite that deliberately stages a
/// failure shape does not have to give up the rest of the census.
pub fn assert_flat(before: &GlobalStats, what: &str, allowed: &[&str]) {
    let after = FcastPlaybin::global_stats();
    let moved: Vec<(&'static str, u64)> = after
        .moved_from_zero(before)
        .into_iter()
        .filter(|(name, _)| !allowed.contains(name))
        .collect();
    if moved.is_empty() {
        return;
    }
    let detail = moved
        .iter()
        .map(|(name, delta)| format!("  {name}: +{delta}"))
        .collect::<Vec<_>>()
        .join("\n");
    panic!(
        "{what}: {} counter(s) whose healthy value is zero moved:\n{detail}\n\
         Each one is an instrument for a silent failure (see src/stats.rs for \
         what the counter means). A count here is the failure, not a flaky \
         assertion.",
        moved.len()
    );
}

/// [`assert_flat`] with nothing excluded.
pub fn assert_flat_all(before: &GlobalStats, what: &str) {
    assert_flat(before, what, &[]);
}

/// A scope guard shape for suites whose tests all run through one harness:
/// build it with the harness, check it on drop.
///
/// Silent while the thread is already panicking, so the real failure is what
/// the test reports rather than a double panic (which aborts the process and
/// takes every other test's output with it).
pub struct Census {
    before: GlobalStats,
    what: String,
    allowed: Vec<&'static str>,
}

impl Census {
    pub fn arm(what: impl Into<String>) -> Self {
        Self {
            before: baseline(),
            what: what.into(),
            allowed: Vec::new(),
        }
    }

    /// Exclude a counter this scenario legitimately moves.
    pub fn allowing(mut self, counter: &'static str) -> Self {
        self.allowed.push(counter);
        self
    }
}

impl Drop for Census {
    fn drop(&mut self) {
        if std::thread::panicking() {
            return;
        }
        assert_flat(&self.before, &self.what, &self.allowed);
    }
}
