//! The buffering knob observed at the element level.
//!
//! `ftest://` media has no buffering element, so a scenario without the knob
//! posts no `GST_MESSAGE_BUFFERING` at all and a consumer's buffering state
//! machine is unreachable by construction. These tests prove the knob posts
//! the messages the spec describes, when it describes them, and that every
//! dip ends with a 100 so a consumer can leave the state.
//!
//! Delivery positions are sampled inside the bus sync handler, on the posting
//! thread, so the "which buffer had been delivered when this was posted"
//! assertions are not blurred by a bus round trip.

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicI64, Ordering},
    },
    time::{Duration, Instant},
};

use fcasttest::{
    scenario::ScenarioBuilder,
    spec::{BufferingDip, BufferingRecovery, BufferingSpec, Pacing},
};
use gst::prelude::*;

const TIMEOUT: Duration = Duration::from_secs(15);

fn init() {
    use std::sync::Once;
    static INIT: Once = Once::new();
    INIT.call_once(fcasttest::register_for_tests);
}

/// One recorded buffering post.
#[derive(Clone, Debug)]
struct Post {
    percent: i32,
    /// Highest video buffer index a sink had seen when the post happened, or
    /// -1 before the first buffer. ftestsrc stamps the schedule index into
    /// the buffer offset, which is what makes this exact.
    video_index: i64,
    at: Instant,
}

/// A bare pipeline over ftestsrc with one counting fakesink per exposed pad.
/// No parser, no decoder. The knob posts from the source, so nothing else is
/// needed to observe it.
struct Harness {
    pipeline: gst::Pipeline,
    posts: Arc<Mutex<Vec<Post>>>,
    errors: Arc<Mutex<Vec<String>>>,
    eos: Arc<AtomicBool>,
}

