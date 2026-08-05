//! `ftestsink`: recording sink and sequence assertions. Owner: agent C.
//!
//! The element records every buffer, event and state transition it observes into one
//! ordered log. It changes nothing about the base class data path: `sync` keeps the
//! BaseSink default, no clock is provided, and no vmethod drops or reorders anything.

use std::{
    fmt,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use gst::{glib, prelude::*};
use parking_lot::{Condvar, Mutex};

use crate::registry;

pub const FACTORY_NAME: &str = "ftestsink";
/// Handle name used when `recording-key` carries no explicit slot.
pub const DEFAULT_HANDLE_NAME: &str = "recording";

/// Canonical `GstEventType` names, as stored in [`RecordEntry::Event::type_name`].
pub mod event_name {
    pub const STREAM_START: &str = "stream-start";
    pub const STREAM_COLLECTION: &str = "stream-collection";
    pub const CAPS: &str = "caps";
    pub const SEGMENT: &str = "segment";
    pub const FLUSH_START: &str = "flush-start";
    pub const FLUSH_STOP: &str = "flush-stop";
    pub const GAP: &str = "gap";
    pub const EOS: &str = "eos";
}

/// One observation, in arrival order. Buffer entries carry metadata only, never a
/// payload reference, so a recorded log keeps no buffer alive.
#[derive(Clone, Debug)]
pub enum RecordEntry {
    Buffer {
        pts: Option<gst::ClockTime>,
        dts: Option<gst::ClockTime>,
        duration: Option<gst::ClockTime>,
        flags: gst::BufferFlags,
        size: usize,
        monotonic: Instant,
    },
    /// The preroll buffer, recorded from the GstBaseSink `preroll` vfunc. The same
    /// buffer reaches `render` later, so it appears twice in the log.
    Preroll {
        pts: Option<gst::ClockTime>,
        monotonic: Instant,
    },
    Event {
        event_type: gst::EventType,
        /// `gst_event_type_get_name`, see [`event_name`].
        type_name: &'static str,
        seqnum: gst::Seqnum,
        sticky: bool,
        /// Only filled for events whose payload the harness cares about, and only
        /// while the `capture-details` property is set.
        details: Option<String>,
        monotonic: Instant,
    },
    StateChange {
        /// Recorded when the transition starts, before chaining up.
        transition: gst::StateChange,
        monotonic: Instant,
    },
}

impl RecordEntry {
    pub fn monotonic(&self) -> Instant {
        match self {
            Self::Buffer { monotonic, .. }
            | Self::Preroll { monotonic, .. }
            | Self::Event { monotonic, .. }
            | Self::StateChange { monotonic, .. } => *monotonic,
        }
    }

    pub fn is_buffer(&self) -> bool {
        matches!(self, Self::Buffer { .. })
    }

    /// Buffer or preroll: every way a buffer arrives at the sink.
    pub fn is_data(&self) -> bool {
        matches!(self, Self::Buffer { .. } | Self::Preroll { .. })
    }

    pub fn event_type(&self) -> Option<gst::EventType> {
        match self {
            Self::Event { event_type, .. } => Some(*event_type),
            _ => None,
        }
    }

    pub fn event_name(&self) -> Option<&'static str> {
        match self {
            Self::Event { type_name, .. } => Some(type_name),
            _ => None,
        }
    }

    pub fn is_event(&self, type_name: &str) -> bool {
        self.event_name() == Some(type_name)
    }

    pub fn pts(&self) -> Option<gst::ClockTime> {
        match self {
            Self::Buffer { pts, .. } | Self::Preroll { pts, .. } => *pts,
            _ => None,
        }
    }

    /// Buffer flags. `None` for anything that is not a rendered buffer, the
    /// preroll entry included. GstBaseSink hands the preroll vfunc the same
    /// buffer that reaches render, so its flags are read there.
    pub fn buffer_flags(&self) -> Option<gst::BufferFlags> {
        match self {
            Self::Buffer { flags, .. } => Some(*flags),
            _ => None,
        }
    }

    /// Synthetic entry for assertion tests and fuzz-shrinker logs.
    pub fn synthetic_buffer(pts: impl Into<Option<gst::ClockTime>>) -> Self {
        Self::synthetic_buffer_with_flags(pts, gst::BufferFlags::empty())
    }

    /// [`Self::synthetic_buffer`] with the flags a checker reads, see
    /// [`asserts::first_buffer_is_discont`].
    pub fn synthetic_buffer_with_flags(
        pts: impl Into<Option<gst::ClockTime>>,
        flags: gst::BufferFlags,
    ) -> Self {
        Self::Buffer {
            pts: pts.into(),
            dts: None,
            duration: None,
            flags,
            size: 0,
            monotonic: Instant::now(),
        }
    }

    /// Synthetic entry for assertion tests and fuzz-shrinker logs.
    pub fn synthetic_preroll(pts: impl Into<Option<gst::ClockTime>>) -> Self {
        Self::Preroll {
            pts: pts.into(),
            monotonic: Instant::now(),
        }
    }

    /// Synthetic entry for assertion tests and fuzz-shrinker logs.
    pub fn synthetic_event(event_type: gst::EventType) -> Self {
        Self::Event {
            event_type,
            type_name: event_type.name().as_str(),
            seqnum: gst::Seqnum::next(),
            sticky: event_type.is_sticky(),
            details: None,
            monotonic: Instant::now(),
        }
    }
}

impl fmt::Display for RecordEntry {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Buffer {
                pts, size, flags, ..
            } => write!(f, "buffer(pts={pts:?} size={size} flags={flags:?})"),
            Self::Preroll { pts, .. } => write!(f, "preroll(pts={pts:?})"),
            Self::Event {
                type_name, details, ..
            } => match details {
                Some(details) => write!(f, "event({type_name}: {details})"),
                None => write!(f, "event({type_name})"),
            },
            Self::StateChange { transition, .. } => write!(f, "state({transition:?})"),
        }
    }
}

/// Cloneable handle onto one sink's log. Every clone shares the same log.
#[derive(Clone, Default)]
pub struct Recording {
    inner: Arc<Inner>,
}

