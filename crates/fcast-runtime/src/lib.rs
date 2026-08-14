//! The single shared tokio runtime for the receiver and its GStreamer elements.
//!
//! Declaring the runtime in one small crate that everything depends on means
//! all FCast crates spawn their async work on the same thread pool, rather than
//! each standing up its own.

use std::sync::LazyLock;

/// The process-wide async runtime shared by every FCast crate.
pub static RUNTIME: LazyLock<tokio::runtime::Runtime> = LazyLock::new(|| {
    tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .worker_threads(num_cpus::get().min(4))
        .thread_name("main-async-worker")
        .build()
        .unwrap()
});
