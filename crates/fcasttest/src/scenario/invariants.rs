//! Invariant sweep over every recording a scenario produced.
//!
//! [`crate::sink::asserts::all`] is only meaningful on a QUIESCENT log: a
//! snapshot taken mid-flushing-seek legally ends with an unmatched FLUSH_START.
//! Call the sweep at a settled point (after EOS, after a stop, after a
//! confirmed selection), or let [`wait_quiescent`] establish one first.

use std::time::{Duration, Instant};

use crate::sink::{Recording, asserts};

/// Entries appended to the error message per failing recording, so a violation
/// arrives with the sequence that produced it.
const TAIL_ENTRIES: usize = 12;

/// Runs every sequence invariant over each recording. Names are positional, see
/// [`check_all_named`] for readable ones.
///
/// An empty log breaks no sequence rule, and neither does an empty slice of
/// recordings, so this passing does NOT mean anything played. Assert that data
/// arrived (`buffer_count() > 0`, an EOS at the sink) separately.
pub fn check_all(recordings: &[Recording]) -> Result<(), String> {
    let named: Vec<(String, &Recording)> = recordings
        .iter()
        .enumerate()
        .map(|(index, recording)| (format!("recording[{index}]"), recording))
        .collect();
    check(
        named
            .iter()
            .map(|(name, recording)| (name.as_str(), *recording)),
    )
}

/// [`check_all`] with caller-supplied names. Every failure is reported, not just
/// the first: a wedge usually violates the same rule on several sinks and the
/// full picture is what identifies it.
pub fn check_all_named(recordings: &[(&str, &Recording)]) -> Result<(), String> {
    check(recordings.iter().copied())
}

fn check<'a>(recordings: impl Iterator<Item = (&'a str, &'a Recording)>) -> Result<(), String> {
    let mut failures = Vec::new();
    let mut total = 0usize;
    for (name, recording) in recordings {
        total += 1;
        let log = recording.snapshot();
        if let Err(violation) = asserts::all(&log) {
            let tail: Vec<String> = log
                .iter()
                .skip(log.len().saturating_sub(TAIL_ENTRIES))
                .map(|entry| entry.to_string())
                .collect();
            failures.push(format!(
                "  {name}: {violation}\n    ({} entries, last {}: {})",
                log.len(),
                tail.len(),
                tail.join(", ")
            ));
        }
    }
    if failures.is_empty() {
        return Ok(());
    }
    Err(format!(
        "{} of {total} recordings violated a sequence invariant:\n{}",
        failures.len(),
        failures.join("\n")
    ))
}

/// Blocks until no recording has grown for `settle`, so a following
/// [`check_all`] sees a quiescent log. Returns false when something was still
/// producing at `bound`.
///
/// This is a settle heuristic and nothing stronger. It cannot tell "the stream
/// ended" from "the next buffer is more than `settle` away", so `settle` has to
/// exceed the widest inter-arrival gap the media can produce, and nothing here
/// checks that it does. It is also satisfied by a log that never had anything in
/// it, and [`check_all`] passes an empty log: quiescence is a precondition for
/// the sweep, never evidence that anything played. Anchor on the event that says
/// the work is done (EOS, a confirmed selection, a completed stop) and use this
/// only to let the tail of that work land.
pub fn wait_quiescent(
    recordings: &[(&str, &Recording)],
    settle: Duration,
    bound: Duration,
) -> bool {
    let Some((_, first)) = recordings.first() else {
        return true;
    };
    let lengths = |recordings: &[(&str, &Recording)]| -> Vec<usize> {
        recordings
            .iter()
            .map(|(_, recording)| recording.len())
            .collect()
    };
    let deadline = Instant::now() + bound;
    loop {
        let before = lengths(recordings);
        // Never satisfied, so this returns exactly at the settle deadline unless
        // the log stays untouched (in which case it returns then too).
        first.wait_for(|_| false, settle);
        if lengths(recordings) == before {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sink::RecordEntry;

    fn event(event_type: gst::EventType) -> RecordEntry {
        RecordEntry::synthetic_event(event_type)
    }

    fn legal() -> Recording {
        let recording = Recording::new();
        recording.push(event(gst::EventType::StreamStart));
        recording.push(event(gst::EventType::Caps));
        recording.push(event(gst::EventType::Segment));
        recording.push(RecordEntry::synthetic_buffer(gst::ClockTime::ZERO));
        recording.push(event(gst::EventType::Eos));
        recording
    }

    #[test]
    fn a_quiescent_legal_sweep_passes() {
        gst::init().unwrap();
        let video = legal();
        let audio = legal();
        check_all(&[video.clone(), audio.clone()]).expect("legal logs");
        check_all_named(&[("video", &video), ("audio", &audio)]).expect("legal logs");
        check_all(&[]).expect("nothing to check");
    }

    #[test]
    fn every_failing_recording_is_named_with_its_tail() {
        gst::init().unwrap();
        let good = legal();
        let dangling = legal();
        dangling.push(event(gst::EventType::FlushStart));
        let after_eos = legal();
        after_eos.push(RecordEntry::synthetic_buffer(gst::ClockTime::ZERO));

        let err = check_all_named(&[("video", &good), ("audio", &dangling), ("text", &after_eos)])
            .expect_err("two violations");
        assert!(err.starts_with("2 of 3 recordings"), "{err}");
        assert!(err.contains("audio: flush_pairs_matched"), "{err}");
        assert!(err.contains("text: no_buffer_after_eos"), "{err}");
        assert!(!err.contains("video:"), "{err}");
        assert!(err.contains("event(eos)"), "the tail is included: {err}");
    }

    #[test]
    fn quiescence_waits_out_a_producer() {
        gst::init().unwrap();
        let recording = Recording::new();
        let writer = recording.clone();
        std::thread::spawn(move || {
            for _ in 0..5 {
                writer.push(RecordEntry::synthetic_buffer(gst::ClockTime::ZERO));
                std::thread::sleep(Duration::from_millis(5));
            }
        });
        assert!(wait_quiescent(
            &[("sink", &recording)],
            Duration::from_millis(40),
            Duration::from_secs(5),
        ));
        assert_eq!(recording.buffer_count(), 5);

        assert!(wait_quiescent(&[], Duration::ZERO, Duration::ZERO));
    }
}
