//! Builder over [`crate::spec`] plus the handle tests drive a registered
//! scenario through.

use std::sync::Arc;

use crate::{
    registry::{self, Scenario, SyncPoint},
    sink::{FTestSink, Recording},
    spec::{BufferingSpec, CueSpec, MediaSpec, Pacing, StreamSpec},
};

use super::timeline::Timeline;

/// FNV-1a over the key, so an unseeded scenario still has a seed that is fully
/// determined by its name. No clock, no RNG, no counter.
pub fn stable_seed(key: &str) -> u64 {
    const OFFSET: u64 = 0xcbf2_9ce4_8422_2325;
    const PRIME: u64 = 0x0000_0100_0000_01b3;
    let mut hash = OFFSET;
    for byte in key.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(PRIME);
    }
    hash
}

/// Keys name a scenario in the registry, in `ftest://<key>` and inside every
/// stream-id, so `-` (the stream-id separator) and `/` (the recording-key
/// separator) are out.
fn validate_key(key: &str) {
    assert!(!key.is_empty(), "scenario keys must not be empty");
    assert!(
        key.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'),
        "scenario key {key:?} must be ASCII alphanumeric or '_' \
         ('-' separates stream-ids, '/' separates recording handles)"
    );
}

/// Assembles a [`MediaSpec`] and registers it under a caller-supplied key.
///
/// The key is the reproducibility anchor. It names the scenario, seeds every
/// jitter draw (see [`stable_seed`]) and appears in every stream-id, so two
/// runs of the same test describe byte-identical media.
pub struct ScenarioBuilder {
    key: String,
    spec: MediaSpec,
    duration: Option<gst::ClockTime>,
    pacing: Option<Pacing>,
    bytes_per_buffer: Option<usize>,
}

impl ScenarioBuilder {
    pub fn new(key: impl Into<String>) -> Self {
        let key = key.into();
        validate_key(&key);
        let seed = stable_seed(&key);
        Self {
            key,
            spec: MediaSpec::new(seed),
            duration: None,
            pacing: None,
            bytes_per_buffer: None,
        }
    }

    /// Overrides the key-derived seed.
    pub fn seed(mut self, seed: u64) -> Self {
        self.spec.seed = seed;
        self
    }

    pub fn stream(mut self, stream: StreamSpec) -> Self {
        self.spec.streams.push(stream);
        self
    }

    pub fn video(self, id: impl Into<String>) -> Self {
        self.stream(StreamSpec::video(id))
    }

    pub fn audio(self, id: impl Into<String>) -> Self {
        self.stream(StreamSpec::audio(id))
    }

    pub fn text(self, id: impl Into<String>, cues: Vec<CueSpec>) -> Self {
        self.stream(StreamSpec::text(id, cues))
    }

    /// Applied to every stream at [`register`](Self::register) time,
    /// whatever the individual specs say.
    pub fn duration(mut self, duration: gst::ClockTime) -> Self {
        self.duration = Some(duration);
        self
    }

    /// Applied to every stream, see [`duration`](Self::duration).
    pub fn pacing(mut self, pacing: Pacing) -> Self {
        self.pacing = Some(pacing);
        self
    }

    /// Applied to every stream, see [`duration`](Self::duration).
    pub fn bytes_per_buffer(mut self, bytes: usize) -> Self {
        self.bytes_per_buffer = Some(bytes);
        self
    }

    /// Scheduled buffering messages over the whole media, see
    /// [`BufferingSpec`]. Without this a scenario never reports buffering at
    /// all, because ftest media has no buffering element.
    pub fn buffering(mut self, buffering: BufferingSpec) -> Self {
        self.spec.buffering = Some(buffering);
        self
    }

