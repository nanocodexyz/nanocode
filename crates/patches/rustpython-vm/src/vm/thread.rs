
use crate::frame::InterpreterFrame;
use crate::{AsObject, PyObject, VirtualMachine};
use core::{
    cell::{Cell, RefCell},
    ptr::NonNull,
    sync::atomic::{AtomicUsize, Ordering},
};
use rustpython_common::wtf8::iteration::Itertools;
use std::thread_local;

/// Coalesced per-thread frame-publishing state, touched on every
/// `enter_iframe`/`exit_iframe`. Bundling `current_frame` together with
/// the cached `top_frame`/`top_iframe` slot pointers means
/// `set_current_frame` needs a single `thread_local!.with()` call
/// (one `_tlv_get_addr` on macOS) instead of two or three separate
/// ones — each `.with()` on a distinct `thread_local!` is its own TLS
/// lookup even though the bodies are just a cached-pointer store.
struct FrameSlotCache {
    /// Current top frame for signal-safe traceback walking.
    /// Stores a `*const InterpreterFrame` as `usize`.
    /// Read by faulthandler's signal handler to dump tracebacks without
    /// accessing RefCell or locks. Uses AtomicUsize for async-signal-safety.
    current_frame: AtomicUsize,
}

thread_local! {
    pub(super) static VM_STACK: RefCell<Vec<NonNull<VirtualMachine>>> = Vec::with_capacity(1).into();

    pub(crate) static COROUTINE_ORIGIN_TRACKING_DEPTH: Cell<u32> = const { Cell::new(0) };

    pub(crate) static FRAME_SLOT_CACHE: FrameSlotCache = const {
        FrameSlotCache {
            current_frame: AtomicUsize::new(0),
        }
    };

}

#[must_use]
pub fn current_vm_is_set() -> bool {
    VM_STACK.with(|vms| !vms.borrow().is_empty())
}

pub fn with_current_vm<R>(f: impl FnOnce(&VirtualMachine) -> R) -> R {
    VM_STACK.with(|vms| {
        let vm = vms
            .borrow()
            .last()
            .copied()
            .expect("call with_current_vm() but no current VM is attached");
        // SAFETY: entries in VM_STACK either borrow a VM for the dynamic
        // scope of a set_current_vm()/enter_vm() call or point at GILSTATE_VM.
        f(unsafe { vm.as_ref() })
    })
}

fn set_current_vm<R>(vm: &VirtualMachine, f: impl FnOnce() -> R) -> R {
    // Attach to this VM's interpreter, detaching the enclosing one if this is a
    // switch between interpreters on the same OS thread.

    VM_STACK.with(|vms| {
        vms.borrow_mut().push(vm.into());
        rustpython_common::defer! {
            vms.borrow_mut().pop();
        }
        f()
    })
}

/// Pointer to the GC state of the interpreter running on this thread.
///
/// The pointee belongs to the `PyGlobalState` of the VM on top of `VM_STACK`,
/// which is borrowed for the whole `set_current_vm` scope — so the pointer stays
/// valid as long as the caller remains inside that scope.
pub(crate) fn current_gc_state() -> Option<NonNull<crate::gc_state::GcInterpreterState>> {
    // Reached from every tracked allocation, including ones a thread-local
    // destructor makes while the VM stack is being torn down, so neither a
    // destroyed key nor an outstanding borrow may panic here.
    VM_STACK
        .try_with(|vms| {
            let vm = vms.try_borrow().ok()?.last().copied()?;
            // SAFETY: entries in VM_STACK either borrow a VM for the dynamic
            // scope of a set_current_vm()/enter_vm() call or point at GILSTATE_VM.
            Some(NonNull::from(&unsafe { vm.as_ref() }.state.gc))
        })
        .ok()
        .flatten()
}

pub fn try_with_current_vm<R>(f: impl FnOnce(&VirtualMachine) -> R) -> Option<R> {
    VM_STACK.with(|vms| {
        let vm = vms.borrow().last().copied()?;
        // SAFETY: entries in VM_STACK either borrow a VM for the dynamic
        // scope of a set_current_vm()/enter_vm() call or point at GILSTATE_VM.
        Some(f(unsafe { vm.as_ref() }))
    })
}

pub fn enter_vm<R>(vm: &VirtualMachine, f: impl FnOnce() -> R) -> R {
    // Attach/detach is handled by `set_current_vm`, which pairs it with the
    // VM_STACK push so that switching interpreters mid-stack stays consistent.
    set_current_vm(vm, f)
}

/// RAII counterpart to `enter_vm`, for code that runs Python bytecode across
/// several statements interspersed with `&mut VirtualMachine` calls
/// (`VirtualMachine::initialize`), where a single closure-based `enter_vm`
/// scope cannot be expressed because the borrow checker won't let a closure
/// hold `&mut VirtualMachine` at the same time `enter_vm` reborrows it as
/// `&VirtualMachine`. Construction only needs a transient `&VirtualMachine`
/// borrow, so it can be dropped before subsequent `&mut` use.
///
/// Without this, code that runs Python bytecode before any `enter_vm` scope
/// exists would leave the thread not ATTACHED, making lock-free type cache
/// reads unsound.
#[must_use]
pub(crate) struct VmBootstrapGuard {
}

impl VmBootstrapGuard {
    pub(crate) fn new(vm: &VirtualMachine) -> Self {
        VM_STACK.with(|vms| vms.borrow_mut().push(vm.into()));

        Self {
        }
    }
}

impl Drop for VmBootstrapGuard {
    fn drop(&mut self) {
        VM_STACK.with(|vms| {
            vms.borrow_mut().pop();
        });

    }
}

/// No-op on non-threading builds.
pub fn allow_threads<R>(_vm: &VirtualMachine, f: impl FnOnce() -> R) -> R {
    f()
}

