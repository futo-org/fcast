//! Mutation sweep over the sink sequence checker.
//!
//! A checker that never rejects is invisible until someone hands it garbage on
//! purpose, so this file does exactly that. Every entry in [`ILLEGAL`] is a
//! sequence a real sink must never observe, and every entry in [`LEGAL`] is one
//! it legitimately can. `check_all_named` has to reject the first set and
//! accept the second. Both directions matter. A checker that rejects everything
//! is as useless as one that rejects nothing.

use fcasttest::{
    scenario::check_all_named,
    sink::{RecordEntry, Recording, asserts},
};

fn ev(event_type: gst::EventType) -> RecordEntry {
    RecordEntry::synthetic_event(event_type)
}

fn buf(ms: u64) -> RecordEntry {
    RecordEntry::synthetic_buffer(gst::ClockTime::from_mseconds(ms))
}

fn preroll(ms: u64) -> RecordEntry {
    RecordEntry::synthetic_preroll(gst::ClockTime::from_mseconds(ms))
}

fn stream_start() -> RecordEntry {
    ev(gst::EventType::StreamStart)
}
fn caps() -> RecordEntry {
    ev(gst::EventType::Caps)
}
fn segment() -> RecordEntry {
    ev(gst::EventType::Segment)
}
fn flush_start() -> RecordEntry {
    ev(gst::EventType::FlushStart)
}
fn flush_stop() -> RecordEntry {
    ev(gst::EventType::FlushStop)
}
fn eos() -> RecordEntry {
    ev(gst::EventType::Eos)
}
fn gap() -> RecordEntry {
    ev(gst::EventType::Gap)
}

/// STREAM_START, CAPS, SEGMENT, one buffer, the shortest legal opening.
fn opened() -> Vec<RecordEntry> {
    vec![stream_start(), caps(), segment(), buf(0)]
}

fn recording(entries: Vec<RecordEntry>) -> Recording {
    let recording = Recording::new();
    for entry in entries {
        recording.push(entry);
    }
    recording
}