#[derive(Default)]
struct Inner {
    log: Mutex<Log>,
    changed: Condvar,
}

#[derive(Default)]
struct Log {
    entries: Vec<RecordEntry>,
    buffers: usize,
}

impl Recording {
    pub fn new() -> Self {
        Self::default()
    }

    /// Appends an entry and wakes every waiter. Public so tests and the fuzz driver
    /// can assemble synthetic logs through the same type.
    pub fn push(&self, entry: RecordEntry) {
        let mut log = self.inner.log.lock();
        if entry.is_buffer() {
            log.buffers += 1;
        }
        log.entries.push(entry);
        self.inner.changed.notify_all();
    }

    pub fn snapshot(&self) -> Vec<RecordEntry> {
        self.inner.log.lock().entries.clone()
    }

    pub fn len(&self) -> usize {
        self.inner.log.lock().entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Buffer entries only, prerolls excluded.
    pub fn buffer_count(&self) -> usize {
        self.inner.log.lock().buffers
    }

    pub fn event_count(&self, type_name: &str) -> usize {
        self.inner
            .log
            .lock()
            .entries
            .iter()
            .filter(|entry| entry.is_event(type_name))
            .count()
    }

    /// Drops every entry, counters included. A sink never clears by itself, not even
    /// across a state change, so a log survives teardown.
    pub fn clear(&self) {
        let mut log = self.inner.log.lock();
        log.entries.clear();
        log.buffers = 0;
    }

    /// Blocks until `pred` accepts the log or `timeout` expires. `pred` runs under the
    /// log lock and must not call back into this handle.
    pub fn wait_for<F>(&self, mut pred: F, timeout: Duration) -> bool
    where
        F: FnMut(&[RecordEntry]) -> bool,
    {
        let deadline = Instant::now() + timeout;
        let mut log = self.inner.log.lock();
        loop {
            if pred(&log.entries) {
                return true;
            }
            if self
                .inner
                .changed
                .wait_until(&mut log, deadline)
                .timed_out()
            {
                return pred(&log.entries);
            }
        }
    }

    pub fn wait_for_buffers(&self, count: usize, timeout: Duration) -> bool {
        self.wait_for(
            |entries| entries.iter().filter(|entry| entry.is_buffer()).count() >= count,
            timeout,
        )
    }

    pub fn wait_for_event(&self, type_name: &str, timeout: Duration) -> bool {
        self.wait_for(
            |entries| entries.iter().any(|entry| entry.is_event(type_name)),
            timeout,
        )
    }

    /// Every sequence invariant, run against the current log.
    pub fn check_invariants(&self) -> Result<(), String> {
        asserts::all(&self.snapshot())
    }
}

impl fmt::Debug for Recording {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let log = self.inner.log.lock();
        f.debug_struct("Recording")
            .field("entries", &log.entries.len())
            .field("buffers", &log.buffers)
            .finish()
    }
}

/// Splits a `recording-key` property value into scenario key and handle name.
/// `"scen1"` stashes under [`DEFAULT_HANDLE_NAME`], `"scen1/audio"` under `"audio"`.
pub fn split_recording_key(recording_key: &str) -> (&str, &str) {
    match recording_key.split_once('/') {
        Some((key, name)) if !name.is_empty() => (key, name),
        _ => (recording_key, DEFAULT_HANDLE_NAME),
    }
}

/// Fetches a recording a `recording-key` sink stashed in the scenario registry.
pub fn stashed_recording(recording_key: &str) -> Option<Recording> {
    let (key, name) = split_recording_key(recording_key);
    let handle = registry::lookup(key)?.handle::<Recording>(name)?;
    Some((*handle).clone())
}

/// Sequence invariants over a recorded (or synthetic) log. Every checker is total:
/// it reports the first violation with the entry indices that prove it.
pub mod asserts {
    use super::{RecordEntry, event_name};

    /// Runs every invariant that holds for any sink under any pipeline.
    ///
    /// [`first_buffer_is_discont`] is the one exception and is not called from
    /// here. Its own documentation says what it is measured against and why
    /// arming it everywhere would be arming an unvalidated rule.
    pub fn all(log: &[RecordEntry]) -> Result<(), String> {
        flush_pairs_matched(log)?;
        stream_start_before_caps(log)?;
        stream_start_before_segment(log)?;
        caps_before_first_buffer(log)?;
        segment_before_first_buffer(log)?;
        no_buffer_after_eos(log)?;
        no_stream_event_after_eos(log)?;
        eos_not_repeated(log)?;
        no_data_during_flush(log)?;
        nothing_serialized_during_flush(log)?;
        monotonic_pts_within_segment(log)?;
        Ok(())
    }

    /// Timed data: a buffer, a preroll, or the GAP event that stands in for one.
    /// All three are positioned by the segment and all three are refused by an
    /// EOS or flushing pad, so every rule about "data" covers the GAP too.
    fn is_timed_data(entry: &RecordEntry) -> bool {
        entry.is_data() || entry.is_event(event_name::GAP)
    }

    /// The serialized events that describe the stream itself rather than a moment
    /// in it. Each one travels the pad under the stream lock, so a flushing pad
    /// refuses it and an EOS pad discards it, exactly like a buffer.
    ///
    /// FLUSH_START and FLUSH_STOP are deliberately absent. They are the events
    /// that open and close a flush, and FLUSH_STOP is the one serialized event a
    /// flushing pad accepts. STREAM_START is absent from the EOS rule and present
    /// in the flush rule, because it is what CLEARS an EOS but is still refused
    /// mid-flush. The two callers filter it themselves.
    fn is_stream_event(entry: &RecordEntry) -> bool {
        matches!(
            entry.event_name(),
            Some(event_name::CAPS | event_name::SEGMENT | event_name::STREAM_COLLECTION)
        )
    }

