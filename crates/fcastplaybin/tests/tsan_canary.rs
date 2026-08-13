//! Verifies the ThreadSanitizer gate measures anything at all.
//!
//! A green TSan run only means something if the instrument reports races it
//! should report and stays silent about synchronization it should
//! understand. The crate's synchronization is `parking_lot`, a word-lock
//! over raw atomics and a futex, so whether TSan sees a happens-before edge
//! through it is an empirical question.
//!
//! * [`tsan_canary_positive`] is a deliberate unsynchronized write pair.
//!   TSan must report it, or the gate is not instrumenting the crate's Rust
//!   code and every green run is vacuous.
//! * [`tsan_canary_negative`] is the same pair through a
//!   `parking_lot::Mutex`. TSan must stay silent, or the gate would flag
//!   every correctly-synchronized access in the crate.
//!
//! Both are `#[ignore]`d because the positive one is deliberate undefined
//! behaviour. `tools/run-tsan.sh` runs this target first, with `--ignored`,
//! and refuses to report on the fuzz drivers unless both canaries behave.
//! Re-run after any toolchain bump.

use std::{cell::UnsafeCell, thread};

use parking_lot::Mutex;

/// How many writes each side performs. Kept small, one racing pair is
/// enough for TSan.
const WRITES: u64 = 1_000;

/// A `u64` two threads may write at once, with no synchronization. The
/// `Sync` promise is a deliberate lie, which is what the positive canary
/// asks TSan to catch.
struct Racy(UnsafeCell<u64>);

// SAFETY: it is not safe. This type exists to produce exactly the data race
// the sanitizer must see, in a test that only runs under tools/run-tsan.sh.
unsafe impl Sync for Racy {}

impl Racy {
    /// One unsynchronized read-modify-write. Volatile so no optimizer can
    /// fold the pair away. A method rather than an inline expression so the
    /// closure captures the whole `Racy`, since disjoint capture of the
    /// `UnsafeCell` field would not be `Send`.
    fn bump(&self) {
        // SAFETY: unsound on purpose. The unsynchronized pair is the
        // measurement.
        unsafe {
            let slot = self.0.get();
            std::ptr::write_volatile(slot, std::ptr::read_volatile(slot) + 1);
        }
    }

    /// SAFETY: only after every writer has been joined.
    unsafe fn read(&self) -> u64 {
        unsafe { std::ptr::read_volatile(self.0.get()) }
    }
}

#[test]
#[ignore = "a deliberate data race; run through tools/run-tsan.sh"]
fn tsan_canary_positive() {
    let racy = Racy(UnsafeCell::new(0));
    thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                for _ in 0..WRITES {
                    racy.bump();
                }
            });
        }
    });
    // Read it back so nothing above can be optimized away as dead. The value
    // is meaningless, only TSan's verdict matters.
    // SAFETY: every writer has been joined by `thread::scope`.
    let total = unsafe { racy.read() };
    println!("tsan canary (positive): total {total} (expected: a TSan data-race report)");
}

#[test]
#[ignore = "the instrument's negative control; run through tools/run-tsan.sh"]
fn tsan_canary_negative() {
    // parking_lot, not std::sync, because the crate's own mutexes are
    // parking_lot's.
    let guarded = Mutex::new(0u64);
    thread::scope(|scope| {
        for _ in 0..2 {
            scope.spawn(|| {
                for _ in 0..WRITES {
                    *guarded.lock() += 1;
                }
            });
        }
    });
    let total = *guarded.lock();
    assert_eq!(total, 2 * WRITES, "the guarded writes must all land");
    println!("tsan canary (negative): total {total} (expected: TSan silence)");
}
