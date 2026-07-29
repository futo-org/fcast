//! Integration tests for the gapless prepared-next handoff
//! (`FcastPlaybin::prepare_next_async`): real decode pipelines over
//! generated media, driven through the public API exactly like the
//! receiver drives it.
//!
//! The core property under test: with a prepared next input linked into the
//! live core, the current item's drain must NOT produce a pipeline EOS.
//! decodebin3 switches to the prepared streams, the crate rolls the
//! generation and emits `PreparedActivated` followed by the new item's
//! collection and selection, and playback continues seamlessly. The final
//! item's end still produces a normal EOS.

use std::{
    sync::mpsc,
    time::{Duration, Instant},
};

use fcastplaybin::{AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, Sinks, StartPoint};
use gst::prelude::*;

/// Everything here plays media in real time (synced fake sinks), so waits
/// are bounded generously: a busy CI box must not flake, a wedge must not
/// hang the suite.
const EVENT_TIMEOUT: Duration = Duration::from_secs(20);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        // Debuggability under --nocapture: FCASTPLAYBIN_TEST_LOG=debug
        // shows the crate's swap tracing.
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        gst::init().unwrap();
    });
}

/// Whether the plugins the media generator needs are present. Absent on
/// exotic environments; the tests skip rather than fail there.
fn encoders_available() -> bool {
    ["vp8enc", "vorbisenc", "webmmux", "vp8dec", "vorbisdec"]
        .iter()
        .all(|f| gst::ElementFactory::find(f).is_some())
}

/// A conservative lower bound for "the clip actually PLAYED in real time".
/// Every generated clip is nominally 2s; decode without sync (a broken
/// transition free-wheeling through the second item) finishes one in well
/// under half of that, so this cleanly separates the two.
const CLIP_MIN: Duration = Duration::from_millis(1000);

/// Encode a 2s A/V webm clip (64x64 vp8 + vorbis tone) to a temp file and
/// return its file:// URI. `pattern`/`freq` make each clip distinct.
fn encode_av_clip(name: &str, pattern: &str, freq: u32) -> String {
    let path = std::env::temp_dir().join(format!(
        "fcastplaybin-gapless-{}-{}.webm",
        std::process::id(),
        name
    ));
    let desc = format!(
        "videotestsrc num-buffers=60 pattern={pattern} \
           ! video/x-raw,width=64,height=64,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         audiotestsrc num-buffers=87 freq={freq} \
           ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! vorbisenc ! mux. \
         webmmux name=mux ! filesink location={}",
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

/// Encode a 2s video-only webm clip.
fn encode_video_clip(name: &str, pattern: &str) -> String {
    let path = std::env::temp_dir().join(format!(
        "fcastplaybin-gapless-{}-{}.webm",
        std::process::id(),
        name
    ));
    let desc = format!(
        "videotestsrc num-buffers=60 pattern={pattern} \
           ! video/x-raw,width=64,height=64,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! webmmux ! filesink location={}",
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

fn run_to_eos(desc: &str) {
    let pipeline = gst::parse::launch(desc).expect("encode pipeline parses");
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(30),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("encode finishes");
    if let gst::MessageView::Error(err) = msg.view() {
        panic!("encode pipeline failed: {}", err.error());
    }
    pipeline.set_state(gst::State::Null).unwrap();
}

/// A playbin under test plus the ordered `(event, generation)` stream its
/// callback produced.
struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
}

impl Harness {
    fn new() -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: None, // internal synced fakesink
            audio: AudioSink::Factory(Box::new(|| {
                let sink = gst::ElementFactory::make("fakesink")
                    .property("sync", true)
                    .build()?;
                Ok(sink)
            })),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, generation| {
            let _ = tx.send((event, generation));
        });
        Self { playbin, events }
    }

    /// Wait until `pred` matches an event, returning everything seen up to
    /// and including it. Panics (with the seen log) on timeout.
    fn wait_for(
        &self,
        what: &str,
        mut pred: impl FnMut(&PlaybinEvent, u64) -> bool,
    ) -> Vec<(PlaybinEvent, u64)> {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut seen = Vec::new();
        loop {
            let remaining = deadline.saturating_duration_since(Instant::now());
            if remaining.is_zero() {
                panic!("timed out waiting for {what}; seen: {seen:#?}");
            }
            match self.events.recv_timeout(remaining) {
                Ok((event, generation)) => {
                    let hit = pred(&event, generation);
                    seen.push((event, generation));
                    if hit {
                        return seen;
                    }
                }
                Err(_) => panic!("timed out waiting for {what}; seen: {seen:#?}"),
            }
        }
    }

    /// Load `uri`, wait for the load to finish wiring, and start playback.
    fn load_and_play(&self, uri: &str) -> u64 {
        let generation = self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.wait_for("Loaded", |event, generation_seen| {
            matches!(event, PlaybinEvent::Loaded { .. }) && generation_seen == generation
        });
        self.playbin.play().expect("play");
        generation
    }
}

fn assert_no_eos(seen: &[(PlaybinEvent, u64)], context: &str) {
    assert!(
        !seen
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::EndOfStream)),
        "unexpected pipeline EOS {context}; seen: {seen:#?}"
    );
}