    /// Every FLUSH_START is followed by a FLUSH_STOP before the next FLUSH_START and
    /// before the log ends. A FLUSH_STOP without a recorded FLUSH_START is legal: the
    /// log may begin (or have been cleared) mid-flush.
    pub fn flush_pairs_matched(log: &[RecordEntry]) -> Result<(), String> {
        let mut open: Option<usize> = None;
        for (index, entry) in log.iter().enumerate() {
            match entry.event_name() {
                Some(event_name::FLUSH_START) => {
                    if let Some(start) = open {
                        return Err(format!(
                            "flush_pairs_matched: entry {index} flush-start while the \
                             flush-start at entry {start} is still unmatched"
                        ));
                    }
                    open = Some(index);
                }
                Some(event_name::FLUSH_STOP) => open = None,
                _ => {}
            }
        }
        match open {
            Some(start) => Err(format!(
                "flush_pairs_matched: entry {start} flush-start never matched by a \
                 flush-stop ({} entries recorded after it)",
                log.len() - start - 1
            )),
            None => Ok(()),
        }
    }

    /// A SEGMENT precedes the first buffer, and a fresh SEGMENT precedes the first
    /// buffer after every FLUSH_STOP (a flush drops the sticky segment). A GAP is
    /// positioned by the segment exactly like a buffer, and `gst_pad_push_event`
    /// pushes the pending sticky events ahead of any serialized event, so a GAP
    /// that arrives first is the same violation.
    pub fn segment_before_first_buffer(log: &[RecordEntry]) -> Result<(), String> {
        let mut have_segment = false;
        for (index, entry) in log.iter().enumerate() {
            match entry.event_name() {
                Some(event_name::SEGMENT) => have_segment = true,
                Some(event_name::FLUSH_STOP) => have_segment = false,
                _ => {}
            }
            if is_timed_data(entry) && !have_segment {
                return Err(format!(
                    "segment_before_first_buffer: entry {index} {entry} arrived with no \
                     preceding segment{}",
                    last_flush_stop_note(log, index)
                ));
            }
        }
        Ok(())
    }

    /// A CAPS event precedes the first buffer. Caps stay sticky across a flush, so
    /// only the first buffer of the log is checked.
    ///
    /// A GAP counts, and it has to. `gst_pad_push_event` pushes the pending sticky
    /// events ahead of any serialized event, so a GAP reaching an uncapsed pad is
    /// the same violation a buffer would be, and a sparse stream (every text
    /// branch here) opens with a GAP rather than a buffer, which is precisely the
    /// case a buffer-only rule never inspects.
    pub fn caps_before_first_buffer(log: &[RecordEntry]) -> Result<(), String> {
        let mut have_caps = false;
        for (index, entry) in log.iter().enumerate() {
            if entry.is_event(event_name::CAPS) {
                have_caps = true;
            } else if is_timed_data(entry) {
                if !have_caps {
                    return Err(format!(
                        "caps_before_first_buffer: entry {index} {entry} arrived before any \
                         caps event"
                    ));
                }
                return Ok(());
            }
        }
        Ok(())
    }

    /// No buffer or preroll whose PTS is behind the one before it, inside a single
    /// segment. A sink renders in arrival order and positions every entry by the
    /// current segment, so a PTS that moves backwards is a stream the sink cannot
    /// present as it received it.
    ///
    /// Reset by SEGMENT, FLUSH_STOP and STREAM_START, which are the three ways the
    /// timeline legally restarts. A seek back to zero, a gapless item boundary and
    /// a replayed external subtitle all reopen the timeline with one of them, and
    /// none of those is a violation. A buffer with no PTS is skipped rather than
    /// treated as zero, and does not lower the bound.
    ///
    /// GAP is out of scope, and only because [`RecordEntry::Event`] keeps a gap's
    /// timestamps in the optional `details` string rather than a field. Widen the
    /// entry before widening this rule, do not parse the string.
    pub fn monotonic_pts_within_segment(log: &[RecordEntry]) -> Result<(), String> {
        let mut last: Option<(usize, gst::ClockTime)> = None;
        for (index, entry) in log.iter().enumerate() {
            if matches!(
                entry.event_name(),
                Some(event_name::SEGMENT | event_name::FLUSH_STOP | event_name::STREAM_START)
            ) {
                last = None;
                continue;
            }
            let Some(pts) = entry.pts() else { continue };
            if let Some((previous_index, previous)) = last
                && pts < previous
            {
                return Err(format!(
                    "monotonic_pts_within_segment: entry {index} {entry} moves the \
                     timeline backwards from {previous} at entry {previous_index}, with \
                     no segment, flush-stop or stream-start between them"
                ));
            }
            last = Some((index, pts));
        }
        Ok(())
    }

    /// No buffer arrives after an EOS. STREAM_START and FLUSH_STOP clear the EOS
    /// state, the same way GstBaseSink does. GAP counts as data: `gstpad.c`
    /// discards every serialized event that reaches an EOS pad ("Received event
    /// on EOS pad. Discarding"), so one in the log means the EOS was not honoured.
    pub fn no_buffer_after_eos(log: &[RecordEntry]) -> Result<(), String> {
        let mut eos_at: Option<usize> = None;
        for (index, entry) in log.iter().enumerate() {
            match entry.event_name() {
                Some(event_name::EOS) => {
                    eos_at = Some(index);
                    continue;
                }
                Some(event_name::FLUSH_STOP | event_name::STREAM_START) => {
                    eos_at = None;
                    continue;
                }
                _ => {}
            }
            if is_timed_data(entry)
                && let Some(eos) = eos_at
            {
                return Err(format!(
                    "no_buffer_after_eos: entry {index} {entry} arrived after the \
                     eos at entry {eos}"
                ));
            }
        }
        Ok(())
    }

