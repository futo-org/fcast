//! Temporary probe, not part of the suite. Prints position/duration
//! samples for a clip loaded through the real pipeline. Run with:
//!   FCASTPLAYBIN_PROBE=/path/to/file.ogg cargo test -p fcastplaybin \
//!     --test position_probe -- --nocapture --ignored

use std::time::Duration;

use fcastplaybin::{AudioSink, FcastPlaybin, MediaInput, PlaybinEvent, Sinks, StartPoint};

#[test]
#[ignore]
fn probe_position_after_load() {
    let Ok(path) = std::env::var("FCASTPLAYBIN_PROBE") else {
        eprintln!("set FCASTPLAYBIN_PROBE");
        return;
    };
    assert!(
        std::path::Path::new(&path).is_file(),
        "FCASTPLAYBIN_PROBE does not point at a file: {path}"
    );
    gst::init().unwrap();
        // The receiver's part of the pipeline: fcastaudiostretch is built by
        // the fcastplaybin constructor but registered by the application.
        fcast_gst_elements::fcastaudiostretch::plugin_init()
            .expect("registering fcastaudiostretch");
    let playbin = FcastPlaybin::new(Sinks {
        video: None,
        audio: AudioSink::Factory(Box::new(|| {
            Ok(gst::ElementFactory::make("fakesink")
                .property("sync", true)
                .build()?)
        })),
    })
    .unwrap();
    let (tx, rx) = std::sync::mpsc::channel();
    playbin.set_event_handler(None, move |event, generation| {
        let _ = tx.send((format!("{event:?}"), generation));
    });
    let generation = playbin.load_async(
        MediaInput::Uri(format!("file://{path}")),
        StartPoint::Seek {
            position: gst::ClockTime::ZERO,
            rate: 1.0,
        },
    );
    loop {
        let (event, g) = rx.recv_timeout(Duration::from_secs(10)).expect("loaded");
        if event.starts_with("Loaded") && g == generation {
            break;
        }
    }
    playbin.play().expect("play");
    let mut samples = Vec::new();
    for i in 0..8 {
        std::thread::sleep(Duration::from_millis(400));
        let position = playbin.position();
        eprintln!(
            "t+{:.1}s position={:?} duration={:?}",
            0.4 * (i as f64 + 1.0),
            position,
            playbin.duration()
        );
        samples.push(position);
    }
    let _ = playbin.stop();
    let _ = PlaybinEvent::EndOfStream; // keep the import used

    // A probe that only prints cannot fail, so a run of it never told anyone
    // anything they did not read by eye. The two properties every one of these
    // dumps was inspected FOR are cheap to state, so state them: the clip
    // answers a position at all, and that position moves over 3.2 s of
    // playback. Neither can hold on the freeze this probe was written to chase.
    let readings: Vec<gst::ClockTime> = samples.iter().flatten().copied().collect();
    assert!(
        readings.len() >= 2,
        "the pipeline answered fewer than two positions over 3.2 s of playback: {samples:?}"
    );
    assert!(
        readings[readings.len() - 1] > readings[0],
        "the position never advanced over 3.2 s of playback (frozen clock): {samples:?}"
    );
}
