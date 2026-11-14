//! Quiescent-state-based reclamation (QSBR) for lock-free caches.
//!
//! Objects published to lock-free caches (type method cache, type
//! specialization caches) are read via borrowed pointers plus try-incref.
//! Their memory must stay mapped until every thread that could hold such a
//! borrowed pointer has passed a quiescent state. Destructors run at the
//! normal drop point; only the final deallocation is deferred.
//!
//! Mirrors _Py_qsbr (Python/qsbr.c): a global write sequence advances on
//! each retirement; each thread records the last sequence it observed at a
//! quiescent point (eval-breaker checkpoint, attach/detach). A retired
//! allocation is freed once every online thread's sequence passes its goal.

use core::alloc::Layout;

/// Defer (threading) or immediately perform (non-threading) deallocation
/// of a dead published object's memory.
///
/// # Safety
/// Same contract as [`Qsbr::free_delayed`].
#[inline]
pub(crate) unsafe fn free_delayed(ptr: *mut u8, layout: Layout) {
    // No concurrent readers can exist without threads.
    unsafe {
        alloc::alloc::dealloc(ptr, layout)
    };
}