    /// Models an adaptive main input. The source answers the SELECTABLE
    /// query, so decodebin3 defers all stream selection upstream. See
    /// [`MediaSpec::upstream_selection`].
    pub fn upstream_selection(mut self) -> Self {
        self.spec.upstream_selection = true;
        self
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    /// The media exactly as [`register`](Self::register) will store it,
    /// top-level overrides already applied. Check a description against
    /// this, not against the per-stream entries the overrides still win
    /// over.
    pub fn resolved_spec(&self) -> MediaSpec {
        let mut spec = self.spec.clone();
        for stream in &mut spec.streams {
            if let Some(duration) = self.duration {
                stream.duration = duration;
            }
            if let Some(pacing) = self.pacing {
                stream.pacing = pacing;
            }
            if let Some(bytes) = self.bytes_per_buffer {
                stream.bytes_per_buffer = bytes;
            }
        }
        spec
    }

    /// Registers the scenario, replacing any previous entry under the same
    /// key. Call before building the pipeline, since registering discards
    /// stashed handles.
    pub fn register(self) -> ScenarioHandle {
        let spec = self.resolved_spec();
        ScenarioHandle {
            scenario: registry::register_scenario(self.key, spec),
        }
    }
}

/// A registered scenario, as tests hold it. Cheap to clone, and nothing to
/// clean up on drop. A push parked on a sync point is unparked by the flush
/// or teardown that ends the pipeline, so a panicking test leaves no stuck
/// streaming thread.
#[derive(Clone)]
pub struct ScenarioHandle {
    scenario: Arc<Scenario>,
}

impl ScenarioHandle {
    pub fn key(&self) -> &str {
        self.scenario.key()
    }

    /// The URI a test hands to the real load API.
    pub fn uri(&self) -> String {
        self.scenario.uri()
    }

    pub fn scenario(&self) -> Arc<Scenario> {
        self.scenario.clone()
    }

    pub fn spec(&self) -> &MediaSpec {
        self.scenario.spec()
    }

    /// Full stream-id of one of this scenario's streams, as it reaches
    /// fcastplaybin's selection API.
    pub fn stream_id(&self, suffix: &str) -> String {
        self.scenario.stream_id(suffix)
    }

    /// The named gate, created on first use by whichever side asks first (the
    /// stalling push or the test).
    pub fn sync_point(&self, name: &str) -> Arc<SyncPoint> {
        self.scenario.sync_point(name)
    }

    pub fn release(&self, name: &str) {
        self.scenario.sync_point(name).release();
    }

    pub fn release_all(&self) {
        self.scenario.release_all_sync_points();
    }

    /// Event-anchored actions over this scenario, see [`Timeline`].
    pub fn timeline(&self) -> Timeline {
        Timeline::new(self.scenario.clone())
    }

    /// Builds a recording sink and stashes its log under `name`, so a scenario
    /// whose sink the test cannot reach still exposes one. The element is
    /// returned for tests that place it themselves.
    pub fn recording_sink(&self, name: &str) -> (FTestSink, Recording) {
        let sink = FTestSink::new();
        let recording = sink.recording();
        self.scenario.set_handle(name, Arc::new(recording.clone()));
        (sink, recording)
    }

    pub fn recording(&self, name: &str) -> Option<Recording> {
        self.scenario
            .handle::<Recording>(name)
            .map(|handle| (*handle).clone())
    }

    /// Value for a sink's `recording-key` property, so an element the test only
    /// reaches by factory name still publishes its log here.
    pub fn recording_key(&self, name: &str) -> String {
        format!("{}/{name}", self.scenario.key())
    }

