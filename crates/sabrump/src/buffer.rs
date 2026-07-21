use std::collections::BTreeMap;
use std::ops::RangeInclusive;
use std::sync::Arc;
use std::time::{Duration, Instant};

use parking_lot::Mutex;
use tokio::sync::Notify;

use crate::format::SabrFormatKey;
use crate::segment::SabrSegment;

/// Sentinel matching the Kotlin `Long.MIN_VALUE` "no value" marker.
pub const NO_US: i64 = i64::MIN;

struct Inner {
    segments: BTreeMap<i32, Arc<SabrSegment>>,
    init_segment: Option<Arc<SabrSegment>>,
}

pub struct SabrTrackBuffer {
    pub format_key: SabrFormatKey,
    inner: Mutex<Inner>,
    notify: Notify,
}

impl SabrTrackBuffer {
    pub fn new(format_key: SabrFormatKey) -> Self {
        Self {
            format_key,
            inner: Mutex::new(Inner {
                segments: BTreeMap::new(),
                init_segment: None,
            }),
            notify: Notify::new(),
        }
    }

    pub fn init_segment(&self) -> Option<Arc<SabrSegment>> {
        self.inner.lock().init_segment.clone()
    }

    pub fn segment_count(&self) -> usize {
        self.inner.lock().segments.len()
    }

    pub fn highest_sequence(&self) -> i32 {
        self.inner
            .lock()
            .segments
            .keys()
            .next_back()
            .copied()
            .unwrap_or(-1)
    }

    pub fn lowest_sequence(&self) -> i32 {
        self.inner
            .lock()
            .segments
            .keys()
            .next()
            .copied()
            .unwrap_or(-1)
    }

    pub fn announce(&self, segment: Arc<SabrSegment>) {
        let mut inner = self.inner.lock();
        if segment.is_init {
            inner.init_segment = Some(segment);
        } else {
            inner.segments.insert(segment.sequence_number, segment);
        }
        drop(inner);
        self.notify.notify_waiters();
    }

    pub fn notify_changed(&self) {
        self.notify.notify_waiters();
    }

    pub fn get(&self, sequence_number: i32) -> Option<Arc<SabrSegment>> {
        self.inner
            .lock()
            .segments
            .get(&sequence_number)
            .cloned()
    }

    pub fn snapshot(&self) -> Vec<Arc<SabrSegment>> {
        self.inner
            .lock()
            .segments
            .values()
            .cloned()
            .collect()
    }

    pub fn first_at_or_after(&self, min_sequence: i32) -> Option<Arc<SabrSegment>> {
        let inner = self.inner.lock();
        first_at_or_after_locked(&inner, min_sequence)
    }

    pub fn first_covering(&self, position_us: i64) -> Option<Arc<SabrSegment>> {
        let inner = self.inner.lock();
        first_covering_locked(&inner, position_us)
    }

