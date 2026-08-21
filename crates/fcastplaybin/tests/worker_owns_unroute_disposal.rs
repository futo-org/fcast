//! The blocking half of a text detach (the disposal's flush pair and the
//! queue teardown) must run on the crate worker, never on the decodebin3
//! `pad-removed` streaming thread that delivers the unroute.
//!
//! The deterministic trigger is a gapless swap from a text-bearing item to
//! one without text. The outgoing item's text slot has no successor, so
//! decodebin3 removes its output pad mid-swap from a streaming thread while
//! the branch is still linked into the renderer. Every other path that
//! reaches `detach_text_parts` detaches the branch before decodebin3 removes
//! the pad, so this shape is the one that exercises the hazard.
//!
//! Verification: it fails, naming the streaming thread in the panic, whenever
//! the disposal is dispatched inline from the unroute instead of deferred.

use std::{
    sync::{Arc, Mutex, mpsc},
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint, TrackSlot,
    TrackTarget,
};
use gst::prelude::*;

#[path = "support/text_arm.rs"]
mod text_arm;

const EVENT_TIMEOUT: Duration = Duration::from_secs(30);

/// How long after the activation the disposal may take to be delivered. It
/// only has to cross the worker's queue, so this is generous.
const DISPOSAL_BOUND: Duration = Duration::from_secs(10);

/// Seconds per item.
const CLIP_SECONDS: u32 = 2;

/// The name `FcastPlaybin::new` gives its worker thread.
const WORKER_THREAD: &str = "fcastplaybin";

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        gst::init().unwrap();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

const REQUIRED_FACTORIES: &[&str] = &[
    "videotestsrc",
    "audiotestsrc",
    "vp8enc",
    "vp8dec",
    "vorbisenc",
    "vorbisdec",
    "matroskamux",
    "matroskademux",
    "subparse",
    "decodebin3",
];

/// A silent skip would read as a pass, so a missing plugin fails unless the
/// skip is explicitly allowed.
fn require_plugins() -> bool {
    let missing: Vec<&str> = REQUIRED_FACTORIES
        .iter()
        .copied()
        .filter(|f| gst::ElementFactory::find(f).is_none())
        .collect();
    if missing.is_empty() {
        return true;
    }
    assert!(
        std::env::var_os("FCASTPLAYBIN_ALLOW_PLUGIN_SKIP").is_some(),
        "required GStreamer plugins are missing ({missing:?}) and this test cannot \
         pass without them. Set FCASTPLAYBIN_ALLOW_PLUGIN_SKIP=1 to skip instead."
    );
    eprintln!("skipping: required GStreamer plugins missing: {missing:?}");
    false
}

fn tmp_path(name: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "fcastplaybin-unroute-disposal-{}-{}",
        std::process::id(),
        name
    ))
}

fn run_to_eos(desc: &str) {
    let pipeline = gst::parse::launch(desc).expect("encode pipeline parses");
    pipeline.set_state(gst::State::Playing).unwrap();
    let bus = pipeline.bus().unwrap();
    let msg = bus
        .timed_pop_filtered(
            gst::ClockTime::from_seconds(60),
            &[gst::MessageType::Eos, gst::MessageType::Error],
        )
        .expect("encode finishes");
    if let gst::MessageView::Error(err) = msg.view() {
        panic!("encode pipeline failed: {}", err.error());
    }
    pipeline.set_state(gst::State::Null).unwrap();
}

/// Dense cues so the text branch is still producing when the swap performs.
fn write_srt(name: &str, seconds: u32) -> std::path::PathBuf {
    let path = tmp_path(name);
    let mut srt = String::new();
    for i in 0..(seconds * 1000 / 400) {
        let start = i * 400;
        let end = start + 380;
        let stamp = |ms: u32| {
            format!(
                "{:02}:{:02}:{:02},{:03}",
                ms / 3_600_000,
                (ms / 60_000) % 60,
                (ms / 1000) % 60,
                ms % 1000
            )
        };
        srt.push_str(&format!(
            "{}\n{} --> {}\nCUE{i:02}\n\n",
            i + 1,
            stamp(start),
            stamp(end)
        ));
    }
    std::fs::write(&path, srt).expect("writing the srt file");
    path
}