impl Harness {
    fn play(uri: &str) -> Self {
        let pipeline = gst::Pipeline::new();
        let src = gst::ElementFactory::make("ftestsrc")
            .property("uri", uri)
            .build()
            .expect("ftestsrc");
        pipeline.add(&src).expect("adding ftestsrc");

        let video_index = Arc::new(AtomicI64::new(-1));
        {
            let pipeline = pipeline.downgrade();
            let video_index = video_index.clone();
            src.connect_pad_added(move |_, pad| {
                let Some(pipeline) = pipeline.upgrade() else {
                    return;
                };
                let sink = gst::ElementFactory::make("fakesink")
                    .property("sync", false)
                    .build()
                    .expect("fakesink");
                pipeline.add(&sink).expect("adding fakesink");
                let sinkpad = sink.static_pad("sink").expect("fakesink sink pad");
                if pad.name().starts_with("video") {
                    let video_index = video_index.clone();
                    sinkpad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
                        if let Some(gst::PadProbeData::Buffer(buffer)) = &info.data {
                            video_index.store(buffer.offset() as i64, Ordering::SeqCst);
                        }
                        gst::PadProbeReturn::Ok
                    });
                }
                pad.link(&sinkpad).expect("linking an exposed pad");
                sink.sync_state_with_parent().expect("syncing fakesink");
            });
        }

        let posts = Arc::new(Mutex::new(Vec::new()));
        let errors = Arc::new(Mutex::new(Vec::new()));
        let eos = Arc::new(AtomicBool::new(false));
        {
            let posts = posts.clone();
            let errors = errors.clone();
            let eos = eos.clone();
            let bus = pipeline.bus().expect("pipeline bus");
            bus.set_sync_handler(move |_, msg| {
                match msg.view() {
                    gst::MessageView::Buffering(buffering) => {
                        posts.lock().unwrap().push(Post {
                            percent: buffering.percent(),
                            video_index: video_index.load(Ordering::SeqCst),
                            at: Instant::now(),
                        });
                    }
                    gst::MessageView::Error(err) => {
                        errors
                            .lock()
                            .unwrap()
                            .push(format!("{} ({:?})", err.error(), err.debug()));
                    }
                    gst::MessageView::Eos(_) => eos.store(true, Ordering::SeqCst),
                    _ => (),
                }
                gst::BusSyncReply::Drop
            });
        }

        pipeline
            .set_state(gst::State::Playing)
            .expect("pipeline to PLAYING");
        Self {
            pipeline,
            posts,
            errors,
            eos,
        }
    }

    fn posts(&self) -> Vec<Post> {
        self.posts.lock().unwrap().clone()
    }

    fn wait_posts(&self, what: &str, mut done: impl FnMut(&[Post]) -> bool) -> Vec<Post> {
        let deadline = Instant::now() + TIMEOUT;
        loop {
            {
                let errors = self.errors.lock().unwrap();
                assert!(
                    errors.is_empty(),
                    "pipeline error while waiting for {what}: {errors:?}"
                );
            }
            let posts = self.posts();
            if done(&posts) {
                return posts;
            }
            assert!(
                Instant::now() < deadline,
                "timed out waiting for {what}, posts so far {posts:?}"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn shutdown(self) {
        self.pipeline
            .set_state(gst::State::Null)
            .expect("pipeline to NULL");
    }
}

#[test]
fn an_initial_buffering_period_posts_low_then_100_and_nothing_more() {
    init();
    let media = ScenarioBuilder::new("bufmsginitial")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(4))
        .pacing(Pacing::Realtime)
        .buffering(BufferingSpec::new(15).with_initial_ms(400))
        .register();
    let harness = Harness::play(&media.uri());

    let posts = harness.wait_posts("the initial low and its recovery", |posts| posts.len() >= 2);
    assert_eq!(posts[0].percent, 15, "the low post carries low_percent");
    assert_eq!(posts[1].percent, 100, "the recovery is a full 100");
    let held = posts[1].at.duration_since(posts[0].at);
    assert!(
        held >= Duration::from_millis(350),
        "the low period lasted only {held:?} against a 400 ms spec"
    );

    // The spec describes one initial period and nothing else, so nothing else
    // may be posted.
    std::thread::sleep(Duration::from_millis(300));
    let posts = harness.posts();
    assert_eq!(
        posts.len(),
        2,
        "the schedule kept posting past its spec: {posts:?}"
    );

    harness.shutdown();
    media.unregister();
}

#[test]
fn a_dip_anchored_to_a_buffer_fires_there_and_recovers_on_release() {
    init();
    let media = ScenarioBuilder::new("bufmsgdip")
        .video("video_0")
        .duration(gst::ClockTime::from_seconds(8))
        .pacing(Pacing::Realtime)
        .buffering(BufferingSpec::new(20).with_dip(BufferingDip {
            stream: "video_0".to_owned(),
            buffer_index: 10,
            recovery: BufferingRecovery::OnSyncPoint("refill".to_owned()),
        }))
        .register();
    let harness = Harness::play(&media.uri());

    let posts = harness.wait_posts("the anchored low", |posts| !posts.is_empty());
    assert_eq!(posts[0].percent, 20);
    // The anchoring, both directions. The dip must not fire before its buffer
    // is delivered, and at 25 fps realtime a post trailing its anchor by more
    // than half a second of frames is not anchored to it at all.
    assert!(
        posts[0].video_index >= 10,
        "the dip is anchored to buffer 10 but fired when the sink had only \
         seen index {}",
        posts[0].video_index
    );
    assert!(
        posts[0].video_index <= 22,
        "the dip fired with the sink already at index {}, far past its anchor",
        posts[0].video_index
    );

    // The recovery is gated on a sync point the test has not released, so no
    // 100 may appear on its own.
    std::thread::sleep(Duration::from_millis(400));
    let posts = harness.posts();
    assert_eq!(
        posts.len(),
        1,
        "a recovery was posted before the gate released: {posts:?}"
    );

    media.release("refill");
    let posts = harness.wait_posts("the recovery after the release", |posts| posts.len() >= 2);
    assert_eq!(posts[1].percent, 100);

    harness.shutdown();
    media.unregister();
}

#[test]
fn periodic_dips_alternate_low_and_100() {
    init();
    let media = ScenarioBuilder::new("bufmsgperiodic")
        .audio("audio_0")
        .duration(gst::ClockTime::from_seconds(6))
        .pacing(Pacing::Realtime)
        .buffering(BufferingSpec::new(35).with_periodic(400, 100))
        .register();
    let harness = Harness::play(&media.uri());

    let posts = harness.wait_posts("two full periodic dips", |posts| posts.len() >= 4);
    for (index, post) in posts.iter().take(4).enumerate() {
        let expected = if index % 2 == 0 { 35 } else { 100 };
        assert_eq!(
            post.percent, expected,
            "post {index} breaks the low/100 alternation: {posts:?}"
        );
    }

    harness.shutdown();
    media.unregister();
}

#[test]
fn a_scenario_without_the_knob_posts_nothing() {
    init();
    let media = ScenarioBuilder::new("bufmsgnone")
        .audio("audio_0")
        .duration(gst::ClockTime::from_mseconds(600))
        .pacing(Pacing::Realtime)
        .register();
    let harness = Harness::play(&media.uri());

    // Play the whole clip out, so the assertion covers the element's entire
    // life and not a lucky early sample.
    let deadline = Instant::now() + TIMEOUT;
    while !harness.eos.load(Ordering::SeqCst) {
        assert!(Instant::now() < deadline, "the clip never reached EOS");
        std::thread::sleep(Duration::from_millis(10));
    }
    let posts = harness.posts();
    assert!(
        posts.is_empty(),
        "media without the knob posted buffering: {posts:?}"
    );

    harness.shutdown();
    media.unregister();
}
