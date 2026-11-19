use core::alloc::{GlobalAlloc, Layout};
use core::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

static ON: AtomicBool = AtomicBool::new(false);
static LIVE: AtomicUsize = AtomicUsize::new(0);
static PEAK: AtomicUsize = AtomicUsize::new(0);

pub struct Counted<A>(pub A);

#[derive(Clone, Copy, Debug, Default)]
pub struct Count {
    pub live: usize,
    pub peak: usize,
}

impl<A> Counted<A> {
    pub fn start() {
        ON.store(true, Ordering::Release);
    }

    pub fn count() -> Count {
        Count {
            live: LIVE.load(Ordering::Acquire),
            peak: PEAK.load(Ordering::Acquire),
        }
    }

    pub fn reset_peak() -> usize {
        let live = LIVE.load(Ordering::Acquire);
        PEAK.store(live, Ordering::Release);
        live
    }
}

fn grew(bytes: usize) {
    let live = LIVE.fetch_add(bytes, Ordering::AcqRel) + bytes;
    PEAK.fetch_max(live, Ordering::AcqRel);
}

fn shrank(bytes: usize) {
    LIVE.fetch_sub(bytes, Ordering::AcqRel);
}

unsafe impl<A: GlobalAlloc> GlobalAlloc for Counted<A> {
    #[inline]
    unsafe fn alloc(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { self.0.alloc(layout) };
        if !pointer.is_null() && ON.load(Ordering::Relaxed) {
            grew(layout.size());
        }
        pointer
    }

    #[inline]
    unsafe fn alloc_zeroed(&self, layout: Layout) -> *mut u8 {
        let pointer = unsafe { self.0.alloc_zeroed(layout) };
        if !pointer.is_null() && ON.load(Ordering::Relaxed) {
            grew(layout.size());
        }
        pointer
    }

    #[inline]
    unsafe fn dealloc(&self, pointer: *mut u8, layout: Layout) {
        unsafe { self.0.dealloc(pointer, layout) };
        if ON.load(Ordering::Relaxed) {
            shrank(layout.size());
        }
    }

    #[inline]
    unsafe fn realloc(&self, pointer: *mut u8, layout: Layout, bytes_new: usize) -> *mut u8 {
        let moved = unsafe { self.0.realloc(pointer, layout, bytes_new) };
        if !moved.is_null() && ON.load(Ordering::Relaxed) {
            if bytes_new >= layout.size() {
                grew(bytes_new - layout.size());
            } else {
                shrank(layout.size() - bytes_new);
            }
        }
        moved
    }
}
