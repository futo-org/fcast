//! Process-global scenario registry.
//!
//! Autoplugged elements cannot be handed test state through properties, so
//! every element resolves what it needs here: ftestsrc by URI key, ftestdec by
//! the key it parses out of its sink-pad stream-id.

use std::{
    any::Any,
    collections::HashMap,
    sync::{Arc, LazyLock},
    time::{Duration, Instant},
};

use parking_lot::{Condvar, Mutex};

use crate::{
    caps,
    spec::{DecoderKnobs, MediaSpec, StreamSpec},
};

static SCENARIOS: LazyLock<Mutex<HashMap<String, Arc<Scenario>>>> =
    LazyLock::new(|| Mutex::new(HashMap::new()));

/// Registers a scenario under `key`, replacing any previous entry.
pub fn register_scenario(key: impl Into<String>, spec: MediaSpec) -> Arc<Scenario> {
    let key = key.into();
    let scenario = Arc::new(Scenario::new(key.clone(), spec));
    if SCENARIOS
        .lock()
        .insert(key.clone(), scenario.clone())
        .is_some()
    {
        tracing::warn!("fcasttest: scenario {key} replaced while registered");
    }
    scenario
}

pub fn lookup(key: &str) -> Option<Arc<Scenario>> {
    SCENARIOS.lock().get(key).cloned()
}

pub fn unregister(key: &str) {
    SCENARIOS.lock().remove(key);
}

/// Resolves the owning scenario from a full stream-id.
pub fn scenario_for_stream_id(stream_id: &str) -> Option<Arc<Scenario>> {
    lookup(caps::key_from_stream_id(stream_id)?)
}

/// Knobs for the stream a decoder was plugged for, resolved lazily from its
/// sink-pad stream-id.
pub fn decoder_knobs_for_stream_id(stream_id: &str) -> Option<DecoderKnobs> {
    let (key, suffix) = caps::split_stream_id(stream_id)?;
    let scenario = lookup(key)?;
    scenario.spec().stream(suffix).map(|stream| stream.decoder)
}

pub struct Scenario {
    key: String,
    spec: MediaSpec,
    sync_points: Mutex<HashMap<String, Arc<SyncPoint>>>,
    handles: Mutex<HashMap<String, Arc<dyn Any + Send + Sync>>>,
}

impl Scenario {
    fn new(key: String, spec: MediaSpec) -> Self {
        Self {
            key,
            spec,
            sync_points: Mutex::new(HashMap::new()),
            handles: Mutex::new(HashMap::new()),
        }
    }

    pub fn key(&self) -> &str {
        &self.key
    }

    pub fn uri(&self) -> String {
        caps::uri_for_key(&self.key)
    }

    pub fn spec(&self) -> &MediaSpec {
        &self.spec
    }

    pub fn stream(&self, id: &str) -> Option<&StreamSpec> {
        self.spec.stream(id)
    }

    /// Full stream-id for one of this scenario's streams.
    pub fn stream_id(&self, suffix: &str) -> String {
        caps::stream_id(&self.key, suffix)
    }

    /// Returns the named gate, creating it on first use.
    pub fn sync_point(&self, name: &str) -> Arc<SyncPoint> {
        self.sync_points
            .lock()
            .entry(name.to_owned())
            .or_insert_with(|| Arc::new(SyncPoint::new(name.to_owned())))
            .clone()
    }

    /// Releases every gate created so far. Teardown safety net.
    pub fn release_all_sync_points(&self) {
        for sync_point in self.sync_points.lock().values() {
            sync_point.release();
        }
    }

    /// Stashes an element-side handle (a sink recording) so the test can reach
    /// it without fcasttest depending on the element's type here.
    pub fn set_handle<T: Any + Send + Sync>(&self, name: &str, handle: Arc<T>) {
        self.handles.lock().insert(name.to_owned(), handle);
    }

    pub fn handle<T: Any + Send + Sync>(&self, name: &str) -> Option<Arc<T>> {
        let handle = self.handles.lock().get(name).cloned()?;
        handle.downcast::<T>().ok()
    }

    pub fn handle_names(&self) -> Vec<String> {
        self.handles.lock().keys().cloned().collect()
    }
}

/// One-shot named gate. A stalled push blocks on it from the streaming thread
/// and the scenario runner releases it by name.
pub struct SyncPoint {
    name: String,
    state: Mutex<SyncState>,
    cv: Condvar,
}

#[derive(Default)]
struct SyncState {
    released: bool,
    /// Threads that reached the gate, whether or not they blocked.
    arrivals: u64,
    waiting: usize,
}