/// The headline property: prepare the next A/V item while the first plays,
/// and the transition produces PreparedActivated + the new item's held-back
/// collection (under the prepared generation) with NO pipeline EOS in
/// between. The second item's real end still produces EOS, stamped with the
/// new generation. Timing proves both items actually PLAYED (synced) rather
/// than free-wheeling through the switch: activation comes no earlier than
/// the first clip's tail and the final EOS no earlier than the second
/// clip's playtime after activation.
#[test]
fn gapless_switch_produces_no_eos_between_items() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_av_clip("switch-a", "smpte", 440);
    let second = encode_av_clip("switch-b", "ball", 880);

    let h = Harness::new();
    let first_generation = h.load_and_play(&first);
    let played_at = Instant::now();

    let prepared_generation = h
        .playbin
        .prepare_next_async(MediaInput::Uri(second.clone()));
    assert!(prepared_generation > first_generation);

    // The switch: activation must arrive, without any EOS before it, and
    // not before the first item played out.
    let seen = h.wait_for("PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    assert_no_eos(&seen, "before the gapless activation");
    let activated_at = Instant::now();
    assert!(
        activated_at.duration_since(played_at) >= CLIP_MIN,
        "activation after {:?} means the first item did not play in real time",
        activated_at.duration_since(played_at)
    );

    // The new item's collection follows the activation, stamped with the
    // new generation. (No StreamsSelected: a same-slot continuation posts
    // none, the selection carries over.)
    let seen = h.wait_for("the new item's StreamCollection", |event, generation| {
        matches!(event, PlaybinEvent::StreamCollection(_)) && generation == prepared_generation
    });
    assert_no_eos(&seen, "before the new item's collection");

    // The second item ends normally: a real EOS under the new generation,
    // no earlier than its playtime.
    let seen = h.wait_for("the final EndOfStream", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    let (_, eos_generation) = seen.last().unwrap();
    assert_eq!(
        *eos_generation, prepared_generation,
        "the final EOS belongs to the activated item"
    );
    assert!(
        activated_at.elapsed() >= CLIP_MIN,
        "EOS {:?} after activation means the second item did not play in real time",
        activated_at.elapsed()
    );

    h.playbin.stop().expect("stop");
}

/// Same switch with video-only items (no audio chain in play).
#[test]
fn gapless_switch_video_only_items() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_video_clip("vonly-a", "smpte");
    let second = encode_video_clip("vonly-b", "ball");

    let h = Harness::new();
    let _first_generation = h.load_and_play(&first);
    let prepared_generation = h.playbin.prepare_next_async(MediaInput::Uri(second));

    let seen = h.wait_for("PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    assert_no_eos(&seen, "before the gapless activation (video-only)");
    let activated_at = Instant::now();

    h.wait_for("the final EndOfStream", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    assert!(
        activated_at.elapsed() >= CLIP_MIN,
        "EOS {:?} after activation means the second item did not play in real time",
        activated_at.elapsed()
    );
    h.playbin.stop().expect("stop");
}

/// Cancelling a prepare restores the ordinary ending: the current item's
/// EOS arrives under the CURRENT generation and no activation ever fires.
#[test]
fn cancelled_prepare_falls_back_to_normal_eos() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_av_clip("cancel-a", "smpte", 440);
    let second = encode_av_clip("cancel-b", "ball", 880);

    let h = Harness::new();
    let first_generation = h.load_and_play(&first);
    let _prepared = h.playbin.prepare_next_async(MediaInput::Uri(second));
    h.playbin.cancel_prepared_async();

    let seen = h.wait_for("EndOfStream after cancel", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    let (_, eos_generation) = seen.last().unwrap();
    assert_eq!(
        *eos_generation, first_generation,
        "a cancelled prepare must leave the current item's ending untouched"
    );
    assert!(
        !seen
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::PreparedActivated)),
        "no activation may fire after a cancel; seen: {seen:#?}"
    );
    h.playbin.stop().expect("stop");
}