/// An A/V clip with no text track.
fn encode_av_even(name: &str, pattern: &str, freq: u32) -> String {
    let path = tmp_path(name);
    let desc = format!(
        "videotestsrc num-buffers={} pattern={pattern} \
           ! video/x-raw,width=64,height=64,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         audiotestsrc num-buffers=87 freq={freq} \
           ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! vorbisenc ! mux. \
         matroskamux name=mux ! filesink location={}",
        CLIP_SECONDS * 30,
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

/// The same clip with a muxed subtitle track.
fn encode_av_text(name: &str, pattern: &str, freq: u32) -> String {
    let srt = write_srt(&format!("{name}.srt"), CLIP_SECONDS);
    let path = tmp_path(name);
    let desc = format!(
        "videotestsrc num-buffers={} pattern={pattern} \
           ! video/x-raw,width=64,height=64,framerate=30/1 \
           ! vp8enc deadline=1 cpu-used=8 ! mux. \
         audiotestsrc num-buffers={} freq={freq} \
           ! audio/x-raw,rate=44100,channels=2 ! audioconvert ! vorbisenc ! mux. \
         filesrc location={} ! subparse ! mux. \
         matroskamux name=mux ! filesink location={}",
        CLIP_SECONDS * 30,
        CLIP_SECONDS * 44,
        srt.display(),
        path.display()
    );
    run_to_eos(&desc);
    format!("file://{}", path.display())
}

struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: Mutex<Vec<(PlaybinEvent, u64)>>,
}

impl Harness {
    fn new() -> Self {
        let playbin = FcastPlaybin::new(Sinks {
            video: None,
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
        Self {
            playbin,
            events,
            log: Mutex::new(Vec::new()),
        }
    }

    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: true,
            paused: false,
            seekable: false,
        }
    }

    fn pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(self.gate());
        while let Ok(entry) = self.events.try_recv() {
            if let PlaybinEvent::Error { error, .. } = &entry.0 {
                panic!("pipeline error: {error}");
            }
            self.log.lock().expect("log").push(entry);
        }
    }

    fn wait_for(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent, u64) -> bool) {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let mut seen = 0usize;
        loop {
            self.pump();
            {
                let log = self.log.lock().expect("log");
                while seen < log.len() {
                    let (event, generation) = &log[seen];
                    seen += 1;
                    if pred(event, *generation) {
                        return;
                    }
                }
            }
            assert!(Instant::now() < deadline, "timed out waiting for {what}");
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    fn text_sid(&self) -> Option<String> {
        self.log
            .lock()
            .expect("log")
            .iter()
            .rev()
            .find_map(|(event, _)| match event {
                PlaybinEvent::StreamCollection(collection) => {
                    collection.iter().find_map(|stream| {
                        stream
                            .stream_type()
                            .contains(gst::StreamType::TEXT)
                            .then(|| stream.stream_id().map(|s| s.to_string()))
                            .flatten()
                    })
                }
                _ => None,
            })
    }

    fn shutdown(self) {
        let (done_tx, done_rx) = mpsc::channel();
        self.playbin.shutdown_async(Box::new(move || {
            let _ = done_tx.send(());
        }));
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            match done_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(()) => return,
                Err(mpsc::RecvTimeoutError::Timeout) => {
                    assert!(Instant::now() < deadline, "the shutdown never finished");
                    self.playbin.pump_selection(self.gate());
                    while self.events.try_recv().is_ok() {}
                }
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!("the worker died"),
            }
        }
    }
}