    /// Nothing that describes the stream reaches a pad that is already EOS
    /// either. `gst_pad_send_event_unchecked` discards every serialized event on
    /// an EOS pad ("Received event on EOS pad. Discarding"), so a CAPS, SEGMENT or
    /// STREAM_COLLECTION recorded after an EOS did not travel a correct pad.
    ///
    /// The companion to [`no_buffer_after_eos`], which covers the data half. The
    /// two clear on the same events. A STREAM_START starts a new stream and a
    /// FLUSH_STOP resets the pad, and both legitimately reopen it.
    ///
    /// This one has teeth on its own. A gapless handoff and a subtitle replay
    /// both re-caps and re-segment a live sink, and getting that ordering wrong
    /// against the outgoing item's EOS produces exactly this log, with no buffer
    /// out of place for the data rule to catch.
    pub fn no_stream_event_after_eos(log: &[RecordEntry]) -> Result<(), String> {
        let mut eos_at: Option<usize> = None;
        for (index, entry) in log.iter().enumerate() {
            match entry.event_name() {
                Some(event_name::EOS) => {
                    eos_at = Some(index);
                    continue;
                }
                Some(event_name::FLUSH_STOP | event_name::STREAM_START) => {
                    eos_at = None;
                    continue;
                }
                _ => {}
            }
            if is_stream_event(entry)
                && let Some(eos) = eos_at
            {
                return Err(format!(
                    "no_stream_event_after_eos: entry {index} {entry} arrived after the \
                     eos at entry {eos}, with no stream-start or flush-stop to reopen \
                     the pad"
                ));
            }
        }
        Ok(())
    }

    /// One EOS per stream. A pad that is already EOS drops the sticky event
    /// (`store_sticky_event` returns `GST_FLOW_EOS`), so a second EOS with no
    /// STREAM_START or FLUSH_STOP between them cannot reach a sink in the field:
    /// seeing one means the log was assembled, not observed, or the sink was fed
    /// by something that bypasses the pad.
    pub fn eos_not_repeated(log: &[RecordEntry]) -> Result<(), String> {
        let mut eos_at: Option<usize> = None;
        for (index, entry) in log.iter().enumerate() {
            match entry.event_name() {
                Some(event_name::EOS) => {
                    if let Some(first) = eos_at {
                        return Err(format!(
                            "eos_not_repeated: entry {index} eos while the eos at entry \
                             {first} was never cleared by a stream-start or flush-stop"
                        ));
                    }
                    eos_at = Some(index);
                }
                Some(event_name::FLUSH_STOP | event_name::STREAM_START) => eos_at = None,
                _ => {}
            }
        }
        Ok(())
    }

    /// A pad that is flushing refuses data, so a FLUSH_START/FLUSH_STOP window in
    /// the log must be empty of buffers, prerolls and gaps.
    ///
    /// One exception, and it is the reason this is a count and not a flag:
    /// `gst_pad_send_event_unchecked` sets the pad's flushing flag BEFORE it calls
    /// the event function, and [`super::imp`] records the FLUSH_START from that
    /// event function before chaining up. A chain call that had already passed the
    /// flushing check is holding the stream lock and still finishes, so exactly one
    /// in-flight render (or the preroll it turns into, never both: the preroll wait
    /// returns FLUSHING instead of reaching render) can be recorded after the
    /// FLUSH_START. Every later chain call is refused by the pad itself, so a
    /// SECOND entry inside the window means data really did flow through a flush.
    pub fn no_data_during_flush(log: &[RecordEntry]) -> Result<(), String> {
        /// See [`no_data_during_flush`]: the render that was already in flight.
        const IN_FLIGHT: usize = 1;

        let mut open: Option<usize> = None;
        let mut during: Vec<usize> = Vec::new();
        for (index, entry) in log.iter().enumerate() {
            match entry.event_name() {
                Some(event_name::FLUSH_START) => {
                    open = Some(index);
                    during.clear();
                    continue;
                }
                Some(event_name::FLUSH_STOP) => {
                    open = None;
                    during.clear();
                    continue;
                }
                _ => {}
            }
            let Some(start) = open else { continue };
            if !is_timed_data(entry) {
                continue;
            }
            during.push(index);
            if during.len() > IN_FLIGHT {
                return Err(format!(
                    "no_data_during_flush: entries {during:?} arrived between the \
                     flush-start at entry {start} and its flush-stop; at most {IN_FLIGHT} \
                     in-flight render can be recorded after a flush-start"
                ));
            }
        }
        Ok(())
    }

    /// A flushing pad refuses every SERIALIZED delivery, not just data, and the
    /// whole flush window shares ONE in-flight slot.
    ///
    /// [`no_data_during_flush`] allows one in-flight render inside the window, for
    /// the race described there. The same race exists for a serialized event:
    /// `gst_pad_send_event_unchecked` takes the stream lock, re-checks the
    /// flushing flag under it, releases the object lock and only then calls the
    /// event function, so a FLUSH_START recorded in between produces a legal
    /// `flush-start, caps` log. What the race CANNOT produce is two of them. The
    /// stream lock is held for the whole of either a chain call or a serialized
    /// event, so at most one dispatch of any kind is ever past the check.
    ///
    /// That single shared slot is the entire point of this checker. Data alone and
    /// events alone are each already bounded at one. A buffer AND an EOS inside one
    /// window passes both of those rules and is still impossible.
    ///
    /// The counted entries are buffers, prerolls, GAP, STREAM_START, CAPS, SEGMENT,
    /// STREAM_COLLECTION and EOS. FLUSH_STOP is the one serialized event a
    /// flushing pad accepts, and it is the terminator. StateChange entries are
    /// exempt. They come from the state-change vfunc on whichever thread drives
    /// the pipeline, never touch the pad, and a flushing seek across a state
    /// change legitimately puts one here.
    pub fn nothing_serialized_during_flush(log: &[RecordEntry]) -> Result<(), String> {
        /// The one dispatch that was already past the flushing check.
        const IN_FLIGHT: usize = 1;

        let mut open: Option<usize> = None;
        let mut during: Vec<usize> = Vec::new();
        for (index, entry) in log.iter().enumerate() {
            match entry.event_name() {
                Some(event_name::FLUSH_START) => {
                    open = Some(index);
                    during.clear();
                    continue;
                }
                Some(event_name::FLUSH_STOP) => {
                    open = None;
                    during.clear();
                    continue;
                }
                _ => {}
            }
            let Some(start) = open else { continue };
            let serialized = is_timed_data(entry)
                || is_stream_event(entry)
                || matches!(
                    entry.event_name(),
                    Some(event_name::STREAM_START | event_name::EOS)
                );
            if !serialized {
                continue;
            }
            during.push(index);
            if during.len() > IN_FLIGHT {
                return Err(format!(
                    "nothing_serialized_during_flush: entries {during:?} arrived between \
                     the flush-start at entry {start} and its flush-stop; a flushing pad \
                     refuses every serialized delivery and the stream lock leaves at most \
                     {IN_FLIGHT} of them already dispatched"
                ));
            }
        }
        Ok(())
    }

