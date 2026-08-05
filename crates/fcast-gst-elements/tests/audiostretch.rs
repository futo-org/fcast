//! Element-level tests for `fcastaudiostretch`.
//!
//! The property that matters for A/V sync is the *frame ratio*: at scale `r` the element must
//! emit `in_frames / r` output frames, because downstream plays the result at rate 1.0 over a
//! segment whose duration was divided by `r`. If the ratio drifts, audio desyncs from video
//! progressively, which is exactly the class of bug that is invisible in a short listen.

use std::sync::Once;

const RATE: u32 = 48_000;

fn init() {
    static INIT: Once = Once::new();
    INIT.call_once(|| {
        gst::init().unwrap();
        fcast_gst_elements::fcastaudiostretch::plugin_init().unwrap();
    });
}

/// One second of a 200 Hz sine at 48 kHz mono — a clean periodic signal with a 240-frame
/// period, comfortably inside the 65-400 Hz search range.
fn sine(frames: usize) -> Vec<f32> {
    (0..frames)
        .map(|i| {
            let t = i as f32 / RATE as f32;
            (2.0 * std::f32::consts::PI * 200.0 * t).sin() * 0.5
        })
        .collect()
}

fn buffer_of(samples: &[f32]) -> gst::Buffer {
    let mut bytes = Vec::with_capacity(samples.len() * 4);
    for s in samples {
        bytes.extend_from_slice(&s.to_ne_bytes());
    }
    gst::Buffer::from_mut_slice(bytes)
}

/// Push `frames` frames of sine through the element at `scale`, return the output frame count
/// and the concatenated output samples.
fn run(scale: f64, frames: usize) -> (usize, Vec<f32>) {
    init();

    let mut h = gst_check::Harness::new("fcastaudiostretch");
    h.set_src_caps_str(&format!(
        "audio/x-raw,format=F32LE,rate={RATE},channels=1,layout=interleaved"
    ));

    // The rate travels on the segment, exactly as a rate seek delivers it.
    let mut seg = gst::FormattedSegment::<gst::ClockTime>::new();
    seg.set_rate(scale);
    assert!(h.push_event(gst::event::Segment::builder(&seg).build()));

    let data = sine(frames);
    // Feed in 4096-frame chunks, the way a decoder would.
    for chunk in data.chunks(4096) {
        assert_eq!(h.push(buffer_of(chunk)), Ok(gst::FlowSuccess::Ok));
    }
    h.push_event(gst::event::Eos::new());

    let mut out = Vec::new();
    while let Some(buf) = h.try_pull() {
        let map = buf.map_readable().unwrap();
        for c in map.as_slice().chunks_exact(4) {
            out.push(f32::from_ne_bytes([c[0], c[1], c[2], c[3]]));
        }
    }
    (out.len(), out)
}

/// The core contract: output frames ≈ input frames / scale.
///
/// The allowance is expressed as an absolute frame bound, not a percentage, because the one
/// legitimate deviation is a *fixed* quantity: at EOS the engine passes whatever it still has
/// buffered through un-stretched rather than truncating the stream, which is at most one search
/// window (`3 * rate / 65` ≈ 2214 frames, ~46 ms at 48 kHz). A percentage tolerance would scale
/// with stream length and so would silently accept genuine per-splice drift on a long file —
/// exactly the bug this test exists to catch, since drift desyncs audio from video progressively
/// rather than audibly.
#[test]
fn frame_ratio_matches_scale() {
    // 2 seconds of input, so a systematic per-splice error accumulates well past the tail bound.
    let frames = 2 * RATE as usize;
    let tail = 3 * (RATE as usize / 65);

    for scale in [1.5f64, 2.0, 3.0, 0.75, 0.5] {
        let (out, _) = run(scale, frames);
        let expected = frames as f64 / scale;
        assert!(
            out as f64 >= expected - tail as f64,
            "scale {scale}: got {out} frames, expected at least {:.0} — output is being lost",
            expected - tail as f64
        );
        assert!(
            out as f64 <= expected + tail as f64,
            "scale {scale}: got {out} frames, expected at most {:.0} (ideal {expected:.0} plus \
             the {tail}-frame EOS tail) — the splice schedule is drifting",
            expected + tail as f64
        );
    }
}

