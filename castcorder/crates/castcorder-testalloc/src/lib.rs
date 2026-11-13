//! A counting global allocator for tests that assert a memory bound.
//!
//! In a test binary:
//!
//! ```ignore
//! #[global_allocator]
//! static ALLOC: castcorder_testalloc::CountingAlloc = castcorder_testalloc::CountingAlloc::new();
//! ```
//!
//! then call [`CountingAlloc::reset_peak`] before the work and
//! [`CountingAlloc::peak`] after. The counts are process-wide, so tests that
//! use them must not run concurrently with other allocating tests (take a
//! mutex, or keep them in a binary of their own).

use std::alloc::{GlobalAlloc, Layout, System};
use std::sync::atomic::{AtomicUsize, Ordering::Relaxed};

/// Forwards to the system allocator and tracks live and peak heap bytes.
#[derive(Debug)]
pub struct CountingAlloc {
    live: AtomicUsize,
    peak: AtomicUsize,
}

impl CountingAlloc {
    /// A fresh counter (for the `static`).
    pub const fn new() -> Self {
        Self {
            live: AtomicUsize::new(0),
            peak: AtomicUsize::new(0),
        }
    }

    /// Bytes allocated and not yet freed.
    pub fn live(&self) -> usize {
        self.live.load(Relaxed)
    }

    /// The highest `live` since the last [`reset_peak`](Self::reset_peak).
    pub fn peak(&self) -> usize {
        self.peak.load(Relaxed)
    }

    /// Start measuring from the current live size.
    pub fn reset_peak(&self) {
        self.peak.store(self.live(), Relaxed);
    }

    fn grow(&self, n: usize) {
        let now = self.live.fetch_add(n, Relaxed) + n;
        self.peak.fetch_max(now, Relaxed);
    }
}

impl Default for CountingAlloc {
    fn default() -> Self {
        Self::new()
    }
}

// SAFETY: every call forwards unchanged to `System`, which upholds the
// `GlobalAlloc` contract; the counters are only bookkeeping.
unsafe impl GlobalAlloc for CountingAlloc {
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc(layout) };
        if !p.is_null() {
            self.grow(layout.size());
        }
        p
    }

    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let p = unsafe { System.alloc_zeroed(layout) };
        if !p.is_null() {
            self.grow(layout.size());
        }
        p
    }

    unsafe fn dealloc(&self, ptr: *mut u8, layout: Layout) {
        unsafe { System.dealloc(ptr, layout) };
        self.live.fetch_sub(layout.size(), Relaxed);
    }

    unsafe fn realloc(&self, ptr: *mut u8, layout: Layout, new_size: usize) -> *mut u8 {
        let p = unsafe { System.realloc(ptr, layout, new_size) };
        if !p.is_null() {
            if new_size >= layout.size() {
                self.grow(new_size - layout.size());
            } else {
                self.live.fetch_sub(layout.size() - new_size, Relaxed);
            }
        }
        p
    }
}
