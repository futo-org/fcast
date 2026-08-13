//! The transient-NotLinked window, reproduced without parsebin.
//!
//! Downstream may briefly unlink the src pad while it plugs elements, and
//! sticky caps let the first buffer land in that window. Here the window is
//! made explicit. The src pad is left peerless for [`UNLINKED_WINDOW`] and
//! then linked. Nothing may be lost and nothing may be posted. A pad that is
//! never linked still has to fail, which is the other half of what ftestsrc's
//! bound buys.

use std::{
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};

use fcasttest::{
    caps,
    sink::{FTestSink, asserts, event_name},
    spec::{MediaSpec, StreamSpec},
};
use gst::prelude::*;

/// One 20 ms audio packet per buffer, so this is ten buffers.
const DURATION: gst::ClockTime = gst::ClockTime::from_mseconds(200);
const EXPECTED_BUFFERS: usize = 10;
/// Wider than the real plug window by an order of magnitude, and well
/// inside ftestsrc's retry bound.
const UNLINKED_WINDOW: Duration = Duration::from_millis(50);
/// Bound for anything the pipeline has to reach.
const TIMEOUT: Duration = Duration::from_secs(15);

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();

    INIT.call_once(fcasttest::register_for_tests);
}

/// A single-audio-stream scenario and an ftestsrc serving it. The src pad is
/// captured on pad-added and deliberately NOT linked.
fn source(key: &str) -> (gst::Pipeline, gst::Element, Arc<Mutex<Option<gst::Pad>>>) {
    fcasttest::registry::register_scenario(
        key,
        MediaSpec::new(1).with_stream(StreamSpec::audio("audio_0").with_duration(DURATION)),
    );

    let pipeline = gst::Pipeline::new();
    let src = gst::ElementFactory::make("ftestsrc")
        .property("uri", caps::uri_for_key(key))
        .build()
        .expect("ftestsrc");
    pipeline.add(&src).expect("adding ftestsrc");

    let captured: Arc<Mutex<Option<gst::Pad>>> = Arc::new(Mutex::new(None));
    let slot = captured.clone();
    src.connect_pad_added(move |_, pad| {
        *slot.lock().expect("pad slot") = Some(pad.clone());
    });
    (pipeline, src, captured)
}

/// Blocks until pad-added ran. It fires from inside the state change, so this
/// is satisfied by the time the transition returns.
fn captured_pad(captured: &Arc<Mutex<Option<gst::Pad>>>) -> gst::Pad {
    let deadline = Instant::now() + TIMEOUT;
    loop {
        if let Some(pad) = captured.lock().expect("pad slot").clone() {
            return pad;
        }
        assert!(Instant::now() < deadline, "ftestsrc never added its pad");
        std::thread::sleep(Duration::from_millis(2));
    }
}

/// First ERROR or EOS on the bus, whichever lands first.
enum Outcome {
    Eos,
    Error(String),
}

fn wait_for_outcome(pipeline: &gst::Pipeline, bound: Duration) -> Outcome {
    let bus = pipeline.bus().expect("the pipeline has a bus");
    let deadline = Instant::now() + bound;
    loop {
        let left = deadline.saturating_duration_since(Instant::now());
        assert!(!left.is_zero(), "neither EOS nor an error reached the bus");
        let Some(message) = bus.timed_pop(gst::ClockTime::from_nseconds(
            u64::try_from(left.as_nanos()).unwrap_or(u64::MAX),
        )) else {
            continue;
        };
        match message.view() {
            gst::MessageView::Eos(_) => return Outcome::Eos,
            gst::MessageView::Error(err) => {
                return Outcome::Error(format!("{}: {:?}", err.error(), err.debug()));
            }
            _ => (),
        }
    }
}

/// A pad with no peer for the whole window must cost nothing. Every buffer
/// still arrives, in order, and the source posts nothing.
#[test]
fn a_late_link_loses_no_buffer() {
    init();
    const KEY: &str = "notlinkedlate";

    let (pipeline, _src, captured) = source(KEY);
    let sink = FTestSink::new();
    sink.set_property("sync", false);
    let recording = sink.recording();
    pipeline.add(&sink).expect("adding ftestsink");

    // The pad is created and its task started by this transition, so pushes begin
    // against a peerless pad.
    pipeline
        .set_state(gst::State::Playing)
        .expect("the pipeline reaches PLAYING");
    let pad = captured_pad(&captured);
    assert!(
        !pad.is_linked(),
        "the pad was linked before the test linked it"
    );

    std::thread::sleep(UNLINKED_WINDOW);
    // State changes are recorded too, only data proves something crossed the pad.
    assert!(
        !recording.snapshot().iter().any(|entry| entry.is_data()),
        "the sink saw data while its pad had no peer"
    );
    pad.link(&sink.static_pad("sink").expect("ftestsink sink pad"))
        .expect("linking the source into the sink");

    match wait_for_outcome(&pipeline, TIMEOUT) {
        Outcome::Eos => (),
        Outcome::Error(err) => panic!("the late link was reported as a flow error: {err}"),
    }

    let log = recording.snapshot();
    asserts::all(&log).expect("the recorded sequence");
    assert_eq!(
        recording.buffer_count(),
        EXPECTED_BUFFERS,
        "the retried buffer was dropped or duplicated: {log:?}"
    );
    assert_eq!(
        recording.event_count(event_name::EOS),
        1,
        "exactly one EOS: {log:?}"
    );
    let first_pts = log
        .iter()
        .find(|entry| entry.is_buffer())
        .and_then(|entry| entry.pts());
    assert_eq!(
        first_pts,
        Some(gst::ClockTime::ZERO),
        "buffer 0 never made it past the unlinked window"
    );

    pipeline.set_state(gst::State::Null).expect("teardown");
    fcasttest::registry::unregister(KEY);
}

/// The bound is a bound, not a licence. A pad nothing ever links to still posts
/// the stream error every demuxer posts for a fatal flow return, and it does so
/// without spinning forever.
#[test]
fn a_pad_that_is_never_linked_still_errors() {
    init();
    const KEY: &str = "notlinkednever";

    let (pipeline, _src, captured) = source(KEY);
    pipeline
        .set_state(gst::State::Paused)
        .expect("the pipeline reaches PAUSED");
    let pad = captured_pad(&captured);
    assert!(!pad.is_linked(), "nothing may link this pad");

    let started = Instant::now();
    match wait_for_outcome(&pipeline, TIMEOUT) {
        Outcome::Error(err) => assert!(
            // basesrc's exact phrasing. Consumers key their recover-in-place
            // classification on this debug text.
            err.contains("reason not-linked"),
            "the error names something other than the not-linked reason: {err}"
        ),
        Outcome::Eos => panic!("an unlinked stream reached EOS"),
    }
    // Non-vacuous: without the retry the error would be posted immediately.
    assert!(
        started.elapsed() >= Duration::from_secs(1),
        "the push was not retried, it failed after {:?}",
        started.elapsed()
    );

    pipeline.set_state(gst::State::Null).expect("teardown");
    fcasttest::registry::unregister(KEY);
}