    /// Await either a buffer-change notification or the deadline, re-checking
    /// `pred` each wake. The `Notified` future is registered (`enable`) *before*
    /// `pred` runs, so a notification racing the check is never lost.
    async fn await_pred<T>(
        &self,
        deadline: Instant,
        mut pred: impl FnMut() -> Option<T>,
    ) -> Option<T> {
        loop {
            let notified = self.notify.notified();
            tokio::pin!(notified);
            notified.as_mut().enable();
            if let Some(v) = pred() {
                return Some(v);
            }
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                return pred();
            }
            if tokio::time::timeout(remaining, notified.as_mut()).await.is_err() {
                return pred();
            }
        }
    }

    pub async fn await_announced(&self, min_sequence: i32, timeout: Duration) -> Option<Arc<SabrSegment>> {
        let deadline = Instant::now() + timeout;
        self.await_pred(deadline, || {
            first_at_or_after_locked(&self.inner.lock(), min_sequence)
        })
        .await
    }

    pub async fn await_covering(&self, position_us: i64, timeout: Duration) -> Option<Arc<SabrSegment>> {
        let deadline = Instant::now() + timeout;
        self.await_pred(deadline, || {
            first_covering_locked(&self.inner.lock(), position_us)
        })
        .await
    }

    pub async fn await_sequence(&self, sequence: i32, timeout: Duration) -> Option<Arc<SabrSegment>> {
        let deadline = Instant::now() + timeout;
        self.await_pred(deadline, || self.inner.lock().segments.get(&sequence).cloned())
            .await
    }

    /// Wait until `segment` has more than `position` bytes or is complete.
    pub async fn await_bytes(&self, segment: &SabrSegment, position: usize, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        self.await_pred(deadline, || {
            (segment.size() > position || segment.is_complete()).then_some(())
        })
        .await
        .is_some()
    }

    pub fn buffered_end_from_front_us(&self) -> i64 {
        self.buffered_end_us(NO_US)
    }

    pub fn buffered_exact_end_us(&self) -> i64 {
        let inner = self.inner.lock();
        let mut end = NO_US;
        let mut expected = -1i32;
        for (&sequence, segment) in inner.segments.iter() {
            if expected != -1 && sequence != expected {
                break;
            }
            if !segment.is_complete() || !segment.duration_exact() {
                break;
            }
            end = segment.end_us();
            expected = sequence + 1;
        }
        end
    }

    pub fn publishable_run(&self) -> Option<RangeInclusive<i32>> {
        let inner = self.inner.lock();
        let first = *inner.segments.keys().next()?;
        let mut end = -1i32;
        let mut sequence = first;
        loop {
            let Some(segment) = inner.segments.get(&sequence) else {
                break;
            };
            if !segment.is_complete() || !segment.duration_exact() {
                break;
            }
            end = sequence;
            sequence += 1;
        }
        if end < 0 { None } else { Some(first..=end) }
    }

    pub fn exact_end_from_sequence(&self, sequence: i32) -> i64 {
        let inner = self.inner.lock();
        let mut end = NO_US;
        let mut expected = sequence;
        for (&seq, segment) in inner.segments.range(sequence..) {
            if seq != expected {
                break;
            }
            if !segment.is_complete() || !segment.duration_exact() {
                break;
            }
            end = segment.end_us();
            expected = seq + 1;
        }
        end
    }

    pub fn recent_start_deltas_us(&self, max: usize) -> Vec<i64> {
        let inner = self.inner.lock();
        let mut deltas = Vec::with_capacity(max);
        let mut newer: Option<&Arc<SabrSegment>> = None;
        for segment in inner.segments.values().rev() {
            let next = newer;
            newer = Some(segment);
            let Some(next) = next else { continue };
            if next.sequence_number != segment.sequence_number + 1 {
                continue;
            }
            let delta = next.start_us - segment.start_us;
            if delta > 0 {
                deltas.push(delta);
            }
            if deltas.len() >= max {
                break;
            }
        }
        deltas
    }

    pub fn buffered_end_us(&self, from_us: i64) -> i64 {
        let inner = self.inner.lock();
        let mut end = NO_US;
        let mut expected = -1i32;
        for (&sequence, segment) in inner.segments.iter() {
            if expected == -1 {
                if from_us != NO_US && segment.end_us() < from_us {
                    continue;
                }
                if from_us != NO_US && segment.start_us > from_us {
                    return NO_US;
                }
            }
            if expected != -1 && sequence != expected {
                break;
            }
            if !segment.is_complete() {
                break;
            }
            end = segment.end_us();
            expected = sequence + 1;
        }
        end
    }

    pub fn last_completed_sequence(&self, from_us: i64) -> i32 {
        let inner = self.inner.lock();
        let mut last = -1i32;
        let mut expected = -1i32;
        for (&sequence, segment) in inner.segments.iter() {
            if expected == -1 {
                if from_us != NO_US && segment.end_us() < from_us {
                    continue;
                }
                if from_us != NO_US && segment.start_us > from_us {
                    return -1;
                }
            }
            if expected != -1 && sequence != expected {
                break;
            }
            if !segment.is_complete() {
                break;
            }
            last = sequence;
            expected = sequence + 1;
        }
        last
    }

    pub fn last_completed_from_front(&self) -> i32 {
        self.last_completed_sequence(NO_US)
    }

    pub fn discard(&self, segment: &Arc<SabrSegment>) {
        let mut inner = self.inner.lock();
        if segment.is_complete() {
            return;
        }
        if segment.is_init {
            if inner
                .init_segment
                .as_ref()
                .is_some_and(|s| Arc::ptr_eq(s, segment))
            {
                inner.init_segment = None;
            }
        } else if inner
            .segments
            .get(&segment.sequence_number)
            .is_some_and(|s| Arc::ptr_eq(s, segment))
        {
            inner.segments.remove(&segment.sequence_number);
        }
        drop(inner);
        self.notify.notify_waiters();
    }

    pub fn evict_before_sequence(&self, sequence: i32) {
        let mut inner = self.inner.lock();
        let mut to_remove = Vec::new();
        for (&seq, segment) in inner.segments.range(..sequence) {
            if !segment.is_complete() {
                break;
            }
            to_remove.push(seq);
        }
        let evicted = !to_remove.is_empty();
        for seq in to_remove {
            inner.segments.remove(&seq);
        }
        drop(inner);
        if evicted {
            self.notify.notify_waiters();
        }
    }

    pub fn evict_before(&self, position_us: i64) {
        let mut inner = self.inner.lock();
        let mut to_remove = Vec::new();
        let mut expected = -1i32;
        for (&sequence, segment) in inner.segments.iter() {
            if expected != -1 && sequence != expected {
                break;
            }
            if !segment.is_complete() {
                break;
            }
            if segment.end_us() >= position_us {
                break;
            }
            to_remove.push(sequence);
            expected = sequence + 1;
        }
        let evicted = !to_remove.is_empty();
        for seq in to_remove {
            inner.segments.remove(&seq);
        }
        drop(inner);
        if evicted {
            self.notify.notify_waiters();
        }
    }

    pub fn clear(&self) {
        let mut inner = self.inner.lock();
        inner.segments.clear();
        drop(inner);
        self.notify.notify_waiters();
    }
}

fn first_at_or_after_locked(inner: &Inner, min_sequence: i32) -> Option<Arc<SabrSegment>> {
    if min_sequence < 0 {
        inner.segments.values().next().cloned()
    } else {
        inner.segments.range(min_sequence..).next().map(|(_, s)| s.clone())
    }
}

fn first_covering_locked(inner: &Inner, position_us: i64) -> Option<Arc<SabrSegment>> {
    for segment in inner.segments.values() {
        if segment.end_us() > position_us {
            return Some(segment.clone());
        }
    }
    None
}