impl SyncPoint {
    fn new(name: String) -> Self {
        Self {
            name,
            state: Mutex::new(SyncState::default()),
            cv: Condvar::new(),
        }
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    /// Blocks until released. Returns immediately once released.
    pub fn wait(&self) {
        let mut state = self.state.lock();
        state.arrivals += 1;
        self.cv.notify_all();
        if state.released {
            return;
        }
        state.waiting += 1;
        while !state.released {
            self.cv.wait(&mut state);
        }
        state.waiting -= 1;
    }

    /// Blocks until released or the timeout expires. Returns whether it was
    /// released.
    pub fn wait_timeout(&self, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock();
        state.arrivals += 1;
        self.cv.notify_all();
        if state.released {
            return true;
        }
        state.waiting += 1;
        while !state.released {
            if self.cv.wait_until(&mut state, deadline).timed_out() {
                break;
            }
        }
        state.waiting -= 1;
        state.released
    }

    /// Idempotent.
    pub fn release(&self) {
        let mut state = self.state.lock();
        if state.released {
            return;
        }
        state.released = true;
        self.cv.notify_all();
    }

    pub fn is_released(&self) -> bool {
        self.state.lock().released
    }

    pub fn arrivals(&self) -> u64 {
        self.state.lock().arrivals
    }

    pub fn waiting(&self) -> usize {
        self.state.lock().waiting
    }

    /// Blocks until at least one thread has reached the gate. Lets a test know
    /// a push is parked before it drives the next action.
    ///
    /// Arrivals are cumulative and never reset, so this is satisfied forever
    /// once anything hit the gate. Waiting for a restarted schedule's
    /// second park needs [`wait_for_arrivals`](Self::wait_for_arrivals)
    /// with the count.
    pub fn wait_for_arrival(&self, timeout: Duration) -> bool {
        self.wait_for_arrivals(1, timeout)
    }

    /// Blocks until `count` threads have reached the gate in total. The
    /// counted form of [`wait_for_arrival`](Self::wait_for_arrival), so a
    /// test can anchor on the second park of a restarted schedule.
    pub fn wait_for_arrivals(&self, count: u64, timeout: Duration) -> bool {
        let deadline = Instant::now() + timeout;
        let mut state = self.state.lock();
        while state.arrivals < count {
            if self.cv.wait_until(&mut state, deadline).timed_out() {
                break;
            }
        }
        state.arrivals >= count
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::{Pacing, StreamSpec};
    use std::sync::atomic::{AtomicBool, Ordering};

    #[test]
    fn register_lookup_unregister() {
        let spec = MediaSpec::new(7)
            .with_stream(StreamSpec::video("video_0"))
            .with_stream(StreamSpec::audio("audio_0").with_pacing(Pacing::Realtime));
        let scenario = register_scenario("regtest", spec);

        assert_eq!(scenario.uri(), "ftest://regtest");
        let found = lookup("regtest").expect("registered scenario");
        assert_eq!(found.spec().seed, 7);
        assert_eq!(found.spec().streams.len(), 2);
        assert_eq!(
            found.stream("audio_0").map(|s| s.pacing),
            Some(Pacing::Realtime)
        );

        let stream_id = found.stream_id("video_0");
        assert_eq!(stream_id, "ftest-regtest-video_0");
        assert!(decoder_knobs_for_stream_id(&format!("abc/{stream_id}")).is_some());
        assert!(decoder_knobs_for_stream_id("ftest-regtest-missing").is_none());

        let handle = Arc::new(vec![1u32, 2, 3]);
        found.set_handle("recording", handle);
        assert_eq!(
            found.handle::<Vec<u32>>("recording").map(|h| h.len()),
            Some(3)
        );
        assert!(found.handle::<String>("recording").is_none());

        unregister("regtest");
        assert!(lookup("regtest").is_none());
    }

    #[test]
    fn sync_point_release_unblocks_waiter() {
        let scenario = register_scenario("synctest", MediaSpec::new(1));
        let gate = scenario.sync_point("stall");
        assert!(Arc::ptr_eq(&gate, &scenario.sync_point("stall")));
        assert!(!gate.is_released());

        let passed = Arc::new(AtomicBool::new(false));
        let waiter = {
            let gate = gate.clone();
            let passed = passed.clone();
            std::thread::spawn(move || {
                gate.wait();
                passed.store(true, Ordering::SeqCst);
            })
        };

        assert!(gate.wait_for_arrival(Duration::from_secs(5)));
        assert!(!passed.load(Ordering::SeqCst));

        gate.release();
        gate.release();
        waiter.join().expect("waiter thread");
        assert!(passed.load(Ordering::SeqCst));
        assert!(gate.is_released());

        // Already released, so a later arrival passes straight through.
        assert!(gate.wait_timeout(Duration::from_millis(0)));
        assert_eq!(gate.arrivals(), 2);
        assert_eq!(gate.waiting(), 0);

        unregister("synctest");
    }
}