/// Rates within the bypass epsilon must pass audio through untouched, sample for sample.
#[test]
fn unity_rate_is_bit_exact_passthrough() {
    let frames = 8192;
    let (out_len, out) = run(1.0, frames);
    assert_eq!(out_len, frames, "unity rate must not change frame count");

    let input = sine(frames);
    for (i, (a, b)) in input.iter().zip(&out).enumerate() {
        assert!(
            (a - b).abs() < 1e-9,
            "sample {i} changed in passthrough: {a} vs {b}"
        );
    }
}

/// Output must stay finite and in range — a splice that reads past the read head or a NaN
/// leaking through the schedule would show up here.
#[test]
fn output_is_finite_and_bounded() {
    for scale in [1.5f64, 2.0, 0.5] {
        let (_, out) = run(scale, RATE as usize);
        assert!(
            out.iter().all(|s| s.is_finite() && s.abs() <= 1.0),
            "scale {scale}: produced a non-finite or clipping sample"
        );
    }
}

/// Regression test for the rate-change stall (see `rate-change-stuck.txt`).
///
/// A real rate change happens *mid-playback*, so the new segment starts wherever the seek landed
/// — not at zero — and the element flips out of passthrough, which forces `GstBaseTransform` to
/// renegotiate and re-enter `set_caps` **after** the SEGMENT has arrived. If `set_caps` rebuilds
/// the stored segment, the output timeline anchors against start=0 and every buffer is stamped
/// `pts / scale`, landing far before `segment.start`. `basesink` clips out-of-segment buffers
/// before rendering, so the sink never prerolls and the pipeline wedges in PAUSED forever.
///
/// Every other test in this file uses `start = 0`, where a wrong anchor and a right one produce
/// identical output — which is exactly why they all passed while playback was broken.
#[test]
fn rate_change_at_nonzero_position_keeps_output_inside_the_segment() {
    init();

    // 124 s in, matching the SABR capture (seek at 0:02:04.088 to rate 1.5).
    let start = gst::ClockTime::from_seconds(124);
    let half_second = RATE as usize / 2;

    let mut h = gst_check::Harness::new("fcastaudiostretch");
    h.set_src_caps_str(&format!(
        "audio/x-raw,format=F32LE,rate={RATE},channels=1,layout=interleaved"
    ));

    let segment_at = |rate: f64| {
        let mut seg = gst::FormattedSegment::<gst::ClockTime>::new();
        seg.set_rate(rate);
        seg.set_start(start);
        seg.set_time(start);
        seg.set_position(start);
        gst::event::Segment::builder(&seg).build()
    };

    let stamped = |samples: &[f32], offset_frames: usize| {
        let mut buf = buffer_of(samples);
        {
            let b = buf.get_mut().unwrap();
            b.set_pts(
                start
                    + gst::ClockTime::from_nseconds(
                        offset_frames as u64 * 1_000_000_000 / RATE as u64,
                    ),
            );
        }
        buf
    };

    // Phase 1: ordinary playback at 1.0. The element sits in passthrough.
    assert!(h.push_event(segment_at(1.0)));
    let data = sine(half_second);
    for (i, chunk) in data.chunks(4096).enumerate() {
        assert_eq!(h.push(stamped(chunk, i * 4096)), Ok(gst::FlowSuccess::Ok));
    }
    while h.try_pull().is_some() {}

    // Phase 2: the rate change, as a flushing seek back to the same position — which is what
    // `send_rate_seek` issues. This is the transition that flips passthrough off.
    assert!(h.push_event(gst::event::FlushStart::new()));
    assert!(h.push_event(gst::event::FlushStop::builder(true).build()));
    assert!(h.push_event(segment_at(1.5)));

    for (i, chunk) in data.chunks(4096).enumerate() {
        assert_eq!(h.push(stamped(chunk, i * 4096)), Ok(gst::FlowSuccess::Ok));
    }
    h.push_event(gst::event::Eos::new());

    let mut buffers = 0;
    while let Some(buf) = h.try_pull() {
        let pts = buf.pts().expect("output buffer must carry a pts");
        assert!(
            pts >= start,
            "output stamped {pts} — before segment.start {start}. basesink would clip this and \
             never preroll, wedging the pipeline (this is the rate-change stall)"
        );
        // The half second of input compressed by 1.5 cannot reach beyond ~0.5 s past the start.
        assert!(
            pts < start + gst::ClockTime::from_seconds(1),
            "output stamped {pts}, implausibly far past segment.start {start}"
        );
        buffers += 1;
    }
    assert!(buffers > 0, "no output at all after the rate change");
}