/// No-op on non-threading builds.
pub fn attach_for_callback<R>(_vm: &VirtualMachine, f: impl FnOnce() -> R) -> R {
    f()
}

/// Set the current thread's top InterpreterFrame pointer.
/// Returns the previous pointer so it can be restored on pop.
#[must_use]
#[allow(clippy::not_unsafe_ptr_arg_deref)]
pub fn set_current_frame(frame: *const InterpreterFrame) -> *const InterpreterFrame {
    FRAME_SLOT_CACHE.with(|cache| {
        // Publish the top frame for cross-thread readers (faulthandler,
        // sys._current_frames).
        cache.current_frame.swap(frame as usize, Ordering::Relaxed)
    }) as *const InterpreterFrame
}

/// Lightweight version that only writes to TLS `current_frame`, returning
/// the previous value. Does not update cross-thread top_frame (that's
/// updated by `set_current_frame` for FrameObject-based calls).
#[inline(always)]
#[must_use]
pub fn set_current_frame_nosave(frame: *const InterpreterFrame) -> *const InterpreterFrame {
    FRAME_SLOT_CACHE.with(|cache| cache.current_frame.swap(frame as usize, Ordering::Relaxed))
        as *const InterpreterFrame
}

/// Get the current thread's top InterpreterFrame pointer.
/// Used by faulthandler's signal handler to start traceback walking.
#[must_use]
pub fn get_current_frame() -> *const InterpreterFrame {
    FRAME_SLOT_CACHE.with(|cache| cache.current_frame.load(Ordering::Relaxed))
        as *const InterpreterFrame
}

fn top_slot_is_attached() -> bool {
    true
}

/// Which VM `with_vm` found for `obj`, and whether this thread is already
/// attached to it (see the fast path below).
enum WithVmTarget {
    /// `interp` is on top of `VM_STACK`, so this thread is already ATTACHED
    /// to it (see [`begin_interpreter_section`]'s invariant): no section
    /// switch is needed.
    AlreadyCurrent(NonNull<VirtualMachine>),
    /// `interp` owns `obj` but is not the top of `VM_STACK` (a nested,
    /// currently-detached interpreter), so a real attach/detach section is
    /// required.
    NeedsSwitch(NonNull<VirtualMachine>),
}

pub fn with_vm<F, R>(obj: &PyObject, f: F) -> Option<R>
where
    F: Fn(&VirtualMachine) -> R,
{
    let vm_owns_obj = |interp: NonNull<VirtualMachine>| {
        // SAFETY: all references in VM_STACK should be valid
        let vm = unsafe { interp.as_ref() };
        obj.fast_isinstance(vm.ctx.types.object_type)
    };
    // `with_vm` runs on every teardown of an object with a `__del__` slot or a
    // weakref callback (drop_slow_inner / try_call_finalizer / gc_state), which
    // for `__del__` objects and weakrefs collected during a GC pass means it
    // runs on essentially every such drop. The overwhelming majority of those
    // drops happen from within Python bytecode executing on this very thread,
    // i.e. `obj`'s owning interpreter is already the one on top of `VM_STACK`.
    // `begin_interpreter_section` (via `set_current_vm`) only ever needs to run
    // for the rare case where the object belongs to a *different* interpreter
    // than the one currently attached (a nested subinterpreter scenario) or no
    // interpreter is attached at all (object dropped on a thread outside any
    // VM, e.g. during shutdown or from a plain Rust thread) — in the fast case
    // we can skip it entirely and call `f` directly.
    let target = VM_STACK.with(|vms| {
        let vms = vms.borrow();
        // Fast path: at most one interpreter is ever ATTACHED per OS thread,
        // and it is always the one on top of `VM_STACK` — every push onto
        // VM_STACK (set_current_vm, VmBootstrapGuard) is paired with an attach
        // *before* the push, and every pop is paired with a detach (or a
        // re-attach of the newly-exposed top) in `end_interpreter_section`.
        // So if `obj`'s owning interpreter is the current top, this thread is
        // already attached to it and there is nothing for
        // `begin_interpreter_section` to do: no INTERP_THREAD_SLOTS lookup, no
        // Arc clone, no atomic state transition.
        // The top may still be DETACHED while the thread sits inside an
        // `allow_threads` section (a blocking call that dropped an object
        // with `__del__`); running `f` there would execute Python on a thread
        // a stop-the-world requester counts as parked, so that case takes the
        // full attach path below.
        if let Some(top) = vms.last().copied()
            && vm_owns_obj(top)
            && top_slot_is_attached()
        {
            return Some(WithVmTarget::AlreadyCurrent(top));
        }
        let interp = match vms.iter().copied().exactly_one() {
            Ok(x) => {
                debug_assert!(vm_owns_obj(x));
                x
            }
            Err(mut others) => others.find(|x| vm_owns_obj(*x))?,
        };
        Some(WithVmTarget::NeedsSwitch(interp))
    })?;
    match target {
        WithVmTarget::AlreadyCurrent(interp) => {
            // SAFETY: `interp` is (or was, at the point it was read above) the
            // top of VM_STACK for this thread, so it is valid for at least the
            // dynamic scope of the enclosing set_current_vm()/enter_vm() call,
            // which contains this whole function call.
            let vm = unsafe { interp.as_ref() };
            Some(f(vm))
        }
        WithVmTarget::NeedsSwitch(interp) => {
            // SAFETY: all references in VM_STACK should be valid, and should not be changed or moved
            // at least until this function returns and the stack unwinds to an enter_vm() call
            let vm = unsafe { interp.as_ref() };
            Some(set_current_vm(vm, || f(vm)))
        }
    }
}

impl VirtualMachine {
}
