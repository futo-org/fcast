use std::{
    cell::{Cell, RefCell},
    path::PathBuf,
    sync::mpsc,
    thread,
    time::{Duration, Instant},
};

use fcastplaybin::{
    AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, SelectionGate, Sinks, StartPoint,
    state_machine::Seek,
};
use fcasttest::sink::{FTestSink, Recording};
use gst::prelude::*;

mod support;

/// Generous bound, the media is 6 s served from loopback.
const EVENT_TIMEOUT: Duration = Duration::from_secs(40);

fn init() {
    static INIT: std::sync::Once = std::sync::Once::new();
    INIT.call_once(|| {
        if let Ok(filter) = std::env::var("FCASTPLAYBIN_TEST_LOG") {
            let _ = tracing_subscriber::fmt()
                .with_env_filter(format!("fcastplaybin={filter}"))
                .try_init();
        }
        fcasttest::register_for_tests();
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    });
}

/// The patched `.so`, but only if `hlsdemux2` really resolves to it (a plugin
/// that fails to load silently falls back to the system copy).
fn patched_adaptivedemux2() -> Result<PathBuf, String> {
    let want = std::env::var_os("FCAST_PATCHED_ADAPTIVEDEMUX2")
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
        .ok_or_else(|| "FCAST_PATCHED_ADAPTIVEDEMUX2 is unset".to_owned())?;
    let factory = gst::ElementFactory::find("hlsdemux2")
        .ok_or_else(|| "no hlsdemux2 element factory at all".to_owned())?;
    let have = factory
        .plugin()
        .and_then(|plugin| plugin.filename())
        .ok_or_else(|| "hlsdemux2 has no plugin file".to_owned())?;
    let canonical = |path: &PathBuf| path.canonicalize().unwrap_or_else(|_| path.clone());
    if canonical(&have) != canonical(&want) {
        return Err(format!(
            "hlsdemux2 resolves to {} instead of {}",
            have.display(),
            want.display()
        ));
    }
    Ok(want)
}

/// `true` when the test should run, prints loudly and skips otherwise.
fn gated_in() -> bool {
    init();
    match patched_adaptivedemux2() {
        Ok(path) => {
            println!(
                "running against the patched adaptivedemux2: {}",
                path.display()
            );
            true
        }
        Err(why) => {
            if std::env::var_os("FCAST_HLS_CODEC_FAMILY_TEST_FORCE").is_some() {
                println!(
                    "!! FORCED: {why}. Expect a fatal stream error, this is \
                     the unpatched half of the A/B."
                );
                return true;
            }
            println!(
                "\n\
                 ================================================================\n\
                 SKIPPING regression_hls_codec_family: {why}.\n\
                 This test needs the patched adaptivedemux2 plugin:\n\
                 \x20  eval \"$(cargo xtask patched-plugins --quiet)\"\n\
                 Unpatched GStreamer fails here BY DESIGN, so the suite\n\
                 skips instead of reporting an upstream bug as a local one.\n\
                 ================================================================\n"
            );
            false
        }
    }
}

/// The transport-redriving parts of the `tests/dash_testbed.rs` harness.
struct Harness {
    playbin: FcastPlaybin,
    events: mpsc::Receiver<(PlaybinEvent, u64)>,
    log: RefCell<Vec<PlaybinEvent>>,
    buffering: Cell<i32>,
    loading: Cell<bool>,
    wants_playing: Cell<bool>,
    parked_seek: Cell<Option<Seek>>,
    video: Recording,
}

impl Harness {
    fn new() -> Self {
        let video_sink = FTestSink::new();
        let video = video_sink.recording();
        let playbin = FcastPlaybin::new(Sinks {
            video: Some(video_sink.upcast()),
            audio: AudioSink::Factory(Box::new(|| Ok(FTestSink::new().upcast()))),
        })
        .expect("building fcastplaybin");
        let (tx, events) = mpsc::channel();
        playbin.set_event_handler(None, move |event, generation| {
            let _ = tx.send((event, generation));
        });
        Self {
            playbin,
            events,
            log: RefCell::new(Vec::new()),
            buffering: Cell::new(100),
            loading: Cell::new(false),
            wants_playing: Cell::new(false),
            parked_seek: Cell::new(None),
            video,
        }
    }

