//! A single media segment buffer.
//!
//! Segments are shared as `Arc<SabrSegment>` between the pump task (which
//! appends bytes as UMP `MEDIA` parts arrive) and consumer tasks (which read
//! bytes out). Bytes are appended into a `BytesMut`. On completion they are
//! frozen (zero-copy) into a shared `Bytes`, so a consumer can hand a
//! reference-counted slice straight to the sink without copying the payload.
//! `size`/`complete` are atomics so consumers can cheaply poll while awaiting.

use std::sync::atomic::{AtomicBool, AtomicI64, AtomicUsize, Ordering};

use bytes::{Bytes, BytesMut};
use parking_lot::Mutex;

use crate::format::SabrFormatKey;

const INITIAL_CAPACITY: usize = 32 * 1024;

pub struct SabrSegment {
    pub format_key: SabrFormatKey,
    pub sequence_number: i32,
    pub is_init: bool,
    pub start_us: i64,
    pub content_length: i32,
    pub start_ticks: i64,
    pub timescale: i32,

    duration_us: AtomicI64,
    duration_exact: AtomicBool,

    /// Bytes accumulated while downloading. Taken (zero-copy) into `frozen` on
    /// completion.
    data: Mutex<BytesMut>,
    /// The completed payload, frozen once so slices are reference-counted views
    /// rather than copies.
    frozen: Mutex<Option<Bytes>>,
    size: AtomicUsize,
    complete: AtomicBool,
}

impl SabrSegment {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        format_key: SabrFormatKey,
        sequence_number: i32,
        is_init: bool,
        start_us: i64,
        duration_us: i64,
        content_length: i32,
        start_ticks: i64,
        timescale: i32,
    ) -> Self {
        let capacity = if content_length > 0 {
            content_length as usize
        } else {
            INITIAL_CAPACITY
        };
        Self {
            format_key,
            sequence_number,
            is_init,
            start_us,
            content_length,
            start_ticks,
            timescale,
            duration_us: AtomicI64::new(duration_us),
            duration_exact: AtomicBool::new(false),
            data: Mutex::new(BytesMut::with_capacity(capacity)),
            frozen: Mutex::new(None),
            size: AtomicUsize::new(0),
            complete: AtomicBool::new(false),
        }
    }

    /// Segment duration in microseconds (0 if not yet known).
    pub fn duration_us(&self) -> i64 {
        self.duration_us.load(Ordering::Acquire)
    }

    pub fn duration_exact(&self) -> bool {
        self.duration_exact.load(Ordering::Acquire)
    }

    /// Update the segment duration. An exact duration is never overwritten by an
    /// inexact estimate.
    pub fn set_duration(&self, us: i64, exact: bool) {
        if us <= 0 {
            return;
        }
        if self.duration_exact.load(Ordering::Acquire) && !exact {
            return;
        }
        self.duration_us.store(us, Ordering::Release);
        self.duration_exact.store(exact, Ordering::Release);
    }

    pub fn size(&self) -> usize {
        self.size.load(Ordering::Acquire)
    }

    pub fn is_complete(&self) -> bool {
        self.complete.load(Ordering::Acquire)
    }

    /// Segment end position in microseconds (`start_us + duration_us`).
    pub fn end_us(&self) -> i64 {
        self.start_us + self.duration_us()
    }

    pub fn append(&self, bytes: &[u8]) {
        if bytes.is_empty() {
            return;
        }
        let mut data = self.data.lock();
        data.extend_from_slice(bytes);
        self.size.store(data.len(), Ordering::Release);
    }

    /// A reference-counted view of the segment bytes from `start`. Once the
    /// segment is complete this is zero-copy. Before then (rare) it copies the
    /// bytes buffered so far.
    pub fn bytes_from(&self, start: usize) -> Bytes {
        if let Some(frozen) = self.frozen.lock().as_ref() {
            return if start >= frozen.len() {
                Bytes::new()
            } else {
                frozen.slice(start..)
            };
        }
        let data = self.data.lock();
        if start >= data.len() {
            Bytes::new()
        } else {
            Bytes::copy_from_slice(&data[start..])
        }
    }

    /// A reference-counted view of the whole segment.
    pub fn bytes(&self) -> Bytes {
        self.bytes_from(0)
    }

    pub fn to_vec(&self) -> Vec<u8> {
        self.bytes().to_vec()
    }

    pub fn mark_complete(&self) {
        // Freeze the accumulated bytes once (zero-copy). `BytesMut::freeze` hands
        // the allocation to `Bytes`, and later slices are refcounted. Freeze
        // *before* flipping `complete`, so a reader that observes `is_complete()`
        // always sees the frozen bytes, never the empty gap between taking
        // `data` and publishing `frozen`. `frozen.is_some()` also guards against
        // a double `mark_complete` re-taking the (now empty) buffer.
        {
            let mut frozen = self.frozen.lock();
            if frozen.is_some() {
                return;
            }
            *frozen = Some(std::mem::take(&mut *self.data.lock()).freeze());
        }
        self.complete.store(true, Ordering::Release);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::format::SabrFormatKey;

    fn seg() -> SabrSegment {
        SabrSegment::new(SabrFormatKey::of(1, 0, None), 0, false, 0, 0, 0, 0, 0)
    }

    #[test]
    fn complete_implies_full_bytes() {
        let s = seg();
        s.append(b"moof");
        s.append(b"mdat-payload");
        assert!(!s.is_complete());

        s.mark_complete();
        // Invariant: once `is_complete()` is observable, the frozen bytes are
        // already published in full. There is no window where a reader sees
        // completion but empty/partial bytes (which previously fed qtdemux a
        // truncated moov).
        assert!(s.is_complete());
        assert_eq!(&s.bytes()[..], b"moofmdat-payload");
        assert_eq!(&s.bytes_from(4)[..], b"mdat-payload");
    }

    #[test]
    fn mark_complete_is_idempotent() {
        let s = seg();
        s.append(b"data");
        s.mark_complete();
        s.mark_complete(); // must not re-take the (now empty) buffer
        assert_eq!(&s.bytes()[..], b"data");
    }
}
