//! Deterministic test elements for fcastplaybin integration tests.
//!
//! Only the pipeline edges are fake. urisourcebin, parsebin, decodebin3 and the
//! subtitle overlay stay on their field code paths.

pub mod caps;
pub mod dec;
pub mod dvb;
pub mod parse;
pub mod pgs;
pub mod prng;
pub mod registry;
pub mod scenario;
pub mod sink;
pub mod spec;
pub mod src_bin;
pub mod vobsub;

use std::sync::Once;

static REGISTER: Once = Once::new();

/// Registers every fcasttest element into the process registry. Idempotent.
pub fn register_for_tests() {
    REGISTER.call_once(|| {
        gst::init().expect("fcasttest: gst::init() failed");
        src_bin::register().expect("fcasttest: failed to register the ftestsrc elements");
        parse::register().expect("fcasttest: failed to register the ftestparse elements");
        dec::register().expect("fcasttest: failed to register the ftestdec elements");
        sink::register().expect("fcasttest: failed to register the ftestsink elements");
    });
}