/// A prepared input that cannot even start (nonexistent file) reports
/// PreparedFailed with the prepared generation, and the current item still
/// ends with its own normal EOS.
#[test]
fn failing_prepared_input_reports_and_current_item_ends_normally() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_av_clip("fail-a", "smpte", 440);

    let h = Harness::new();
    let first_generation = h.load_and_play(&first);
    let prepared_generation = h.playbin.prepare_next_async(MediaInput::Uri(
        "file:///nonexistent/fcastplaybin-gapless.webm".into(),
    ));

    h.wait_for("PreparedFailed", |event, _| {
        matches!(
            event,
            PlaybinEvent::PreparedFailed { generation } if *generation == prepared_generation
        )
    });

    let seen = h.wait_for("EndOfStream after the failed prepare", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    let (_, eos_generation) = seen.last().unwrap();
    assert_eq!(*eos_generation, first_generation);
    assert!(
        !seen
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::PreparedActivated)),
        "a failed prepare must never activate; seen: {seen:#?}"
    );
    h.playbin.stop().expect("stop");
}

/// A dataless prepared input: an appsrc that is added to the pipeline but
/// never pushes a buffer, so the block probe never parks a thread and the
/// swap never performs. Lets a test arm a pending gapless hold that stays
/// pending for the whole item, driving the current item's end entirely
/// through the output-side hold.
fn dataless_prepared_input() -> MediaInput {
    MediaInput::Element(gst::ElementFactory::make("appsrc").build().unwrap())
}

/// A mid-item cancel must NOT end the current item early. A video clip
/// decodes in lockstep with playback (the synced sink backpressures), so at
/// cancel time its output-side EOS is still ~0.8s away and nothing has been
/// dropped. With a pending (dataless) prepare armed, cancelling mid-playout
/// must leave the item's real end untouched: exactly one natural EOS, at
/// the item's true end, once the cancel disarms the output-side hold.
///
/// The property this protects: a cancel (a user seek-back near the end of
/// an autoplay item, a queue edit) must not fabricate an early end that
/// would skip to the next item. Nothing may be synthesized while the item's
/// real end is still coming.
#[test]
fn mid_item_cancel_does_not_end_the_item_early() {
    init();
    if !encoders_available() || gst::ElementFactory::find("appsrc").is_none() {
        eprintln!("skipping: required elements unavailable");
        return;
    }
    let first = encode_video_clip("cancel-early-a", "smpte");

    let h = Harness::new();
    let first_generation = h.load_and_play(&first);
    let played_at = Instant::now();
    // Dataless prepare: the input side of the clip drains quickly, but the
    // swap never performs (no data), so the hold stays pending. Cancel
    // mid-playout, before the real (output-side) end.
    let _prepared = h.playbin.prepare_next_async(dataless_prepared_input());
    std::thread::sleep(Duration::from_millis(1200));
    h.playbin.cancel_prepared_async();

    let seen = h.wait_for("EndOfStream after the mid-item cancel", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    let (_, eos_generation) = seen.last().unwrap();
    assert_eq!(*eos_generation, first_generation);
    let elapsed = played_at.elapsed();
    assert!(
        elapsed >= Duration::from_millis(1700),
        "EOS after {elapsed:?} means the cancel ended the 2s item early instead of \
         letting it play out"
    );
    assert!(
        !seen
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::PreparedActivated)),
        "no activation may fire after a cancel; seen: {seen:#?}"
    );
    h.playbin.stop().expect("stop");
}

