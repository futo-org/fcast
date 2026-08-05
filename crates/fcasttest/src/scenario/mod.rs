//! Scenario layer: describe media, register it, anchor test actions to pipeline
//! events, and sweep the recorded sinks for sequence violations.
//!
//! The runner is deliberately absent. fcasttest is a dev-dependency OF
//! fcastplaybin, so nothing here may name it. Tests own the playbin harness (see
//! `crates/fcastplaybin/tests/scenarios.rs`) and use this module for everything
//! that is pipeline-agnostic.
//!
//! ```no_run
//! use fcasttest::{scenario::ScenarioBuilder, spec::StreamSpec};
//!
//! fcasttest::register_for_tests();
//! let scenario = ScenarioBuilder::new("mykey")
//!     .video("video_0")
//!     .audio("audio_0")
//!     .duration(gst::ClockTime::from_mseconds(1200))
//!     .register();
//! let uri = scenario.uri(); // ftest://mykey
//! # let _ = (uri, StreamSpec::video("v"));
//! ```

mod builder;
mod invariants;
mod timeline;
pub mod toml;

pub use builder::{ScenarioBuilder, ScenarioHandle, stable_seed};
pub use invariants::{check_all, check_all_named, wait_quiescent};
pub use timeline::{DEFAULT_TIMEOUT, Timeline, TimelineError};