    pub fn unregister(&self) {
        registry::unregister(self.scenario.key());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{DecoderKnobs, Fault};

    #[test]
    fn same_key_builds_the_same_media() {
        let build = |key: &str| {
            ScenarioBuilder::new(key)
                .video("video_0")
                .audio("audio_0")
                .duration(gst::ClockTime::from_mseconds(400))
                .bytes_per_buffer(64)
                .register()
        };

        let first = build("determ");
        let seed = first.spec().seed;
        let ids: Vec<String> = first
            .spec()
            .streams
            .iter()
            .map(|stream| first.stream_id(&stream.id))
            .collect();

        let second = build("determ");
        assert_eq!(second.spec().seed, seed);
        assert_eq!(second.spec().seed, stable_seed("determ"));
        assert_eq!(
            second
                .spec()
                .streams
                .iter()
                .map(|stream| second.stream_id(&stream.id))
                .collect::<Vec<_>>(),
            ids
        );
        assert_eq!(ids[0], "ftest-determ-video_0");
        assert_eq!(second.uri(), "ftest://determ");

        // A different key means different jitter draws, still without a clock.
        assert_ne!(build("determ2").spec().seed, seed);

        for key in ["determ", "determ2"] {
            registry::unregister(key);
        }
    }

    #[test]
    fn overrides_reach_every_stream_and_per_stream_knobs_survive() {
        let handle = ScenarioBuilder::new("overrides")
            .stream(
                StreamSpec::video("video_0")
                    .with_duration(gst::ClockTime::from_seconds(99))
                    .with_decoder(DecoderKnobs {
                        latency: Some(gst::ClockTime::from_mseconds(150)),
                        ..DecoderKnobs::default()
                    })
                    .with_fault(Fault::StallAt {
                        buffer_index: 3,
                        sync_point: "stall".to_owned(),
                    }),
            )
            .audio("audio_0")
            .duration(gst::ClockTime::from_mseconds(500))
            .pacing(Pacing::Realtime)
            .bytes_per_buffer(32)
            .register();

        for stream in &handle.spec().streams {
            assert_eq!(stream.duration, gst::ClockTime::from_mseconds(500));
            assert_eq!(stream.pacing, Pacing::Realtime);
            assert_eq!(stream.bytes_per_buffer, 32);
        }
        let video = handle.spec().stream("video_0").expect("the video stream");
        assert_eq!(
            video.decoder.latency,
            Some(gst::ClockTime::from_mseconds(150))
        );
        assert_eq!(video.faults.len(), 1);

        handle.unregister();
    }

    #[test]
    fn buffering_reaches_the_registered_spec() {
        use crate::spec::{BufferingDip, BufferingRecovery, BufferingSpec, PeriodicBuffering};

        let handle = ScenarioBuilder::new("bufknob")
            .audio("audio_0")
            .buffering(
                BufferingSpec::new(25)
                    .with_initial_ms(100)
                    .with_periodic(400, 50)
                    .with_dip(BufferingDip {
                        stream: "audio_0".to_owned(),
                        buffer_index: 3,
                        recovery: BufferingRecovery::OnSyncPoint("refill".to_owned()),
                    }),
            )
            .register();
        let buffering = handle
            .spec()
            .buffering
            .as_ref()
            .expect("the registered buffering spec");
        assert_eq!(buffering.low_percent, 25);
        assert_eq!(buffering.initial_ms, Some(100));
        assert_eq!(
            buffering.periodic,
            Some(PeriodicBuffering {
                period_ms: 400,
                low_ms: 50
            })
        );
        assert_eq!(
            buffering.dips,
            vec![BufferingDip {
                stream: "audio_0".to_owned(),
                buffer_index: 3,
                recovery: BufferingRecovery::OnSyncPoint("refill".to_owned()),
            }]
        );
        handle.unregister();
    }

    #[test]
    fn handles_are_stashed_and_fetched_by_name() {
        gst::init().unwrap();
        crate::sink::register().unwrap();
        let handle = ScenarioBuilder::new("stash").audio("audio_0").register();
        assert_eq!(handle.recording_key("audio"), "stash/audio");
        assert!(handle.recording("audio").is_none());

        let (_sink, recording) = handle.recording_sink("audio");
        recording.push(crate::sink::RecordEntry::synthetic_buffer(
            gst::ClockTime::ZERO,
        ));
        assert_eq!(
            handle.recording("audio").map(|r| r.buffer_count()),
            Some(1),
            "the stashed handle shares the log"
        );

        handle.unregister();
    }

    #[test]
    #[should_panic(expected = "must be ASCII alphanumeric")]
    fn keys_with_the_stream_id_separator_are_refused() {
        ScenarioBuilder::new("bad-key");
    }
}
