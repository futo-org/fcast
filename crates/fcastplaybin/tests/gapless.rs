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
        // The receiver's part of the pipeline: fcastaudiostretch is built by
        // the fcastplaybin constructor but registered by the application.
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
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

/// Encode an audio-only webm clip (vorbis tone), `num_buffers` × 1024
/// samples at 44.1kHz: `encode_av_clip` minus the video branch. A single
/// elementary stream per item is the field report's topology (one decodebin3
/// request pad, same codec back to back).
fn encode_audio_clip_buffers(name: &str, freq: u32, num_buffers: u32) -> String {
    let path = std::env::temp_dir().join(format!(
        "fcastplaybin-gapless-{}-{}.webm",
        std::process::id(),
        name
    ));
    let desc = format!(
        "audiotestsrc num-buffers={num_buffers} freq={freq} \
           ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! vorbisenc \
           ! webmmux ! filesink location={}",
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

/// A 2s audio-only clip (87 × 1024 samples @ 44.1kHz ≈ 2.02s), matching
/// `encode_av_clip`'s audio branch exactly.
fn encode_audio_clip(name: &str, freq: u32) -> String {
    encode_audio_clip_buffers(name, freq, 87)
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

    /// Everything the callback has produced and nobody consumed yet, taken
    /// without blocking. Lets a test assert on what did NOT arrive by a
    /// given instant, which [`Harness::wait_for`] cannot: `wait_for` walks
    /// the whole backlog, so an event delivered seconds earlier still
    /// satisfies a wait issued now.
    fn drain_pending(&self) -> Vec<(PlaybinEvent, u64)> {
        let mut seen = Vec::new();
        while let Ok(entry) = self.events.try_recv() {
            seen.push(entry);
        }
        seen
    }

    /// Pump for `settle` and return everything that arrived, so a test can
    /// assert an event did not repeat.
    fn drain_after(&self, settle: Duration) -> Vec<(PlaybinEvent, u64)> {
        std::thread::sleep(settle);
        self.drain_pending()
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

/// A prepared input built the way the receiver builds one: `urisourcebin`
/// with parsed streams out, identical to the crate's own
/// `make_urisourcebin`. A prepared input must be urisourcebin-rooted (an
/// appsrc-rooted chain dies not-negotiated against the blocked unlinked
/// pads). Handing the element in instead of a URI lets a test keep the
/// handle.
fn uri_source(uri: &str) -> gst::Element {
    gst::ElementFactory::make("urisourcebin")
        .property("uri", uri)
        .property("parse-streams", true)
        .property("use-buffering", true)
        .build()
        .expect("building a urisourcebin prepared input")
}

/// Block until the gapless swap has PERFORMED for `prepared`.
///
/// The observable is the crate's own wiring: a prepared input's source pads
/// sit blocked and unlinked until `perform_gapless_swap` links them into
/// decodebin3, under the same swap-gate lock that guards the cancel refusal.
/// So a cancel issued after the first linked source pad is guaranteed to
/// find the swap performed (worst case it waits on that lock). A sleep
/// cannot do this: the refusal window (`swapped && pending`) measured only
/// ~280ms, `pending` clearing when the new item's streams reach decodebin3's
/// output.
fn wait_for_performed_swap(prepared: &gst::Element) {
    let deadline = Instant::now() + EVENT_TIMEOUT;
    loop {
        if prepared.src_pads().iter().any(|pad| pad.is_linked()) {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "the prepared input was never swapped in (no linked source pad)"
        );
        std::thread::sleep(Duration::from_millis(2));
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

/// Post-swap `position()` readings must belong to the NEW item's own
/// 0-based timeline. Every generated clip is ~2s, so a healthy reading in
/// the first second of the new item is ~0.5-1.0s, while a cumulative
/// (un-rebased) reading adds the outgoing item's extent (≥ ~2.5s). The bound
/// sits between the two.
///
/// A cumulative timeline is a real field failure mode: streamsynchronizer
/// bumps `segment.base` once per item, gated on the group-id changing at its
/// sink pads. Skip the bump and nothing errors and nothing wedges:
/// `position()` silently reports accumulated running time and the sink, late
/// on every buffer, races the item to EOS. Nothing else in the suite would
/// catch that, hence these assertions.
///
/// The way the bump gets skipped: decodebin3 rewrites a new STREAM_START's
/// group-id while any sibling `sink_%u` input still holds the old id, and
/// only an unlink invalidates an input (gstdecodebin3.c:1776-1778), which is
/// why `perform_gapless_swap` unlinks all reused pads before relinking any.
/// Interleaving those loops on today's code does NOT break the timeline
/// (verified): the prepared pads stay block-probed during the swap, so no
/// event crosses until every unlink has run. The rewrite needs both guards
/// gone, which is exactly what a refactor could quietly do, and why this
/// assertion stays.
fn assert_rebased_position(samples: &[Option<gst::ClockTime>], context: &str) {
    let limit = gst::ClockTime::from_mseconds(1900);
    assert!(
        samples.iter().any(|sample| sample.is_some()),
        "no position reading at all after the gapless activation ({context}); \
         samples: {samples:?}"
    );
    for sample in samples.iter().flatten() {
        assert!(
            *sample < limit,
            "post-swap position {sample} exceeds {limit} ({context}): the new item is ~2s \
             long, so this is the CUMULATIVE running time: the per-item segment.base \
             bump was skipped. Samples: {samples:?}"
        );
    }
    // An upper bound alone cannot fail on a timeline that is not moving:
    // a `position()` frozen at zero satisfies "< 1900 ms" forever. The
    // readings are ~400 ms apart and the first is taken ~500 ms after the
    // activation, so the new item's clock must be both PAST its start and
    // ADVANCING between them.
    let readings: Vec<gst::ClockTime> = samples.iter().flatten().copied().collect();
    let floor = gst::ClockTime::from_mseconds(150);
    let last = *readings
        .last()
        .expect("at least one reading, asserted above");
    assert!(
        last >= floor,
        "post-swap position {last} never got past {floor} ({context}): the new item's \
         timeline is not running. Samples: {samples:?}"
    );
    if readings.len() > 1 {
        let first = readings[0];
        assert!(
            last > first,
            "post-swap position did not advance between the samples ({context}): the \
             new item's timeline is frozen. Samples: {samples:?}"
        );
    }
}

/// The headline property: prepare the next A/V item while the first plays,
/// and the transition produces PreparedActivated + the new item's held-back
/// collection (under the prepared generation) with NO pipeline EOS in
/// between. The second item's real end still produces EOS, stamped with the
/// new generation. Timing proves both items actually PLAYED (synced) rather
/// than free-wheeling through the switch: activation comes no earlier than
/// the first clip's tail and the final EOS no earlier than the second
/// clip's playtime after activation. Finally, the new item's timeline must
/// be rebased: `position()` reads the new item's own 0-based time, not the
/// running time accumulated across both items (`assert_rebased_position`).
/// This is the A/V, two-request-pad topology where the group-id rewrite
/// described on `assert_rebased_position` lives.
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

    // The switched-to item plays on ITS OWN clock: sample the position
    // twice inside its first second. Both readings must be item-local. A
    // missing per-item base bump would report ~2s (the first clip's extent)
    // plus the elapsed time here instead, silently.
    std::thread::sleep(Duration::from_millis(500));
    let early = h.playbin.position();
    std::thread::sleep(Duration::from_millis(400));
    let late = h.playbin.position();
    assert_rebased_position(&[early, late], "A/V gapless switch");

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

/// The same switch in the field report's shape: audio-only items, one
/// elementary stream each (a single decodebin3 request pad), same codec back
/// to back, i.e. music autoplay. Structurally this cannot hit the group-id
/// rewrite (no sibling input pins decodebin3's `current_group_id`), which is
/// why it belongs next to the A/V case: it isolates the audio timeline
/// itself (the sink `position()` anchors on, the pipeline clock, the
/// held-activation gate) from the multi-stream group-id machinery.
///
/// Asserts the whole contract for that topology: no pipeline EOS before the
/// activation, both items really PLAY in real time, and the new item's
/// position is rebased onto its own 0-based timeline.
#[test]
fn gapless_switch_audio_only_rebases_position() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    let first = encode_audio_clip("aonly-a", 440);
    let second = encode_audio_clip("aonly-b", 880);

    let h = Harness::new();
    let first_generation = h.load_and_play(&first);
    let played_at = Instant::now();

    let prepared_generation = h.playbin.prepare_next_async(MediaInput::Uri(second));
    assert!(prepared_generation > first_generation);

    let seen = h.wait_for("PreparedActivated", |event, generation| {
        matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
    });
    assert_no_eos(&seen, "before the gapless activation (audio-only)");
    let activated_at = Instant::now();
    assert!(
        activated_at.duration_since(played_at) >= CLIP_MIN,
        "activation after {:?} means the first item did not play in real time",
        activated_at.duration_since(played_at)
    );

    std::thread::sleep(Duration::from_millis(500));
    let early = h.playbin.position();
    std::thread::sleep(Duration::from_millis(400));
    let late = h.playbin.position();
    assert_rebased_position(&[early, late], "audio-only gapless switch");

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

/// Cancelling a prepare restores the ordinary ending: the cancel is
/// CONFIRMED with `PreparedCancelled` naming the dropped prepare (the event
/// the caller drops its pre-arm bookkeeping on), the current item's EOS
/// arrives under the CURRENT generation, and no activation ever fires. The
/// cancel is queued right behind the prepare on the same worker queue, long
/// before the swap can perform, so it always wins this race (the losing
/// side is `cancel_after_performed_swap_is_declined_...`).
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
    let prepared_generation = h.playbin.prepare_next_async(MediaInput::Uri(second));
    h.playbin.cancel_prepared_async();

    let seen = h.wait_for("PreparedCancelled confirming the cancel", |event, _| {
        matches!(event, PlaybinEvent::PreparedCancelled { .. })
    });
    assert!(
        matches!(
            seen.last().unwrap().0,
            PlaybinEvent::PreparedCancelled { generation } if generation == Some(prepared_generation)
        ),
        "the confirmation must name the prepare it dropped; seen: {seen:#?}"
    );
    assert!(
        !seen
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::PreparedActivated)),
        "a confirmed cancel must not have activated anything; seen: {seen:#?}"
    );

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

/// The other side of the cancel race: a cancel that arrives AFTER the swap
/// performed is REFUSED, and the refusal is reported as
/// `PreparedCancelDeclined` so the caller knows the activation is still
/// coming.
///
/// This is the normal case for a small or cached item: the swap performs as
/// soon as the outgoing item's INPUT side drains, well before its audio
/// finishes playing out of the decoupling queue. Past that point the
/// prepared input is the only linked upstream, so cancelling would rip the
/// live input out mid-stream. The crate must instead let the activation
/// land, and must NOT synthesize an end-of-stream (that synthesis exists for
/// cancels that WIN and swallowed the item's real end). The caller's side of
/// the contract: on a declined cancel it KEEPS its pre-arm bookkeeping so
/// the imminent `PreparedActivated` is adopted (an unmatched activation
/// replays the finished item from 0). Nothing else in the suite reaches the
/// refusal branch (`state.swapped && pending`).
///
/// Timing: the refusal branch is only live between the swap and the
/// pipeline-internal activation, measured ~280ms on a 4s audio-only item
/// (input drains at ~2.7s, `pending` clears at ~3.0s, the caller-facing
/// `PreparedActivated` follows ~1s later at the decoupling-queue crossing).
/// A sleep cannot aim at that reliably, so the cancel triggers off the swap
/// itself (`wait_for_performed_swap`).
#[test]
fn cancel_after_performed_swap_is_declined_and_activation_completes() {
    init();
    if !encoders_available() {
        eprintln!("skipping: vp8/vorbis/webm elements unavailable");
        return;
    }
    // ~4s (173 × 1024 @ 44.1kHz) and ~2s: A long enough that its input
    // drains well before its playout ends, which is what puts the cancel
    // after the swap and still mid-item.
    let first = encode_audio_clip_buffers("declined-a", 440, 173);
    let second = encode_audio_clip("declined-b", 880);

    let h = Harness::new();
    let _first_generation = h.load_and_play(&first);
    // Prepared as an element so the test holds the handle it needs to see
    // the swap. `urisourcebin`-rooted, like the receiver's own gapless
    // source and the crate's internal URI path.
    let prepared_element = uri_source(&second);
    let prepared_generation = h
        .playbin
        .prepare_next_async(MediaInput::Element(prepared_element.clone()));

    wait_for_performed_swap(&prepared_element);
    h.playbin.cancel_prepared_async();

    let seen = h.wait_for("PreparedCancelDeclined", |event, _| {
        matches!(
            event,
            PlaybinEvent::PreparedCancelDeclined { generation }
                if *generation == prepared_generation
        )
    });
    assert_no_eos(&seen, "before the cancel was declined");
    assert!(
        !seen
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::PreparedCancelled { .. })),
        "the cancel raced the swap and WON, so this test never reached the refusal \
         branch: widen the window (longer A, an earlier-draining input) instead of \
         relaxing the assertions; seen: {seen:#?}"
    );

    // The declined cancel changes nothing: the activation lands as if no
    // cancel had been issued.
    let seen = h.wait_for(
        "PreparedActivated after the declined cancel",
        |event, generation| {
            matches!(event, PlaybinEvent::PreparedActivated) && generation == prepared_generation
        },
    );
    assert_no_eos(&seen, "between the declined cancel and the activation");
    let activated_at = Instant::now();

    // And the activated item really plays: its own EOS, no earlier than its
    // playtime after the activation (nothing was synthesized and the input
    // was not torn out).
    let seen = h.wait_for("the final EndOfStream", |event, _| {
        matches!(event, PlaybinEvent::EndOfStream)
    });
    let (_, eos_generation) = seen.last().unwrap();
    assert_eq!(
        *eos_generation, prepared_generation,
        "the final EOS belongs to the item the declined cancel let activate"
    );
    assert!(
        activated_at.elapsed() >= CLIP_MIN,
        "EOS {:?} after activation means the declined cancel cost the new item its \
         playout",
        activated_at.elapsed()
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
    // EXACTLY one, as the doc comment says. The cancel path synthesizes an
    // end when the hold recorded a dropped EOS (`SwapState::dropped_eos`),
    // so a hold that MARKS without dropping produces the real end AND a
    // synthetic one. Only counting catches that: waiting for "an EOS"
    // is satisfied by either of the two.
    let after = h.drain_after(Duration::from_millis(700));
    let extra = after
        .iter()
        .filter(|(event, _)| matches!(event, PlaybinEvent::EndOfStream))
        .count();
    assert_eq!(
        extra, 0,
        "the item ended {extra} extra time(s) after its real end: the cancel \
         synthesized an end that was never consumed; after: {after:#?}"
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

    // The PRECONDITION this test is named for, and the only thing that makes
    // the assertions below mean anything. `wait_for` walks the whole event
    // backlog, so an EOS the hold failed to consume at ~2s still satisfies a
    // wait issued after the cancel: without this check the test passes
    // unchanged on a crate whose gapless EOS hold drops nothing at all
    // (verified by mutation: returning `behind` instead of `pending ||
    // behind` from `gapless_eos_check_and_mark` leaves this test green).
    let before_cancel = h.drain_pending();
    assert!(
        !before_cancel
            .iter()
            .any(|(event, _)| matches!(event, PlaybinEvent::EndOfStream)),
        "the item's real EndOfStream reached the caller while a prepare was still \
         pending, so the hold never consumed it and nothing below exercises the \
         synthesis; seen: {before_cancel:#?}"
    );

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
