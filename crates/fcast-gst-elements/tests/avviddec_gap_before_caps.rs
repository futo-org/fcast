//! A GAP event reaching a libav video decoder before any CAPS crashes the
//! process. GstVideoDecoder fabricates output caps for the gap
//! (`gst_video_decoder_negotiate_default_caps`, "Chose default caps ... for
//! initial gap") and negotiates a pool, and avviddec's `decide_allocation ->
//! try_pool -> prepare_dr_pool` then reads `ffmpegdec->context->coded_height`
//! with the context still NULL, because `set_format` never ran. Field crash:
//! SIGSEGV at gstavviddec.c:915, reported against the 3.0.4 flatpak.
//!
//! Two conditions arm it, both ordinary: the decoder is a libav one with
//! direct rendering on (the default) and downstream advertises GstVideoMeta in
//! the allocation query, which every real video sink does. The query probe
//! below stands in for that sink.
//!
//! The pipeline runs in a child process so the red state is a reported
//! failure instead of a dead test binary.
//!
//! The second test sweeps EVERY video decoder the registry offers rather than
//! the one that crashed. A GAP before CAPS is an ordinary upstream shape (a
//! sparse stream, a demuxer filling a hole, a stream that ends before its
//! first buffer), so surviving it is a property every decoder in the build
//! owes us, and the only way to keep that true as decoders come and go is to
//! ask all of them.

// Forces the static GStreamer link line and isolates the process from
// on-disk plugins, exactly like the receiver binary does.
use gst_static_env as _;

use gst::prelude::*;

const CHILD_ENV: &str = "FCAST_GAP_BEFORE_CAPS_CHILD";
/// Which decoder the swept child is to drive.
const CHILD_FACTORY: &str = "FCAST_GAP_BEFORE_CAPS_FACTORY";

fn run_child() {
    gst::init().unwrap();
    // The static build has nothing on disk to scan, so the built-in C plugins
    // (libav here) only exist after this.
    gstreamer_src::init_static_plugins();

    let pipeline = gst::Pipeline::new();
    let factory = std::env::var(CHILD_FACTORY).unwrap_or_else(|_| "avdec_h264".to_string());
    let Ok(dec) = gst::ElementFactory::make(&factory).build() else {
        // Ranks and will not instantiate (no hardware, no device node, no
        // permission). Nothing to prove about a decoder that cannot exist.
        return;
    };
    // No preroll and no clock wait: the pushing thread below would otherwise
    // block in the sink on the gap event.
    let sink = gst::ElementFactory::make("fakesink")
        .property("async", false)
        .property("sync", false)
        .build()
        .unwrap();
    pipeline.add_many([&dec, &sink]).unwrap();
    if gst::Element::link_many([&dec, &sink]).is_err() {
        // A decoder whose src caps fakesink cannot take is not this test's
        // business either.
        let _ = pipeline.set_state(gst::State::Null);
        return;
    }

    // Stand in for a video sink: answer the decoder's allocation query with
    // GstVideoMeta support and no pool, which is what puts avviddec on its
    // direct-rendering path with the pool it builds itself.
    let decsrc = dec.static_pad("src").unwrap();
    decsrc.add_probe(gst::PadProbeType::QUERY_DOWNSTREAM, |_, info| {
        let Some(query) = info.query_mut() else {
            return gst::PadProbeReturn::Ok;
        };
        match query.view_mut() {
            gst::QueryViewMut::Allocation(alloc) => {
                alloc.add_allocation_meta::<gst_video::VideoMeta>(None);
                gst::PadProbeReturn::Handled
            }
            _ => gst::PadProbeReturn::Ok,
        }
    });

    let src = gst::Pad::builder(gst::PadDirection::Src).name("feed").build();
    src.set_active(true).unwrap();
    let Some(decsink) = dec.static_pad("sink") else {
        let _ = pipeline.set_state(gst::State::Null);
        return;
    };
    if src.link(&decsink).is_err() {
        let _ = pipeline.set_state(gst::State::Null);
        return;
    }

    // A refused state change is a decoder declining the job, not a crash.
    if pipeline.set_state(gst::State::Playing).is_err() {
        let _ = pipeline.set_state(gst::State::Null);
        return;
    }

    src.push_event(gst::event::StreamStart::new("gap-only"));
    src.push_event(gst::event::Segment::new(
        &gst::FormattedSegment::<gst::ClockTime>::new(),
    ));
    // No CAPS event, like a decodebin3 slot whose stream ends without ever
    // producing data: the decoder was plugged from the collection's caps, and
    // the only thing that ever reaches it is the gap that seeded the slot.
    src.push_event(gst::event::Gap::new(
        gst::ClockTime::ZERO,
        gst::ClockTime::ZERO,
    ));
    src.push_event(gst::event::Eos::new());

    let _ = pipeline.set_state(gst::State::Null);
}

#[test]
fn gap_before_caps_does_not_crash_the_libav_decoder() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
        return;
    }
    let status = std::process::Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "gap_before_caps_does_not_crash_the_libav_decoder"])
        .env(CHILD_ENV, "1")
        .status()
        .unwrap();
    assert!(
        status.success(),
        "the gap-before-caps child died: {status} (signal 11 is avviddec's NULL context)"
    );
}

/// Every video decoder in the build survives a GAP before any CAPS.
///
/// One child per factory, so a decoder that dies by signal is a named
/// failure rather than a dead run, and so one crash does not hide the rest.
#[test]
fn no_video_decoder_crashes_on_a_gap_before_caps() {
    if std::env::var_os(CHILD_ENV).is_some() {
        run_child();
        return;
    }
    gst::init().unwrap();
    gstreamer_src::init_static_plugins();

    let video = gst::Caps::builder("video/x-raw").build();
    let decoders: Vec<String> = gst::ElementFactory::factories_with_type(
        gst::ElementFactoryType::DECODER,
        gst::Rank::MARGINAL,
    )
    .iter()
    // Src caps that name raw video: this is about video decoders, and
    // parsebin and decodebin are classed as decoders too and take anything.
    .filter(|factory| {
        factory.static_pad_templates().iter().any(|template| {
            template.direction() == gst::PadDirection::Src
                && !template.caps().is_any()
                && template.caps().can_intersect(&video)
        })
    })
    .map(|factory| factory.name().to_string())
    .collect();
    assert!(
        !decoders.is_empty(),
        "the static build registered no video decoder at all"
    );

    // Printed, so a build that registers three decoders instead of twenty
    // does not read as the same green (`cargo test -- --nocapture`).
    eprintln!("sweeping {} video decoders: {decoders:?}", decoders.len());
    let mut died = Vec::new();
    for factory in &decoders {
        let status = std::process::Command::new(std::env::current_exe().unwrap())
            .args(["--exact", "no_video_decoder_crashes_on_a_gap_before_caps"])
            .env(CHILD_ENV, "1")
            .env(CHILD_FACTORY, factory)
            .status()
            .unwrap();
        if !status.success() {
            died.push(format!("{factory}: {status}"));
        }
    }
    assert!(
        died.is_empty(),
        "a gap before caps killed {} of {} decoders: {died:#?}",
        died.len(),
        decoders.len()
    );
}