    /// Put back the transport the crate parked: a parked seek first, then the
    /// PLAYING target the pipeline dropped when a branch joined.
    fn redrive_transport(&self, event: &PlaybinEvent) {
        match event {
            PlaybinEvent::Buffering(percent) => self.buffering.set(*percent),
            PlaybinEvent::Loaded { .. } => self.loading.set(false),
            PlaybinEvent::QueueSeek(seek) => self.parked_seek.set(Some(*seek)),
            PlaybinEvent::StateChanged {
                current: gst::State::Paused,
                pending: gst::State::VoidPending,
                ..
            } => {
                if let Some(seek) = self.parked_seek.take() {
                    self.playbin.seek_async(seek);
                } else if self.wants_playing.get() {
                    let _ = self.playbin.play();
                }
            }
            _ => {}
        }
    }

    fn gate(&self) -> SelectionGate {
        SelectionGate {
            quiet: self.buffering.get() >= 100 && !self.loading.get(),
            paused: false,
            seekable: {
                let mut query = gst::query::Seeking::new(gst::Format::Time);
                self.playbin.pipeline().query(&mut query) && query.result().0
            },
        }
    }

    fn settle_pump(&self) {
        self.playbin.poll_text_policy();
        self.playbin.pump_selection(self.gate());
    }

    fn wait_for(&self, what: &str, mut pred: impl FnMut(&PlaybinEvent) -> bool) {
        for event in self.log.borrow().iter() {
            if pred(event) {
                return;
            }
        }
        let deadline = Instant::now() + EVENT_TIMEOUT;
        loop {
            if Instant::now() >= deadline {
                panic!(
                    "timed out waiting for {what} within {EVENT_TIMEOUT:?}; log: {:#?}",
                    self.log.borrow()
                );
            }
            self.settle_pump();
            match self.events.recv_timeout(Duration::from_millis(20)) {
                Ok((event, _generation)) => {
                    if let PlaybinEvent::Error { error, .. } = &event {
                        panic!(
                            "pipeline error while waiting for {what}: {error} (log: {:#?})",
                            self.log.borrow()
                        );
                    }
                    self.redrive_transport(&event);
                    let hit = pred(&event);
                    self.log.borrow_mut().push(event);
                    if hit {
                        return;
                    }
                }
                Err(mpsc::RecvTimeoutError::Timeout) => {}
                Err(mpsc::RecvTimeoutError::Disconnected) => panic!(
                    "event channel closed while waiting for {what}; log: {:#?}",
                    self.log.borrow()
                ),
            }
        }
    }

    fn load_and_play(&self, uri: &str) {
        self.loading.set(true);
        self.playbin.load_async(
            MediaInput::Uri(uri.to_owned()),
            StartPoint::Seek {
                position: gst::ClockTime::ZERO,
                rate: 1.0,
            },
        );
        self.wait_for("Loaded", |event| {
            matches!(event, PlaybinEvent::Loaded { .. })
        });
        self.playbin.play().expect("play");
        self.wants_playing.set(true);
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = self.playbin.stop();
    }
}

/// The regression: a mixed-codec master must play to EOS, upswitching within
/// the starting codec family and never touching the other one.
#[test]
fn hls_bitrate_switch_stays_in_codec_family() {
    if !gated_in() {
        return;
    }
    let server = support::FileServer::serve(support::hls_fixtures());
    let harness = Harness::new();

    harness.load_and_play(&server.url("master.m3u8"));
    harness.wait_for("EndOfStream", |event| {
        matches!(event, PlaybinEvent::EndOfStream)
    });

    // Let any in-flight fetch land in the request log
    thread::sleep(Duration::from_millis(100));

    // One fetch of anything under vp9/ is the bug
    assert_eq!(
        server.fetches("vp9/"),
        0,
        "the codec-family filter let ABR reach the VP9 variant; timeline: {:#?}",
        server.timeline(Instant::now())
    );

    // The filter must narrow ABR, not freeze it: playlist plus a segment
    assert!(
        server.fetches("mid/") >= 2,
        "ABR never switched within the H.264 family; timeline: {:#?}",
        server.timeline(Instant::now())
    );

    assert!(
        harness.video.buffer_count() > 0,
        "EOS with no video frames rendered"
    );
}