/// A cancel AFTER the current item's end was consumed by the hold must
/// still surface that end. With a pending (dataless) prepare armed, the
/// item's output-side EOS is dropped by the hold as it plays out; once the
/// item has fully finished, that EOS is gone for good (the sinks whose EOS
/// was dropped can never aggregate a real pipeline EOS on their own).
/// Cancelling then must synthesize the end, or the caller never learns the
/// item finished (a silent autoplay wedge).
///
/// A behavioral guard on the cancel contract, not a discriminator for the
/// exact synthesis predicate: for a tightly muxed A/V file a single demuxer
/// throttles both streams together, so "input drained" and "an EOS was
/// dropped" become true at nearly the same instant near the real end. The
/// guard still catches a cancel that drops the end entirely or fires a
/// spurious activation.
#[test]
fn cancel_after_consumed_end_synthesizes_end_of_stream() {
    init();
    if !encoders_available() || gst::ElementFactory::find("appsrc").is_none() {
        eprintln!("skipping: required elements unavailable");
        return;
    }
    let first = encode_av_clip("consumed-a", "smpte", 440);

    let h = Harness::new();
    let first_generation = h.load_and_play(&first);
    let _prepared = h.playbin.prepare_next_async(dataless_prepared_input());

    // Let the item play past its 2s end: its EOS reaches the outputs and
    // the pending hold consumes it.
    std::thread::sleep(Duration::from_secs(3));
    h.playbin.cancel_prepared_async();

    let seen = h.wait_for("the synthesized EndOfStream", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    let (_, eos_generation) = seen.last().unwrap();
    assert_eq!(
        *eos_generation, first_generation,
        "the synthesized end belongs to the item whose real end was consumed"
    );
    assert!(
        !seen
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::PreparedActivated)),
        "a dataless prepared input must never activate; seen: {seen:#?}"
    );
    h.playbin.stop().expect("stop");
}

/// A prepared item that lacks a stream type the current item is playing
/// (A/V current, video-only next) must NOT switch gaplessly: the abandoned
/// audio sink would block every later end-of-stream. The swap demotes to
/// PreparedFailed and the current item still ends through the ordinary
/// path.
#[test]
fn prepared_item_missing_a_live_stream_kind_demotes() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_av_clip("shape-a", "smpte", 440);
    let second = encode_video_clip("shape-b", "ball");

    let h = Harness::new();
    let first_generation = h.load_and_play(&first);
    let prepared_generation = h.playbin.prepare_next_async(MediaInput::Uri(second));

    h.wait_for("PreparedFailed for the shape mismatch", |event, _| {
        matches!(
            event,
            PlaybinEvent::PreparedFailed { generation } if *generation == prepared_generation
        )
    });

    let seen = h.wait_for("EndOfStream after the demoted prepare", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    let (_, eos_generation) = seen.last().unwrap();
    assert_eq!(*eos_generation, first_generation);
    assert!(
        !seen
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::PreparedActivated)),
        "a demoted prepare must never activate; seen: {seen:#?}"
    );
    h.playbin.stop().expect("stop");
}

/// A normal load while a prepare is pending supersedes it completely: the
/// loaded item plays under its own generation and no activation fires.
#[test]
fn load_supersedes_pending_prepare() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_av_clip("supersede-a", "smpte", 440);
    let second = encode_av_clip("supersede-b", "ball", 880);
    let third = encode_av_clip("supersede-c", "snow", 660);

    let h = Harness::new();
    let _ = h.load_and_play(&first);
    let _prepared = h.playbin.prepare_next_async(MediaInput::Uri(second));
    let loaded_generation = h.load_and_play(&third);

    let seen = h.wait_for("EndOfStream of the loaded item", |event, generation| {
        matches!(event, PlaybinEvent::EndOfStream) && generation == loaded_generation
    });
    assert!(
        !seen
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::PreparedActivated)),
        "a load must drop the pending prepare; seen: {seen:#?}"
    );
    h.playbin.stop().expect("stop");
}