#[test]
fn unroute_disposal_runs_on_the_worker_not_the_streaming_thread() {
    init();
    if !require_plugins() {
        return;
    }
    let first = encode_av_text("unroute-a.mkv", "smpte", 440);
    let second = encode_av_even("unroute-b.mkv", "ball", 880);

    let harness = Harness::new();
    let generation = harness.playbin.load_async(
        MediaInput::Uri(first),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    harness.wait_for("Loaded", |event, seen| {
        matches!(event, PlaybinEvent::Loaded { .. }) && seen == generation
    });
    harness.playbin.play().expect("play");
    harness.wait_for("settled PLAYING", |event, _| {
        matches!(
            event,
            PlaybinEvent::StateChanged {
                current: gst::State::Playing,
                pending: gst::State::VoidPending,
                ..
            }
        )
    });

    // Select the embedded text track and wait for a LIVE branch, so the swap
    // faces a linked text slot. A parked branch would skip the hazard.
    {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        let sid = loop {
            harness.pump();
            if let Some(sid) = harness.text_sid() {
                break sid;
            }
            assert!(
                Instant::now() < deadline,
                "the collection never advertised a text stream"
            );
            std::thread::sleep(Duration::from_millis(15));
        };
        harness
            .playbin
            .request_track(TrackSlot::Subtitle, TrackTarget::Stream(Some(sid)));
    }
    {
        let deadline = Instant::now() + EVENT_TIMEOUT;
        while !text_arm::text_branch_linked(&harness.playbin) {
            harness.pump();
            assert!(
                Instant::now() < deadline,
                "the text branch never linked into the renderer"
            );
            std::thread::sleep(Duration::from_millis(15));
        }
    }

    // The text queue feeding the renderer, found through the live link. The
    // queue is one element upstream of whichever tail `text_arm` resolves,
    // and the disposal that takes it out of the pipeline is the same on both
    // arms.
    let tqueue = text_arm::live_text_tail_pad(&harness.playbin)
        .peer()
        .expect("the text branch's tail is linked")
        .parent_element()
        .expect("the text queue exists");

    // Record who takes the orphaned queue out of the pipeline.
    // `gst_bin_remove` emits `element-removed` synchronously on the calling
    // thread and is the disposal's terminal act on every arm, so it answers
    // both halves of the question (did the disposal run, and on which
    // thread). An intermediate flush is not a reliable signal because the
    // crate may skip the flush pair for a quiescent branch.
    let removal_threads: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    {
        let removal_threads = removal_threads.clone();
        let watched = tqueue.clone();
        harness
            .playbin
            .pipeline()
            .connect_element_removed(move |_bin, element| {
                if element == &watched {
                    let name = std::thread::current()
                        .name()
                        .unwrap_or("<unnamed>")
                        .to_owned();
                    removal_threads.lock().expect("threads").push(name);
                }
            });
    }

    // The gapless swap to a text-less item. decodebin3 removes the text
    // output pad mid-swap from a streaming thread, which is the unroute this
    // test observes.
    let prepared_generation = harness.playbin.prepare_next_async(MediaInput::Uri(second));
    harness.wait_for("PreparedActivated", |event, seen| {
        matches!(event, PlaybinEvent::PreparedActivated) && seen == prepared_generation
    });

    // The disposal must be delivered (the orphaned queue actually left the
    // pipeline) and it must have come from the worker.
    let deadline = Instant::now() + DISPOSAL_BOUND;
    loop {
        harness.pump();
        {
            let threads = removal_threads.lock().expect("threads");
            if !threads.is_empty() {
                assert!(
                    threads.iter().all(|name| name == WORKER_THREAD),
                    "the text branch disposal ran on {threads:?} instead of the worker \
                     (\"{WORKER_THREAD}\"). The pad-removed streaming thread is doing \
                     pipeline surgery, which is the crate rule stated on \
                     Job::FinishActivation and the deadlock class captured in \
                     fuzz-campaign-findings.md"
                );
                break;
            }
        }
        assert!(
            Instant::now() < deadline,
            "the orphaned text queue never left the pipeline within {DISPOSAL_BOUND:?} \
             of the activation, so the detached branch was never disposed of"
        );
        std::thread::sleep(Duration::from_millis(15));
    }

    harness.shutdown();
}
