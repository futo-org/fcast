use std::time::{Duration, Instant};

use tracing::debug;

/// Recent render-cost samples kept for the reservation percentile.
const WINDOW: usize = 32;
/// Reservation percentile: the delay must cover this fraction of recent frames'
/// real render cost. Worst-case, not mean.
const PERCENTILE: f32 = 0.95;
/// Grow the applied delay only once the reservation exceeds it by this much
/// (ignore sub-millisecond jitter).
const GROW_MARGIN: Duration = Duration::from_millis(2);
/// Shrink only once the reservation drops this far below the applied value...
const SHRINK_MARGIN: Duration = Duration::from_millis(5);
/// ...and has stayed low this long. Grow eagerly, shrink lazily: a brief calm
/// between busy stretches must not trigger a reconfig.
const SHRINK_COOLDOWN: Duration = Duration::from_secs(2);
/// Re-evaluate at most this often.
const POLL_INTERVAL: Duration = Duration::from_millis(250);
/// Clamp: a reservation beyond a few frame durations is pathological, capping it
/// bounds the presentation delay we impose.
const MAX_DELAY: Duration = Duration::from_millis(80);

struct LatencyDeque {
    samples: [Duration; WINDOW],
    len: usize,
    head: usize,
}

impl LatencyDeque {
    const fn new() -> Self {
        Self {
            samples: [Duration::ZERO; WINDOW],
            len: 0,
            head: 0,
        }
    }

    #[inline]
    fn push(&mut self, cost: Duration) {
        self.samples[self.head] = cost;
        self.head = (self.head + 1) % WINDOW;
        if self.len < WINDOW {
            self.len += 1;
        }
    }

    #[inline]
    fn len(&self) -> usize {
        self.len
    }

    fn quantile(&self, q: f32) -> Duration {
        debug_assert!(self.len > 0);
        let mut scratch = [Duration::ZERO; WINDOW];
        let valid = &mut scratch[..self.len];
        valid.copy_from_slice(&self.samples[..self.len]);
        let idx = (((self.len as f32) * q).ceil() as usize)
            .saturating_sub(1)
            .min(self.len - 1);
        *valid.select_nth_unstable(idx).1
    }
}

/// Tracks recent video render cost and derives the sink `render-delay` to apply.
/// See the module docs.
pub struct RenderLatencyTracker {
    samples: LatencyDeque,
    /// The delay currently set on the sink (starts at the base-sink default, 0).
    applied: Duration,
    /// Last time [`poll`](Self::poll) evaluated (rate limiter).
    last_poll: Option<Instant>,
    /// When the reservation first dropped enough to justify a shrink, reset the
    /// moment it climbs back. Gates the shrink cooldown.
    shrink_since: Option<Instant>,
}

impl RenderLatencyTracker {
    pub fn new() -> Self {
        Self {
            samples: LatencyDeque::new(),
            applied: Duration::ZERO,
            last_poll: None,
            shrink_since: None,
        }
    }

    pub fn record(&mut self, cost: Duration) {
        self.samples.push(cost);
    }

    /// Worst-case reservation over the window (a high percentile, so a single
    /// slow frame does not pin the delay high). `None` until the window is warm.
    fn reservation(&self) -> Option<Duration> {
        if self.samples.len() < WINDOW / 2 {
            return None;
        }
        Some(self.samples.quantile(PERCENTILE))
    }

    /// Inspector snapshot: the current p95 render cost (`None` until the
    /// window is warm) and the render-delay currently applied to the sink.
    pub fn debug_snapshot(&self) -> (Option<Duration>, Duration) {
        (self.reservation(), self.applied)
    }