/// Sequences no correct pipeline can produce. Each is one mutation away from a
/// legal log, and the checker has to notice the mutation.
fn illegal() -> Vec<(&'static str, Vec<RecordEntry>)> {
    let mut cases: Vec<(&'static str, Vec<RecordEntry>)> = Vec::new();

    cases.push((
        "buffer before any caps",
        vec![stream_start(), segment(), buf(0)],
    ));
    cases.push((
        "caps before any stream-start",
        vec![caps(), segment(), buf(0)],
    ));
    cases.push((
        "buffer with no segment",
        vec![stream_start(), caps(), buf(0)],
    ));

    let mut case = opened();
    case.extend([flush_start(), flush_stop(), buf(0)]);
    cases.push(("buffer after a flush-stop with no fresh segment", case));

    let mut case = opened();
    case.extend([eos(), buf(40)]);
    cases.push(("buffer after eos", case));

    let mut case = opened();
    case.push(flush_start());
    cases.push(("flush-start never matched", case));

    let mut case = opened();
    case.extend([flush_start(), flush_start(), flush_stop()]);
    cases.push(("flush-start nested inside a flush", case));

    let mut case = opened();
    case.extend([
        flush_start(),
        buf(40),
        buf(80),
        buf(120),
        flush_stop(),
        segment(),
        buf(0),
    ]);
    cases.push(("buffers rendered while the pad is flushing", case));

    let mut case = opened();
    case.extend([
        flush_start(),
        preroll(40),
        preroll(80),
        flush_stop(),
        segment(),
        buf(0),
    ]);
    cases.push(("prerolls taken while the pad is flushing", case));

    let mut case = opened();
    case.extend([eos(), preroll(40)]);
    cases.push(("preroll after eos", case));

    let mut case = opened();
    case.extend([eos(), gap()]);
    cases.push(("gap after eos", case));

    let mut case = vec![stream_start(), caps(), gap(), segment(), buf(0)];
    cases.push(("gap before any segment", std::mem::take(&mut case)));

    let mut case = opened();
    case.extend([eos(), eos()]);
    cases.push(("eos repeated with no flush or stream-start between", case));

    // A sparse stream OPENS with a GAP, so a caps rule that only looks at buffers
    // never inspects the first thing a text branch ever delivers.
    cases.push((
        "gap before any caps",
        vec![stream_start(), segment(), gap(), caps(), buf(0)],
    ));

    // Sticky events travel a pad in sticky order, stream-start first.
    cases.push((
        "segment before any stream-start",
        vec![segment(), stream_start(), caps(), buf(0)],
    ));

    let mut case = opened();
    case.extend([eos(), caps()]);
    cases.push(("caps after eos with nothing to reopen the pad", case));

    let mut case = opened();
    case.extend([eos(), segment()]);
    cases.push(("segment after eos with nothing to reopen the pad", case));

    let mut case = opened();
    case.extend([
        flush_start(),
        caps(),
        segment(),
        flush_stop(),
        segment(),
        buf(0),
    ]);
    cases.push((
        "two serialized events delivered while the pad is flushing",
        case,
    ));

    // One buffer and one EOS inside a single flush window. Each is within its own
    // kind's in-flight allowance, so the data-only and event-only rules both pass
    // it, and the stream lock still makes it impossible.
    let mut case = opened();
    case.extend([
        flush_start(),
        buf(40),
        eos(),
        flush_stop(),
        segment(),
        buf(0),
    ]);
    cases.push(("a buffer and an eos sharing one flush window", case));

    cases.push((
        "buffer pts moves backwards inside one segment",
        vec![stream_start(), caps(), segment(), buf(100), buf(40)],
    ));

    let mut case = opened();
    case.extend([buf(80), preroll(40)]);
    cases.push(("preroll behind the buffer before it", case));

    cases
}

/// Sequences a correct pipeline really does produce. A checker that flags any
/// of these would make every test built on it flaky.
fn legal() -> Vec<(&'static str, Vec<RecordEntry>)> {
    let mut cases: Vec<(&'static str, Vec<RecordEntry>)> = Vec::new();

    cases.push(("nothing observed", Vec::new()));
    cases.push(("opening prefix", opened()));

    let mut case = opened();
    case.push(eos());
    cases.push(("played to eos", case));

    let mut case = opened();
    case.extend([flush_start(), flush_stop(), segment(), buf(0), eos()]);
    cases.push(("flushing seek back to zero", case));

    let mut case = opened();
    case.extend([eos(), flush_start(), flush_stop(), segment(), buf(0), eos()]);
    cases.push(("flushing seek after eos", case));

    let mut case = opened();
    case.extend([eos(), stream_start(), caps(), segment(), buf(0), eos()]);
    cases.push(("a second stream on the same sink", case));

    // A log that starts mid-flush. The recording was cleared, or the sink was
    // plugged in while a flush was already in progress.
    cases.push((
        "log begins with an unmatched flush-stop",
        vec![flush_stop(), stream_start(), caps(), segment(), buf(0)],
    ));

    // The preroll buffer reaches the sink twice: once from the preroll vfunc and
    // again from render, see RecordEntry::Preroll.
    cases.push((
        "preroll recorded before its render",
        vec![stream_start(), caps(), segment(), preroll(0), buf(0), eos()],
    ));

    let mut case = opened();
    case.extend([gap(), buf(40), eos()]);
    cases.push(("sparse stream with a gap between buffers", case));

    // The widened caps rule must still accept a branch that OPENS with a gap,
    // which is every text branch in the suite.
    cases.push((
        "sparse stream whose first delivery is a gap",
        vec![stream_start(), caps(), segment(), gap(), buf(40), eos()],
    ));

    // A fresh segment reopens the timeline. Every seek back to zero, every
    // gapless item boundary and every replayed external subtitle looks like this.
    cases.push((
        "pts restarts after a fresh segment",
        vec![
            stream_start(),
            caps(),
            segment(),
            buf(400),
            segment(),
            buf(0),
            eos(),
        ],
    ));

    cases.push((
        "pts restarts on the next stream of a gapless handoff",
        vec![
            stream_start(),
            caps(),
            segment(),
            buf(400),
            eos(),
            stream_start(),
            caps(),
            segment(),
            buf(0),
            eos(),
        ],
    ));

    // A buffer with no PTS neither violates the bound nor lowers it.
    cases.push((
        "an untimed buffer between two timed ones",
        vec![
            stream_start(),
            caps(),
            segment(),
            buf(40),
            RecordEntry::synthetic_buffer(None),
            buf(80),
            eos(),
        ],
    ));

    // The serialized-event twin of the in-flight render below. An event function
    // that had already passed the flushing check records itself before chaining
    // up, so exactly one can land after the flush-start.
    let mut case = opened();
    case.extend([
        flush_start(),
        caps(),
        flush_stop(),
        segment(),
        buf(0),
        eos(),
    ]);
    cases.push((
        "one in-flight serialized event recorded after the flush-start",
        case,
    ));

    // A lone EOS inside the window is the same race and stays legal on purpose.
    // Tightening this to zero would flake. An EOS past the flushing check and
    // preempted before the event function records itself produces exactly this.
    // Two entries is what proves a flush was ignored, see
    // asserts::nothing_serialized_during_flush.
    let mut case = opened();
    case.extend([flush_start(), eos(), flush_stop(), segment(), buf(0), eos()]);
    cases.push(("one in-flight eos recorded after the flush-start", case));

    // The one render that was already inside the stream lock when the FLUSH_START
    // was recorded, see asserts::no_data_during_flush.
    let mut case = opened();
    case.extend([
        flush_start(),
        buf(40),
        flush_stop(),
        segment(),
        buf(0),
        eos(),
    ]);
    cases.push(("one in-flight render recorded after the flush-start", case));

    cases
}

#[test]
fn every_illegal_sequence_is_rejected() {
    fcasttest::register_for_tests();

    let mut accepted = Vec::new();
    for (name, entries) in illegal() {
        let recording = recording(entries);
        if check_all_named(&[(name, &recording)]).is_ok() {
            accepted.push(name);
        }
    }
    assert!(
        accepted.is_empty(),
        "the sequence checker ACCEPTED {} illegal sequence(s), so every test that \
         relies on it is unsound:\n  {}",
        accepted.len(),
        accepted.join("\n  ")
    );
}

#[test]
fn every_legal_sequence_is_accepted() {
    fcasttest::register_for_tests();

    let mut rejected = Vec::new();
    for (name, entries) in legal() {
        let recording = recording(entries);
        if let Err(err) = check_all_named(&[(name, &recording)]) {
            rejected.push(format!("{name}: {err}"));
        }
    }
    assert!(
        rejected.is_empty(),
        "the sequence checker rejected {} legal sequence(s):\n{}",
        rejected.len(),
        rejected.join("\n")
    );
}

/// The sweep has to report EVERY offender, not stop at the first one, and it
/// has to name them. A violation nobody can attribute is a violation nobody
/// fixes.
#[test]
fn the_sweep_names_every_offender() {
    fcasttest::register_for_tests();

    let good = recording({
        let mut log = opened();
        log.push(eos());
        log
    });
    let no_segment = recording(vec![stream_start(), caps(), buf(0)]);
    let after_eos = recording({
        let mut log = opened();
        log.extend([eos(), buf(40)]);
        log
    });

    let err = check_all_named(&[
        ("video", &good),
        ("audio", &no_segment),
        ("text", &after_eos),
    ])
    .expect_err("two of the three recordings are illegal");

    assert!(err.starts_with("2 of 3 recordings"), "{err}");
    assert!(err.contains("audio:"), "{err}");
    assert!(err.contains("text:"), "{err}");
    assert!(!err.contains("video:"), "{err}");
}

/// A rule added later earns its place only by catching something the earlier
/// rules did not. For each sequence below, every checker that already existed
/// must ACCEPT it (so the sequence really was slipping through), and `all` must
/// reject it now.
#[test]
fn each_new_rule_catches_what_the_older_ones_missed() {
    fcasttest::register_for_tests();

    type Checker = fn(&[RecordEntry]) -> Result<(), String>;
    // The rule set as it stood before this round of tightening. caps_before_
    // first_buffer is absent because it is one of the rules that was widened.
    let older: [(&str, Checker); 6] = [
        ("flush_pairs_matched", asserts::flush_pairs_matched),
        (
            "stream_start_before_caps",
            asserts::stream_start_before_caps,
        ),
        (
            "segment_before_first_buffer",
            asserts::segment_before_first_buffer,
        ),
        ("no_buffer_after_eos", asserts::no_buffer_after_eos),
        ("eos_not_repeated", asserts::eos_not_repeated),
        ("no_data_during_flush", asserts::no_data_during_flush),
    ];

    let cases: Vec<(&str, Vec<RecordEntry>)> = vec![
        (
            "gap before any caps",
            vec![stream_start(), segment(), gap(), caps(), buf(0)],
        ),
        (
            "segment before any stream-start",
            vec![segment(), stream_start(), caps(), buf(0)],
        ),
        ("caps after eos", {
            let mut log = opened();
            log.extend([eos(), caps()]);
            log
        }),
        ("segment after eos", {
            let mut log = opened();
            log.extend([eos(), segment()]);
            log
        }),
        ("a buffer and an eos sharing one flush window", {
            let mut log = opened();
            log.extend([
                flush_start(),
                buf(40),
                eos(),
                flush_stop(),
                segment(),
                buf(0),
            ]);
            log
        }),
        ("two serialized events in one flush window", {
            let mut log = opened();
            log.extend([
                flush_start(),
                caps(),
                segment(),
                flush_stop(),
                segment(),
                buf(0),
            ]);
            log
        }),
        (
            "buffer pts moves backwards inside one segment",
            vec![stream_start(), caps(), segment(), buf(100), buf(40)],
        ),
        ("preroll behind the buffer before it", {
            let mut log = opened();
            log.extend([buf(80), preroll(40)]);
            log
        }),
    ];

    for (what, log) in cases {
        for (name, checker) in &older {
            assert!(
                checker(&log).is_ok(),
                "{what}: {name} already rejected this, so it is not evidence that \
                 anything new bites"
            );
        }
        let err = asserts::all(&log).expect_err(what);
        assert!(!err.is_empty(), "{what}: empty violation message");
    }
}

/// `first_buffer_is_discont` is not part of `all` (see its documentation), so
/// it gets its own mutation pair. `toml_scenarios.rs` is what runs it against
/// real media.
#[test]
fn the_discont_rule_rejects_a_continued_stream() {
    fcasttest::register_for_tests();

    let discont = |ms: u64| {
        RecordEntry::synthetic_buffer_with_flags(
            gst::ClockTime::from_mseconds(ms),
            gst::BufferFlags::DISCONT,
        )
    };

    // What every sink in the suite really records.
    asserts::first_buffer_is_discont(&[
        stream_start(),
        caps(),
        segment(),
        preroll(0),
        discont(0),
        buf(40),
        eos(),
    ])
    .expect("a stream that opens with a discont buffer");

    // The same log with the flag cleared on the one buffer that must carry it.
    let err = asserts::first_buffer_is_discont(&[
        stream_start(),
        caps(),
        segment(),
        preroll(0),
        buf(0),
        discont(40),
        eos(),
    ])
    .expect_err("a first buffer with no discont flag");
    assert!(err.contains("entry 4"), "{err}");

    // A log that never saw the stream open is exempt. It was cleared, or the sink
    // was attached mid-flow.
    asserts::first_buffer_is_discont(&[caps(), segment(), buf(0), eos()])
        .expect("a log that begins mid-stream");
    asserts::first_buffer_is_discont(&[]).expect("nothing observed");

    // And it stays out of `all`, so the synthetic logs everything else here is
    // built from are not silently required to carry the flag.
    asserts::all(&[stream_start(), caps(), segment(), buf(0), eos()])
        .expect("all() does not enforce the discont rule");
}

/// Each individual checker has to be the one that fires, otherwise `all` could
/// be passing by accident through a different rule.
#[test]
fn each_checker_owns_its_violation() {
    fcasttest::register_for_tests();

    let cases: Vec<(&str, Vec<RecordEntry>, &str)> = vec![
        (
            "flush_pairs_matched",
            {
                let mut log = opened();
                log.push(flush_start());
                log
            },
            "flush_pairs_matched",
        ),
        (
            "stream_start_before_caps",
            vec![caps()],
            "stream_start_before_caps",
        ),
        (
            "caps_before_first_buffer",
            vec![stream_start(), segment(), buf(0)],
            "caps_before_first_buffer",
        ),
        (
            "segment_before_first_buffer",
            vec![stream_start(), caps(), buf(0)],
            "segment_before_first_buffer",
        ),
        (
            "no_buffer_after_eos",
            {
                let mut log = opened();
                log.extend([eos(), buf(40)]);
                log
            },
            "no_buffer_after_eos",
        ),
        (
            "stream_start_before_segment",
            vec![segment(), stream_start(), caps(), buf(0)],
            "stream_start_before_segment",
        ),
        (
            "no_stream_event_after_eos",
            {
                let mut log = opened();
                log.extend([eos(), segment()]);
                log
            },
            "no_stream_event_after_eos",
        ),
        (
            "nothing_serialized_during_flush",
            {
                let mut log = opened();
                log.extend([
                    flush_start(),
                    buf(40),
                    eos(),
                    flush_stop(),
                    segment(),
                    buf(0),
                ]);
                log
            },
            "nothing_serialized_during_flush",
        ),
        (
            "monotonic_pts_within_segment",
            vec![stream_start(), caps(), segment(), buf(100), buf(40)],
            "monotonic_pts_within_segment",
        ),
        (
            "caps_before_first_buffer covers a leading gap",
            vec![stream_start(), segment(), gap(), caps(), buf(0)],
            "caps_before_first_buffer",
        ),
    ];

    for (what, log, expected) in cases {
        let err = asserts::all(&log).expect_err(what);
        assert!(
            err.contains(expected),
            "{what}: the violation was reported by the wrong checker: {err}"
        );
    }
}
