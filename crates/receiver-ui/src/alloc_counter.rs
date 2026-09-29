// SPDX-FileCopyrightText: 2026 FUTO
// SPDX-License-Identifier: MIT

//! Counts heap allocations on the calling thread, so a test can put a number
//! on what one frame costs. Test builds only, and per thread so a parallel
//! harness cannot pollute a measurement.
//!
//! Its own module because more than one lane's tests measure with it, and a
//! global allocator can only be installed once in a crate.

use std::{
    alloc::{GlobalAlloc, Layout, System},
    cell::Cell,
};

// const init and no destructor, so reading it never allocates itself
thread_local! {
    static COUNT: Cell<u64> = const { Cell::new(0) };
}

pub struct Counting;

unsafe impl GlobalAlloc for Counting {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        COUNT.with(|c| c.set(c.get() + 1));
        unsafe { System.alloc(layout) }
    }
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        COUNT.with(|c| c.set(c.get() + 1));
        unsafe { System.alloc_zeroed(layout) }
    }
    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new: usize) -> *mut u8 {
        COUNT.with(|c| c.set(c.get() + 1));
        unsafe { System.realloc(ptr, layout, new) }
    }
    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) }
    }
}

/// Allocations this thread has made so far.
pub fn count() -> u64 {
    COUNT.with(|c| c.get())
}

/// Allocations `f` makes, which is the measurement every steady-state
/// test below is written against.
pub fn measure<T>(f: impl FnOnce() -> T) -> (T, u64) {
    let before = count();
    let out = f();
    let after = count();
    (out, after - before)
}

#[cfg(test)]
#[global_allocator]
static COUNTING_ALLOC: Counting = Counting;