    /// A STREAM_START precedes the first SEGMENT event, the way it precedes the
    /// first CAPS. Sticky events travel a pad in sticky order (STREAM_START, then
    /// CAPS, then STREAM_COLLECTION, then SEGMENT), so a segment ahead of the
    /// stream-start is a misordering GStreamer itself warns about.
    ///
    /// Without this, a log whose stream-start is MISSING or late is only caught
    /// through the caps rule, and a log that carries no caps event at all (or
    /// carries it after the segment) slips past that one entirely.
    pub fn stream_start_before_segment(log: &[RecordEntry]) -> Result<(), String> {
        let mut have_stream_start = false;
        for (index, entry) in log.iter().enumerate() {
            match entry.event_name() {
                Some(event_name::STREAM_START) => have_stream_start = true,
                Some(event_name::SEGMENT) if !have_stream_start => {
                    return Err(format!(
                        "stream_start_before_segment: entry {index} segment arrived before \
                         any stream-start event"
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// A STREAM_START precedes the first CAPS event.
    pub fn stream_start_before_caps(log: &[RecordEntry]) -> Result<(), String> {
        let mut have_stream_start = false;
        for (index, entry) in log.iter().enumerate() {
            match entry.event_name() {
                Some(event_name::STREAM_START) => have_stream_start = true,
                Some(event_name::CAPS) if !have_stream_start => {
                    return Err(format!(
                        "stream_start_before_caps: entry {index} caps arrived before any \
                         stream-start event"
                    ));
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// The first buffer a sink renders after the stream opened under it carries
    /// GST_BUFFER_FLAG_DISCONT. Both decoder base classes start with their
    /// `discont` flag set and clear it on the first output, so a stream that
    /// begins at a sink without one was not started, it was continued.
    ///
    /// DELIBERATELY NOT IN [`all`], and this is not an oversight.
    ///
    /// * Measured across every scenario file, 47 sink logs, the first buffer
    ///   carried DISCONT in all 47. That is the only form with evidence behind
    ///   it, so it is the only form implemented.
    /// * "Only the first buffer is DISCONT" is FALSE. `mixed_pacing.toml` has
    ///   DISCONT on nine of eleven video buffers, because a realtime source that
    ///   starves the branch really does produce discontinuities. Do not add a rule
    ///   that counts them.
    /// * The two stronger forms, one per STREAM_START and one after every
    ///   FLUSH_STOP, are the ones that would catch a chain reused across a load or
    ///   a gapless handoff. Neither is measured here, because no scenario file
    ///   seeks mid-play or loads twice. Putting either in `all` would arm an
    ///   unvalidated rule inside the gapless and fuzz suites, where a false
    ///   positive lands in tests that are already flaky. Measure first, then widen.
    ///
    /// Skipped entirely when the log does not open with a STREAM_START ahead of
    /// its first buffer, which is a log that was cleared or attached mid-stream
    /// and never saw the stream begin.
    pub fn first_buffer_is_discont(log: &[RecordEntry]) -> Result<(), String> {
        let Some(first_buffer) = log.iter().position(RecordEntry::is_buffer) else {
            return Ok(());
        };
        let opened = log[..first_buffer]
            .iter()
            .any(|entry| entry.is_event(event_name::STREAM_START));
        if !opened {
            return Ok(());
        }
        let flags = log[first_buffer].buffer_flags().unwrap_or_else(|| {
            unreachable!("position() selected a buffer entry");
        });
        if flags.contains(gst::BufferFlags::DISCONT) {
            return Ok(());
        }
        Err(format!(
            "first_buffer_is_discont: entry {first_buffer} {} is the first buffer of a \
             stream this sink watched open, and it carries no DISCONT flag (flags={flags:?}), \
             so it continues a flow that was already running",
            log[first_buffer]
        ))
    }

    fn last_flush_stop_note(log: &[RecordEntry], before: usize) -> String {
        match log[..before]
            .iter()
            .rposition(|entry| entry.is_event(event_name::FLUSH_STOP))
        {
            Some(index) => format!(" (segment expected after the flush-stop at entry {index})"),
            None => String::new(),
        }
    }
}

mod imp {
    use super::*;

    use gst::{glib::translate::IntoGlib, subclass::prelude::*};
    use gst_base::subclass::prelude::*;

    static CAT: std::sync::LazyLock<gst::DebugCategory> = std::sync::LazyLock::new(|| {
        gst::DebugCategory::new(
            FACTORY_NAME,
            gst::DebugColorFlags::empty(),
            Some("FCast test recording sink"),
        )
    });

    pub struct FTestSink {
        recording: Recording,
        recording_key: Mutex<Option<String>>,
        /// Per-entry gst logging, off by default so the record path never formats.
        silent: AtomicBool,
        capture_details: AtomicBool,
        /// See the `stall-transition` property.
        stall_transition: Mutex<Option<String>>,
        stall_ms: AtomicU64,
        /// Name of the thread that entered the stall, published through the
        /// read-only `stalled-thread` property. Empty until the stall engages,
        /// which is also how a test detects that it engaged at all.
        stalled_thread: Mutex<Option<String>>,
    }

    impl Default for FTestSink {
        fn default() -> Self {
            Self {
                recording: Recording::new(),
                recording_key: Mutex::new(None),
                silent: AtomicBool::new(true),
                capture_details: AtomicBool::new(true),
                stall_transition: Mutex::new(None),
                stall_ms: AtomicU64::new(0),
                stalled_thread: Mutex::new(None),
            }
        }
    }

    impl FTestSink {
        pub fn recording(&self) -> Recording {
            self.recording.clone()
        }

        fn record(&self, entry: RecordEntry) {
            if !self.silent.load(Ordering::Relaxed) {
                gst::debug!(CAT, imp = self, "recorded {entry}");
            }
            self.recording.push(entry);
        }

        pub fn record_preroll(&self, buffer: &gst::BufferRef) {
            self.record(RecordEntry::Preroll {
                pts: buffer.pts(),
                monotonic: Instant::now(),
            });
        }

        /// Publishes the handle for scenarios whose sink the test cannot reach.
        /// Returns false when nothing is registered under the key yet, which is
        /// expected when the property is set before the scenario.
        fn stash_recording(&self) -> bool {
            let Some(key) = self.recording_key.lock().clone() else {
                return true;
            };
            let (scenario_key, handle_name) = split_recording_key(&key);
            let Some(scenario) = registry::lookup(scenario_key) else {
                return false;
            };
            scenario.set_handle(handle_name, Arc::new(self.recording.clone()));
            true
        }
    }

    #[glib::object_subclass]
    impl ObjectSubclass for FTestSink {
        const NAME: &'static str = "FCastTestSink";
        type Type = super::FTestSink;
        type ParentType = gst_base::BaseSink;

        fn class_init(klass: &mut Self::Class) {
            // gstreamer-rs does not wrap the GstBaseSink preroll vfunc and GstBaseSink
            // leaves it unset, so install it by hand. SAFETY: Self::Class is
            // #[repr(C)] with GstBaseSinkClass as its first field.
            let base_sink_class = unsafe {
                &mut *(klass as *mut Self::Class).cast::<gst_base::ffi::GstBaseSinkClass>()
            };
            base_sink_class.preroll = Some(preroll_trampoline);
        }
    }

    unsafe extern "C" fn preroll_trampoline(
        ptr: *mut gst_base::ffi::GstBaseSink,
        buffer: *mut gst::ffi::GstBuffer,
    ) -> gst::ffi::GstFlowReturn {
        // SAFETY: GstBaseSink calls this with our own instance and a valid buffer.
        let imp = unsafe { (*(ptr as *mut <FTestSink as ObjectSubclass>::Instance)).imp() };
        let buffer = unsafe { gst::BufferRef::from_ptr(buffer) };
        gst::panic_to_error!(imp, gst::FlowReturn::Error, {
            imp.record_preroll(buffer);
            gst::FlowReturn::Ok
        })
        .into_glib()
    }

    impl ObjectImpl for FTestSink {
        fn properties() -> &'static [glib::ParamSpec] {
            static PROPERTIES: std::sync::LazyLock<Vec<glib::ParamSpec>> =
                std::sync::LazyLock::new(|| {
                    vec![
                        glib::ParamSpecString::builder("recording-key")
                            .nick("Recording key")
                            .blurb(
                                "Scenario registry key (optionally <key>/<handle>) to stash the \
                                 recording handle under",
                            )
                            .readwrite()
                            .build(),
                        glib::ParamSpecBoolean::builder("silent")
                            .nick("Silent")
                            .blurb("Do not log every recorded entry")
                            .default_value(true)
                            .readwrite()
                            .build(),
                        glib::ParamSpecBoolean::builder("capture-details")
                            .nick("Capture details")
                            .blurb("Record a detail string for events that carry a payload")
                            .default_value(true)
                            .readwrite()
                            .build(),
                        glib::ParamSpecString::builder("stall-transition")
                            .nick("Stall transition")
                            .blurb(
                                "GstStateChange to stall in ONCE, spelled as its Rust debug name \
                                 (\"ReadyToPaused\"). Manufactures a slow state change: a real \
                                 window-bound video sink can take seconds here, and the caller of \
                                 set_state waits inside it.",
                            )
                            .readwrite()
                            .build(),
                        glib::ParamSpecUInt64::builder("stall-ms")
                            .nick("Stall milliseconds")
                            .blurb("How long the stall holds. 0 disables it.")
                            .default_value(0)
                            .readwrite()
                            .build(),
                        glib::ParamSpecString::builder("stalled-thread")
                            .nick("Stalled thread")
                            .blurb(
                                "Name of the thread that entered the stall, empty until it does. \
                                 Reading it is how a test learns WHICH thread made the blocking \
                                 set_state call.",
                            )
                            .read_only()
                            .build(),
                    ]
                });
            PROPERTIES.as_ref()
        }

        fn set_property(&self, _id: usize, value: &glib::Value, pspec: &glib::ParamSpec) {
            match pspec.name() {
                "recording-key" => {
                    *self.recording_key.lock() = value.get().expect("type checked upstream");
                    self.stash_recording();
                }
                "silent" => self.silent.store(
                    value.get().expect("type checked upstream"),
                    Ordering::Relaxed,
                ),
                "capture-details" => self.capture_details.store(
                    value.get().expect("type checked upstream"),
                    Ordering::Relaxed,
                ),
                "stall-transition" => {
                    *self.stall_transition.lock() = value.get().expect("type checked upstream");
                }
                "stall-ms" => self.stall_ms.store(
                    value.get().expect("type checked upstream"),
                    Ordering::Relaxed,
                ),
                other => unimplemented!("unknown property {other}"),
            }
        }

        fn property(&self, _id: usize, pspec: &glib::ParamSpec) -> glib::Value {
            match pspec.name() {
                "recording-key" => self.recording_key.lock().to_value(),
                "silent" => self.silent.load(Ordering::Relaxed).to_value(),
                "capture-details" => self.capture_details.load(Ordering::Relaxed).to_value(),
                "stall-transition" => self.stall_transition.lock().to_value(),
                "stall-ms" => self.stall_ms.load(Ordering::Relaxed).to_value(),
                "stalled-thread" => self.stalled_thread.lock().to_value(),
                other => unimplemented!("unknown property {other}"),
            }
        }
    }

    impl GstObjectImpl for FTestSink {}

    impl ElementImpl for FTestSink {
        fn metadata() -> Option<&'static gst::subclass::ElementMetadata> {
            static METADATA: std::sync::OnceLock<gst::subclass::ElementMetadata> =
                std::sync::OnceLock::new();
            Some(METADATA.get_or_init(|| {
                gst::subclass::ElementMetadata::new(
                    "FCast test recording sink",
                    "Sink",
                    "Records every buffer, event and state transition for test assertions",
                    "FCast",
                )
            }))
        }

        fn pad_templates() -> &'static [gst::PadTemplate] {
            static TEMPLATES: std::sync::OnceLock<Vec<gst::PadTemplate>> =
                std::sync::OnceLock::new();
            TEMPLATES.get_or_init(|| {
                vec![
                    gst::PadTemplate::new(
                        "sink",
                        gst::PadDirection::Sink,
                        gst::PadPresence::Always,
                        &gst::Caps::new_any(),
                    )
                    .unwrap(),
                ]
            })
        }

        fn change_state(
            &self,
            transition: gst::StateChange,
        ) -> Result<gst::StateChangeSuccess, gst::StateChangeError> {
            // Recorded before chaining up so the entry precedes whatever the
            // transition sets in motion.
            self.record(RecordEntry::StateChange {
                transition,
                monotonic: Instant::now(),
            });
            // Manufactured slow state change, see `stall-transition`. Held
            // BEFORE chaining up so the caller of `set_state` is inside the
            // element's own transition, exactly where a window-bound sink
            // waits on its display server. Once only: the release path of
            // whatever the test wedged must not be stalled again.
            let stall_ms = self.stall_ms.load(Ordering::Relaxed);
            let wanted = self.stall_transition.lock().clone();
            let stall =
                stall_ms > 0 && wanted.as_deref() == Some(format!("{transition:?}").as_str());
            if stall {
                // The OS thread name, not the Rust one: the threads that matter
                // here are GStreamer's (a multiqueue slot task, a source task),
                // which Rust reports as unnamed. `/proc/thread-self` is the
                // CURRENT thread's directory, so this names the caller exactly.
                // The Rust name is the fallback and identifies the crate's own
                // threads on any platform.
                let thread = std::thread::current();
                let name = std::fs::read_to_string("/proc/thread-self/comm")
                    .map(|comm| comm.trim().to_owned())
                    .ok()
                    .filter(|comm| !comm.is_empty())
                    .or_else(|| thread.name().map(str::to_owned))
                    .unwrap_or_else(|| format!("unnamed-{:?}", thread.id()));
                let engage = {
                    let mut stalled = self.stalled_thread.lock();
                    let first = stalled.is_none();
                    if first {
                        *stalled = Some(name.clone());
                    }
                    first
                };
                if engage {
                    let held = Duration::from_millis(stall_ms);
                    gst::warning!(
                        CAT,
                        imp = self,
                        "stalling {transition:?} for {held:?} on thread {name}"
                    );
                    std::thread::sleep(held);
                }
            }
            if transition == gst::StateChange::NullToReady && !self.stash_recording() {
                // Retried here because a scenario may be registered after the
                // property is set. Still missing by now is a test bug.
                gst::warning!(
                    CAT,
                    imp = self,
                    "recording-key {:?} has no registered scenario",
                    self.recording_key.lock()
                );
            }
            self.parent_change_state(transition)
        }
    }

    impl BaseSinkImpl for FTestSink {
        fn render(&self, buffer: &gst::Buffer) -> Result<gst::FlowSuccess, gst::FlowError> {
            self.record(RecordEntry::Buffer {
                pts: buffer.pts(),
                dts: buffer.dts(),
                duration: buffer.duration(),
                flags: buffer.flags(),
                size: buffer.size(),
                monotonic: Instant::now(),
            });
            Ok(gst::FlowSuccess::Ok)
        }

        fn event(&self, event: gst::Event) -> bool {
            // Recorded before chaining up: the parent handles EOS by draining and
            // posting, and a flush-stop resets state, both without returning here.
            let event_type = event.type_();
            let details = self
                .capture_details
                .load(Ordering::Relaxed)
                .then(|| event_details(&event))
                .flatten();
            self.record(RecordEntry::Event {
                event_type,
                type_name: event_type.name().as_str(),
                seqnum: event.seqnum(),
                sticky: event_type.is_sticky(),
                details,
                monotonic: Instant::now(),
            });
            self.parent_event(event)
        }
    }

    /// Detail strings for the events the harness reasons about. Everything else stays
    /// allocation free.
    fn event_details(event: &gst::Event) -> Option<String> {
        match event.view() {
            gst::EventView::StreamStart(e) => Some(format!("stream-id={}", e.stream_id())),
            gst::EventView::Caps(e) => Some(e.caps().to_string()),
            gst::EventView::Segment(e) => Some(format!("{:?}", e.segment())),
            gst::EventView::Gap(e) => {
                let (pts, duration) = e.get();
                Some(format!("pts={pts} duration={duration:?}"))
            }
            gst::EventView::StreamCollection(e) => {
                Some(format!("streams={}", e.stream_collection().len()))
            }
            gst::EventView::StreamGroupDone(e) => Some(format!("group-id={:?}", e.group_id())),
            gst::EventView::SegmentDone(e) => Some(format!("position={:?}", e.get())),
            gst::EventView::Latency(e) => Some(format!("latency={}", e.latency())),
            _ => None,
        }
    }
}

glib::wrapper! {
    pub struct FTestSink(ObjectSubclass<imp::FTestSink>)
        @extends gst_base::BaseSink, gst::Element, gst::Object;
}

impl FTestSink {
    /// Requires [`crate::register_for_tests`] (or at least `gst::init`) first.
    pub fn new() -> Self {
        glib::Object::builder().build()
    }

    /// The log this sink records into. Cheap to clone, shared with every other handle.
    pub fn recording(&self) -> Recording {
        use gst::subclass::prelude::ObjectSubclassIsExt;
        self.imp().recording()
    }
}

impl Default for FTestSink {
    fn default() -> Self {
        Self::new()
    }
}

/// Rank NONE: tests select the sink explicitly, autoplugging never should.
pub fn register() -> Result<(), gst::glib::BoolError> {
    gst::Element::register(
        None,
        FACTORY_NAME,
        gst::Rank::NONE,
        FTestSink::static_type(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn event(type_name: gst::EventType) -> RecordEntry {
        RecordEntry::synthetic_event(type_name)
    }

    fn buffer() -> RecordEntry {
        RecordEntry::synthetic_buffer(gst::ClockTime::ZERO)
    }

    fn legal_prefix() -> Vec<RecordEntry> {
        vec![
            event(gst::EventType::StreamStart),
            event(gst::EventType::Caps),
            event(gst::EventType::Segment),
            buffer(),
        ]
    }

    #[test]
    fn event_names_match_gstreamer() {
        gst::init().unwrap();
        assert_eq!(gst::EventType::FlushStart.name(), event_name::FLUSH_START);
        assert_eq!(gst::EventType::FlushStop.name(), event_name::FLUSH_STOP);
        assert_eq!(gst::EventType::StreamStart.name(), event_name::STREAM_START);
        assert_eq!(gst::EventType::Caps.name(), event_name::CAPS);
        assert_eq!(gst::EventType::Segment.name(), event_name::SEGMENT);
        assert_eq!(gst::EventType::Gap.name(), event_name::GAP);
        assert_eq!(gst::EventType::Eos.name(), event_name::EOS);
        assert_eq!(
            gst::EventType::StreamCollection.name(),
            event_name::STREAM_COLLECTION
        );
    }

    #[test]
    fn legal_log_passes_every_invariant() {
        gst::init().unwrap();
        let mut log = legal_prefix();
        log.push(event(gst::EventType::FlushStart));
        log.push(event(gst::EventType::FlushStop));
        log.push(event(gst::EventType::Segment));
        log.push(buffer());
        log.push(event(gst::EventType::Eos));
        asserts::all(&log).expect("legal log");
    }

    #[test]
    fn flush_violations_are_reported_with_indices() {
        gst::init().unwrap();
        let unmatched = vec![
            event(gst::EventType::FlushStart),
            buffer(),
            event(gst::EventType::FlushStart),
        ];
        let err = asserts::flush_pairs_matched(&unmatched).expect_err("nested flush-start");
        assert!(err.contains("entry 2"), "{err}");
        assert!(err.contains("entry 0"), "{err}");

        let dangling = vec![event(gst::EventType::FlushStart)];
        let err = asserts::flush_pairs_matched(&dangling).expect_err("dangling flush-start");
        assert!(err.contains("never matched"), "{err}");

        // A flush-stop with no recorded flush-start is legal.
        asserts::flush_pairs_matched(&[event(gst::EventType::FlushStop)]).expect("stray stop");
    }

    #[test]
    fn ordering_violations_are_reported() {
        gst::init().unwrap();
        let no_segment = vec![
            event(gst::EventType::StreamStart),
            event(gst::EventType::Caps),
            buffer(),
        ];
        let err =
            asserts::segment_before_first_buffer(&no_segment).expect_err("buffer without segment");
        assert!(err.contains("entry 2"), "{err}");

        let mut after_flush = legal_prefix();
        after_flush.push(event(gst::EventType::FlushStart));
        after_flush.push(event(gst::EventType::FlushStop));
        after_flush.push(buffer());
        let err = asserts::segment_before_first_buffer(&after_flush)
            .expect_err("buffer after flush without segment");
        assert!(err.contains("flush-stop at entry 5"), "{err}");

        let no_caps = vec![event(gst::EventType::StreamStart), buffer()];
        let err = asserts::caps_before_first_buffer(&no_caps).expect_err("buffer without caps");
        assert!(err.contains("entry 1"), "{err}");

        let caps_first = vec![event(gst::EventType::Caps)];
        let err =
            asserts::stream_start_before_caps(&caps_first).expect_err("caps without stream-start");
        assert!(err.contains("entry 0"), "{err}");

        let mut after_eos = legal_prefix();
        after_eos.push(event(gst::EventType::Eos));
        after_eos.push(buffer());
        let err = asserts::no_buffer_after_eos(&after_eos).expect_err("buffer after eos");
        assert!(err.contains("entry 5"), "{err}");
        assert!(err.contains("entry 4"), "{err}");

        // Stream-start clears the eos state, the way GstBaseSink does.
        let mut restarted = legal_prefix();
        restarted.push(event(gst::EventType::Eos));
        restarted.push(event(gst::EventType::StreamStart));
        restarted.push(event(gst::EventType::Segment));
        restarted.push(buffer());
        asserts::no_buffer_after_eos(&restarted).expect("restarted stream");
    }

    #[test]
    fn recording_counts_and_waits() {
        gst::init().unwrap();
        let recording = Recording::new();
        assert!(recording.is_empty());
        recording.push(event(gst::EventType::StreamStart));
        recording.push(buffer());
        recording.push(RecordEntry::Preroll {
            pts: None,
            monotonic: Instant::now(),
        });
        assert_eq!(recording.len(), 3);
        assert_eq!(recording.buffer_count(), 1);
        assert_eq!(recording.event_count(event_name::STREAM_START), 1);
        assert!(recording.wait_for_buffers(1, Duration::ZERO));
        assert!(!recording.wait_for_buffers(2, Duration::from_millis(1)));
        assert!(recording.wait_for_event(event_name::STREAM_START, Duration::ZERO));
        assert!(!recording.wait_for_event(event_name::EOS, Duration::from_millis(1)));

        let pushed = recording.clone();
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(20));
            pushed.push(event(gst::EventType::Eos));
        });
        assert!(recording.wait_for_event(event_name::EOS, Duration::from_secs(5)));

        recording.clear();
        assert!(recording.is_empty());
        assert_eq!(recording.buffer_count(), 0);
    }

    #[test]
    fn recording_key_splits_into_scenario_and_handle() {
        assert_eq!(split_recording_key("scen1"), ("scen1", DEFAULT_HANDLE_NAME));
        assert_eq!(split_recording_key("scen1/audio"), ("scen1", "audio"));
        assert_eq!(
            split_recording_key("scen1/"),
            ("scen1/", DEFAULT_HANDLE_NAME)
        );
    }
}