/// At 1.0 the element must be in genuine `GstBaseTransform` passthrough — buffers forwarded by
/// reference with no copy, no engine round-trip and no re-timestamping. Sample-exactness alone
/// does not prove this: the engine's own bypass branch also produces identical samples, but via
/// a full copy. Since normal playback sits at 1.0 essentially all the time, the difference is
/// the element's entire steady-state cost.
#[test]
fn unity_rate_engages_basetransform_passthrough() {
    use gst_base::prelude::*;

    init();

    let mut h = gst_check::Harness::new("fcastaudiostretch");
    h.set_src_caps_str(&format!(
        "audio/x-raw,format=F32LE,rate={RATE},channels=1,layout=interleaved"
    ));

    let element = h.element().expect("harness element");
    let bt = element
        .downcast_ref::<gst_base::BaseTransform>()
        .expect("element is a BaseTransform");

    let mut seg = gst::FormattedSegment::<gst::ClockTime>::new();
    seg.set_rate(1.0);
    assert!(h.push_event(gst::event::Segment::builder(&seg).build()));
    assert!(
        bt.is_passthrough(),
        "rate 1.0 must engage BaseTransform passthrough, not just bypass inside the engine"
    );

    // ...and a stretching rate must leave it.
    let mut seg = gst::FormattedSegment::<gst::ClockTime>::new();
    seg.set_rate(1.5);
    assert!(h.push_event(gst::event::Segment::builder(&seg).build()));
    assert!(
        !bt.is_passthrough(),
        "rate 1.5 must disable passthrough or no stretching happens at all"
    );
}

/// A SEGMENT arriving without a preceding flush is what a gapless track transition looks like.
/// The engine holds up to one search window of audio at that moment, and it must be flushed out
/// rather than discarded — otherwise every gapless boundary silently eats ~46 ms of the outgoing
/// track, which is a dropout, not a glitch you would notice on a short listen.
#[test]
fn gapless_segment_boundary_does_not_drop_audio() {
    init();

    let scale = 1.5f64;
    let per_segment = RATE as usize; // 1 s each side of the boundary
    let tail = 3 * (RATE as usize / 65);

    let mut h = gst_check::Harness::new("fcastaudiostretch");
    h.set_src_caps_str(&format!(
        "audio/x-raw,format=F32LE,rate={RATE},channels=1,layout=interleaved"
    ));

    let mut seg = gst::FormattedSegment::<gst::ClockTime>::new();
    seg.set_rate(scale);

    let data = sine(per_segment);
    for _ in 0..2 {
        // Second iteration pushes a fresh segment with NO flush in between.
        assert!(h.push_event(gst::event::Segment::builder(&seg).build()));
        for chunk in data.chunks(4096) {
            assert_eq!(h.push(buffer_of(chunk)), Ok(gst::FlowSuccess::Ok));
        }
    }
    h.push_event(gst::event::Eos::new());

    let mut out = 0usize;
    while let Some(buf) = h.try_pull() {
        out += buf.size() / 4;
    }

    let expected = (2 * per_segment) as f64 / scale;
    // The bound is one-sided and tight on the low side *on purpose*. Draining at the boundary
    // only ever passes buffered audio through un-stretched, which ADDS frames; no input frame
    // is ever discarded. So output must not fall below the ideal (bar a few frames of integer
    // rounding in the splice schedule). A slack of one search window here would be wide enough
    // to swallow the very dropout this test exists to detect.
    assert!(
        out as f64 >= expected - 64.0,
        "lost audio across the gapless boundary: got {out} frames, expected at least {:.0} \
         (ideal {expected:.0})",
        expected - 64.0
    );
    assert!(
        out as f64 <= expected + 2.0 * tail as f64,
        "too many frames across the boundary: got {out}, expected at most {:.0}",
        expected + 2.0 * tail as f64
    );
}