    /// Decide whether the sink's `render-delay` should change. Returns the new
    /// delay to apply (and adopts it internally) or `None` to leave it as-is.
    pub fn poll(&mut self, now: Instant) -> Option<Duration> {
        if let Some(last) = self.last_poll
            && now.duration_since(last) < POLL_INTERVAL
        {
            return None;
        }
        self.last_poll = Some(now);

        let target = self.reservation()?.min(MAX_DELAY);
        let grow = target > self.applied + GROW_MARGIN;
        let shrink = target + SHRINK_MARGIN < self.applied;

        if shrink {
            self.shrink_since.get_or_insert(now);
        } else {
            self.shrink_since = None;
        }

        let apply = grow
            || (shrink
                && self
                    .shrink_since
                    .is_some_and(|since| now.duration_since(since) >= SHRINK_COOLDOWN));
        if !apply {
            return None;
        }

        let previous = self.applied;
        self.applied = target;
        self.shrink_since = None;
        debug!(
            previous_ms = previous.as_secs_f64() * 1e3,
            applied_ms = target.as_secs_f64() * 1e3,
            samples = self.samples.len(),
            "video sink render-delay updated",
        );
        Some(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn no_reservation_until_warm() {
        let mut t = RenderLatencyTracker::new();
        let now = Instant::now();
        for _ in 0..(WINDOW / 2 - 1) {
            t.record(ms(10));
        }
        assert_eq!(t.poll(now), None);
    }

    #[test]
    fn grows_eagerly_to_the_percentile() {
        let mut t = RenderLatencyTracker::new();
        for _ in 0..WINDOW {
            t.record(ms(10));
        }
        // First poll adopts the reservation (0 -> ~10ms).
        let applied = t.poll(Instant::now()).expect("should apply");
        assert!(applied >= ms(9) && applied <= ms(10), "{applied:?}");
    }

    #[test]
    fn ignores_sub_margin_jitter() {
        let mut t = RenderLatencyTracker::new();
        for _ in 0..WINDOW {
            t.record(ms(10));
        }
        let base = Instant::now();
        t.poll(base).expect("first apply");
        // A tiny bump within GROW_MARGIN must not re-apply.
        for _ in 0..WINDOW {
            t.record(ms(11));
        }
        assert_eq!(t.poll(base + POLL_INTERVAL * 2), None);
    }

    #[test]
    fn shrinks_only_after_cooldown() {
        let mut t = RenderLatencyTracker::new();
        for _ in 0..WINDOW {
            t.record(ms(30));
        }
        let base = Instant::now();
        assert!(t.poll(base).is_some(), "grow to 30ms");

        // Render cost collapses, fill the window with low samples.
        for _ in 0..WINDOW {
            t.record(ms(2));
        }
        // Still within the cooldown: no shrink yet.
        assert_eq!(t.poll(base + POLL_INTERVAL), None);
        // After the cooldown: shrink applies.
        let shrunk = t
            .poll(base + SHRINK_COOLDOWN + POLL_INTERVAL)
            .expect("shrink after cooldown");
        assert!(shrunk <= ms(2), "{shrunk:?}");
    }

    #[test]
    fn clamps_to_max() {
        let mut t = RenderLatencyTracker::new();
        for _ in 0..WINDOW {
            t.record(ms(500));
        }
        let applied = t.poll(Instant::now()).expect("apply");
        assert_eq!(applied, MAX_DELAY);
    }

    /// The ring keeps exactly the last WINDOW samples across a wrap, and its
    /// quantile matches a plain sort of that window.
    #[test]
    fn deque_wraps_and_matches_sorted_quantile() {
        let mut d = LatencyDeque::new();
        // Push 3 full windows' worth of distinct values, only the last WINDOW
        // survive.
        let total = WINDOW * 3;
        for i in 0..total {
            d.push(ms(i as u64));
        }
        assert_eq!(d.len(), WINDOW);

        // Reference: the last WINDOW values, sorted.
        let mut expected: Vec<Duration> = ((total - WINDOW)..total).map(|i| ms(i as u64)).collect();
        expected.sort_unstable();

        for q in [0.0f32, 0.5, 0.95, 1.0] {
            let idx = (((WINDOW as f32) * q).ceil() as usize)
                .saturating_sub(1)
                .min(WINDOW - 1);
            assert_eq!(d.quantile(q), expected[idx], "q={q}");
        }
    }

    #[test]
    fn deque_partial_window() {
        let mut d = LatencyDeque::new();
        for v in [5u64, 1, 9, 3] {
            d.push(ms(v));
        }
        assert_eq!(d.len(), 4);
        assert_eq!(d.quantile(0.0), ms(1)); // min
        assert_eq!(d.quantile(1.0), ms(9)); // max
    }
}