/// The stretched signal must still be a 200 Hz sine — that is the whole point, pitch is
/// preserved while duration changes. Measured by zero-crossing count, which for a pure tone is
/// `2 * f * duration` and is unaffected by the time scaling.
#[test]
fn pitch_is_preserved() {
    let frames = RATE as usize;
    for scale in [1.5f64, 2.0] {
        let (out_len, out) = run(scale, frames);
        let duration_s = out_len as f64 / RATE as f64;

        let crossings = out
            .windows(2)
            .filter(|w| (w[0] <= 0.0) != (w[1] <= 0.0))
            .count() as f64;
        let measured_hz = crossings / (2.0 * duration_s);

        // A pitch-shifting implementation (plain resampling) would report 200*scale here.
        assert!(
            (measured_hz - 200.0).abs() < 12.0,
            "scale {scale}: pitch moved to {measured_hz:.1} Hz, expected ~200 Hz"
        );
    }
}

/// Throughput guard. The element must process audio far faster than realtime; the assertion is
/// deliberately loose (a 10x margin) so it flags a real regression — a heap allocation or an
/// accidental O(n²) on the hot path — without failing on a loaded CI machine. The printed
/// figures are the useful output; run with `--nocapture`.
#[test]
fn throughput_is_far_faster_than_realtime() {
    use std::time::Instant;

    init();

    let seconds = 30usize;
    let frames = seconds * RATE as usize;
    let data = sine(frames);

    for (label, scale) in [("stretching 1.5x", 1.5f64), ("passthrough 1.0x", 1.0)] {
        let mut h = gst_check::Harness::new("fcastaudiostretch");
        h.set_src_caps_str(&format!(
            "audio/x-raw,format=F32LE,rate={RATE},channels=2,layout=interleaved"
        ));
        let mut seg = gst::FormattedSegment::<gst::ClockTime>::new();
        seg.set_rate(scale);
        assert!(h.push_event(gst::event::Segment::builder(&seg).build()));

        // Stereo: duplicate each sample so the frame count still equals `frames`.
        let stereo: Vec<f32> = data.iter().flat_map(|s| [*s, *s]).collect();

        let t0 = Instant::now();
        for chunk in stereo.chunks(4096 * 2) {
            assert_eq!(h.push(buffer_of(chunk)), Ok(gst::FlowSuccess::Ok));
            while h.try_pull().is_some() {}
        }
        h.push_event(gst::event::Eos::new());
        while h.try_pull().is_some() {}
        let elapsed = t0.elapsed();

        let factor = seconds as f64 / elapsed.as_secs_f64();
        let cpu_pct = 100.0 / factor;
        println!(
            "  {label:18} {seconds}s stereo in {:7.1}ms  =>  {factor:8.0}x realtime \
             ({cpu_pct:.4}% of one core)",
            elapsed.as_secs_f64() * 1000.0
        );
        assert!(
            factor > 10.0,
            "{label}: only {factor:.1}x realtime — something regressed onto the hot path"
        );
    }
}
