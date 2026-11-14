//! Native frames (0063, ARCH "Generated state machines", D1631): `@native` builtins run
//! as Rust state machines that djs-bgen generates into `builtins/gen/natives.rs`.
//!
//! A native frame is an ordinary frame record (frame.rs) with `FLAG_NATIVE` in its
//! header: callee (the builtin's Function object), `this`, the arguments padded to the
//! parameter count, the header, then the generated slots (locals and temporaries, all
//! Values) and a small operand area for outgoing calls. `pc` is the state. Because the
//! frame holds only Values, the GC, snapshots, stack traces and suspension see nothing
//! new. `load_frame` sets `code_len` to 0 for native frames, so `fast_step` declines
//! them and `step` calls [`Exec::native_step`].
//!
//! The rules the generated code follows: no collection inside a state arm (allocation
//! never collects), so Rust locals holding Values never cross a state boundary; every
//! intrinsic call goes through `call_intrinsic` on the operand area (semantics shared
//! with the bytecode backend), except for a few inline fast paths here that mirror it;
//! a reentrant call either completes inline (the generated code continues at the
//! resume state in the same Rust loop) or pushes a frame and returns to the run loop,
//! which re-enters the state machine at the resume state with the result on the
//! operand stack. Loop back-edges charge fuel and are safepoints.
use crate::PushOut;

use super::*;
use crate::builtins::r#gen::natives::NATIVES;

/// Operand slots a native frame reserves above its locals (outgoing calls `ensure`
/// more when they need it).
const NATIVE_STACK: u32 = 8;
/// Fuel per loop back-edge and per call from native code (bytecode charges per
/// instruction; a native loop iteration does the work of a few dozen of them).
const BACKEDGE_FUEL: i64 = 4;
pub(super) const CALL_FUEL: i64 = 2;

/// A generated native function (`builtins/gen/natives.rs`).
pub(crate) struct NativeFn {
    /// The builtin's function name in the prelude (checked by `%MarkNative`; wasm
    /// builds without the prelude never read it, D3094).
    #[cfg_attr(not(djs_prelude), allow(dead_code))]
    pub name: &'static str,
    /// Formal parameters (the argument area is padded to it).
    pub params: u32,
    /// Generated slots.
    pub locals: u32,
    pub run: fn(&mut Exec<'_>) -> Step,
    /// The handler of a state inside a `try`: (catch state, slot for the exception).
    pub handler: fn(u32) -> Option<(u32, u32)>,
}

/// How a reentrant call from native code went.
pub(crate) enum Flow {
    /// It completed: the result is on the operand stack and `pc` is the resume state.
    Inline,
    /// A frame was pushed (or the VM parked): return this to the run loop.
    Leave(Option<Status>),
}

/// The id of intrinsic `name`, at compile time.
pub(crate) const fn iid(name: &str) -> u16 {
    super::intrinsics::id(name)
}

// ---- value conversions for typed locals -------------------------------------------------
// (The API of generated code: the current set of natives need not use all of it.)

#[allow(dead_code)]
#[cfg_attr(target_arch = "wasm32", inline(never))]
#[cfg_attr(not(target_arch = "wasm32"), inline(always))]
pub(crate) fn num(n: f64) -> Value {
    conv::number(n)
}

#[allow(dead_code)]
#[cfg_attr(target_arch = "wasm32", inline(never))]
#[cfg_attr(not(target_arch = "wasm32"), inline(always))]
pub(crate) fn f64_of(v: Value) -> f64 {
    v.as_number().unwrap_or(f64::NAN)
}

#[allow(dead_code)]
#[cfg_attr(target_arch = "wasm32", inline(never))]
#[cfg_attr(not(target_arch = "wasm32"), inline(always))]
pub(crate) fn i32_of(v: Value) -> i32 {
    match v.as_int() {
        Some(i) => i,
        None => f64_of(v) as i32,
    }
}

#[allow(dead_code)]
#[cfg_attr(target_arch = "wasm32", inline(never))]
#[cfg_attr(not(target_arch = "wasm32"), inline(always))]
pub(crate) fn u32_of(v: Value) -> u32 {
    match v.as_int() {
        Some(i) => i as u32,
        None => f64_of(v) as u32,
    }
}

#[allow(dead_code)]
#[cfg_attr(target_arch = "wasm32", inline(never))]
#[cfg_attr(not(target_arch = "wasm32"), inline(always))]
pub(crate) fn u32_value(n: u32) -> Value {
    if n <= i32::MAX as u32 {
        Value::int(n as i32)
    } else {
        Value::number(n as f64)
    }
}

#[allow(dead_code)]
#[cfg_attr(target_arch = "wasm32", inline(never))]
#[cfg_attr(not(target_arch = "wasm32"), inline(always))]
pub(crate) fn bool_of(v: Value) -> bool {
    v.as_bool().unwrap_or(false)
}

#[allow(dead_code)]
#[cold]
fn overflow() -> Fault {
    Fault::error(
        ErrorKind::InternalError,
        String::from("integer overflow in a builtin"),
    )
}

/// A state the generated code does not have (a bug in djs-bgen).
#[cold]
pub(crate) fn bad_state() -> Fault {
    Fault::error(
        ErrorKind::InternalError,
        String::from("bad native builtin state"),
    )
}

/// Checked integer arithmetic (D1171): an overflow is a bug in the builtin.
#[allow(dead_code)]
#[inline]
pub(crate) fn ck<T>(r: Option<T>) -> Result<T, Fault> {
    r.ok_or_else(overflow)
}

/// The index of an integer key that `get_element_fast` can take (as `index_key`).
#[inline]
fn index_of(k: Value) -> Option<u32> {
    if let Some(i) = k.as_int() {
        return u32::try_from(i).ok();
    }
    k.as_number()
        .filter(|&n| (0.0..2147483648.0).contains(&n) && (n as u32) as f64 == n)
        .map(|n| n as u32)
}

#[allow(dead_code)] // the API of generated code: not every part is used by the current set
impl Exec<'_> {
    // ---- frames (called by the interpreter) ---------------------------------------------

    /// Pushes a native frame for template `t` (a `@native` builtin) and enters state 0.
    pub(super) fn push_native_frame(
        &mut self,
        t: u32,
        base: u32,
        argc: u32,
        cont: Continuation,
        ret_pc: u32,
    ) -> Step {
        let idx = self.heap.arena().get(t, FuncTemplate::NATIVE);
        let Some(nf) = NATIVES.get(idx as usize) else {
            return Err(Fault::error(
                ErrorKind::InternalError,
                String::from("bad native builtin"),
            ));
        };
        let n = argc.max(nf.params);
        let hp = base + 2 + n;
        let lp = hp + HEADER;
        self.ensure(lp + nf.locals + NATIVE_STACK)?;
        let bottom = cont.is_bottom();
        let header = [
            Value::int(if bottom { 0 } else { self.bp as i32 }),
            Value::int(if bottom { 0 } else { self.hp as i32 }),
            Value::int(ret_pc as i32),
            Value::int(argc as i32),
            Value::int(cont.to_flags() | frame::FLAG_NATIVE),
            Value::UNDEFINED,
        ];
        let vals = crate::heap::cells::ValueArray::VALUES.at(self.stack, 0);
        super::hot::write_frame(
            self.heap.arena_mut().hot_bytes(),
            vals,
            (base, argc, n, hp, nf.locals),
            header,
        );
        self.bp = base;
        self.hp = hp;
        self.sp = lp + nf.locals;
        self.pc = 0;
        self.enter_template(t);
        self.enter_native(t);
        // As in `push_frame`: the entry call must not collect.
        if cont != Continuation::Entry {
            self.safepoint();
        }
        Ok(None)
    }

    /// Marks the current frame (template `t`, already entered) as native.
    #[inline]
    pub(super) fn enter_native(&mut self, t: u32) {
        self.native = self.heap.arena().get(t, FuncTemplate::NATIVE) as u32 + 1;
        self.code_len = 0;
    }

    #[inline]
    fn native_fn(&self) -> &'static NativeFn {
        &NATIVES[(self.native - 1) as usize]
    }

    pub(super) fn native_locals(&self) -> u32 {
        self.native_fn().locals
    }

    pub(super) fn native_handler(&self, state: u32) -> Option<(u32, u32)> {
        (self.native_fn().handler)(state)
    }

    /// The handler-table key of the current frame for a call that returns to `ret_pc`:
    /// the call instruction in bytecode, the resume state itself in a native frame
    /// (a resume state is in the same `try` region as its call).
    pub(super) fn call_site_key(&self, ret_pc: u32) -> u32 {
        if self.native != 0 {
            ret_pc
        } else {
            ret_pc.saturating_sub(1)
        }
    }

    /// Whether `o` is of class `c`: the class word compared in place (`object_class`
    /// is a call in wasm and checks the range, 0107).
    #[inline(always)]
    pub(super) fn is_class(&self, o: crate::value::Obj, c: ObjectClass) -> bool {
        self.heap
            .arena()
            .get(o.offset(), crate::heap::cells::Object::CLASS)
            == c as u32
    }

    /// Runs the current native frame from its state until it calls, returns, throws or
    /// reaches a back-edge without fuel. Its faults are thrown here, in its own frame.
    #[inline(never)]
    pub(super) fn native_step(&mut self) -> Step {
        debug_assert!(self.n_res.is_none(), "an inline result outlived its run");
        let run = self.native_fn().run;
        match run(self) {
            Ok(st) => Ok(st),
            Err(f) => {
                let pc = self.pc;
                Ok(self.throw_fault(f, pc))
            }
        }
    }

    #[cfg(djs_prelude)]
    /// `%MarkNative(f, index)`: makes the builtin `f` native builtin `index`. The name
    /// must match the generated table, or the prelude fails (at base-image build time).
    pub(super) fn mark_native(&mut self, f: Value, index: Value) -> Result<(), Fault> {
        let bad = |msg: String| Err(Fault::error(ErrorKind::InternalError, msg));
        let Some(o) = f
            .as_object()
            .filter(|&o| self.is_class(o, ObjectClass::Function))
        else {
            return bad(String::from("%MarkNative: not a function"));
        };
        let i = index.as_int().unwrap_or(-1);
        let t = self.template_of(o);
        let name = self
            .heap
            .atom_name(Atom::from_id(self.heap.arena().get(t, FuncTemplate::NAME)));
        match usize::try_from(i).ok().and_then(|i| NATIVES.get(i)) {
            Some(nf) if nf.name == name => {}
            Some(nf) => {
                return bad(crate::text::cat!(
                    "%MarkNative: native ",
                    i,
                    " is ",
                    nf.name,
                    ", not ",
                    name,
                    " (run scripts/bgen.sh)"
                ));
            }
            None => {
                return bad(crate::text::cat!(
                    "%MarkNative: no native ",
                    i,
                    " for ",
                    name,
                    " (run scripts/bgen.sh)"
                ));
            }
        }
        let a = self.heap.arena_mut();
        let flags = u32::from(a.get::<u16>(t, FuncTemplate::FLAGS));
        a.set(t, FuncTemplate::FLAGS, (flags | func_flags::NATIVE) as u16);
        // The table is far below 2^8 entries (a u8 field, D2448; a test checks).
        a.set(t, FuncTemplate::NATIVE, i as u8);
        Ok(())
    }

    // ---- the API of generated code ------------------------------------------------------

    #[inline]
    pub(crate) fn n_state(&self) -> u32 {
        self.pc
    }

    #[inline]
    pub(crate) fn n_goto(&mut self, s: u32) {
        self.pc = s;
    }

    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_slot(&self, i: u32) -> Value {
        self.get(self.lp + i)
    }

    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_set_slot(&mut self, i: u32, v: Value) {
        self.set(self.lp + i, v)
    }

    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_arg(&self, i: u32) -> Value {
        self.get(self.bp + 2 + i)
    }

    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_set_arg(&mut self, i: u32, v: Value) {
        self.set(self.bp + 2 + i, v)
    }

    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_this(&self) -> Value {
        self.get(self.bp + 1)
    }

    pub(crate) fn n_new_target(&self) -> Value {
        self.header(frame::NEW_TARGET)
    }

    /// A rest parameter: the arguments from `index` on, in a new array (as `Op::Rest`).
    pub(crate) fn n_rest(&mut self, index: u32) -> Result<Value, Fault> {
        let argc = self.header_int(frame::ARGC);
        let mut vals = alloc::vec::Vec::new();
        for i in index..argc.max(index) {
            vals.push_out(self.get(self.bp + 2 + i));
        }
        let proto = intrinsic_obj(self.heap, Intrinsic::ArrayProto);
        Ok(Value::object(
            self.heap.new_array_from_values(Some(proto), &vals)?,
        ))
    }

    /// A loop back-edge to state `s`: charges fuel and is a safepoint. True: return
    /// `Ok(None)` to the run loop (out of fuel; it resumes at `s`).
    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_backedge(&mut self, s: u32) -> bool {
        self.fuel -= BACKEDGE_FUEL;
        self.pc = s;
        if self.fuel <= 0 {
            return true;
        }
        self.safepoint();
        false
    }

    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_ret(&mut self, v: Value) -> Step {
        self.do_return(v)
    }

    /// The result of the reentrant call that resumed this state.
    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_result(&mut self) -> Value {
        match self.n_res.take() {
            Some(v) => v,
            None => self.pop(),
        }
    }

    /// A leaf intrinsic through `call_intrinsic`.
    pub(crate) fn n_leaf(&mut self, id: u16, args: &[Value]) -> Result<Value, Fault> {
        let base = self.sp;
        let n = args.len() as u32;
        self.ensure(base + n + 1)?;
        for &a in args {
            self.push(a);
        }
        let (bp, pc) = (self.bp, self.pc);
        let r = self.call_intrinsic(id, n, pc);
        debug_assert!(self.bp == bp, "leaf intrinsic {id} pushed a frame");
        self.pc = pc;
        match r {
            Ok(None) => Ok(self.pop()),
            Ok(Some(_)) => Err(Fault::error(
                ErrorKind::InternalError,
                String::from("leaf intrinsic suspended"),
            )),
            Err(f) => {
                self.sp = base;
                Err(f)
            }
        }
    }

    /// Runs `f(args)`; if it reaches the heap cap, collects with `args` rooted on the
    /// stack and runs it once more with them (D1900, as D1700 retries an instruction).
    /// Only for operations that allocate before they change anything, called where
    /// `args` are the only heap values the generated code holds in Rust locals: a
    /// state's final call, or a call that is a whole statement (djs-bgen marks those).
    fn n_retry<T>(
        &mut self,
        args: &[Value],
        f: impl Fn(&mut Self, &[Value]) -> Result<T, Fault>,
    ) -> Result<T, Fault> {
        // Nested in another retrying operation: that one retries the whole.
        if self.n_retrying {
            return f(self, args);
        }
        self.n_retrying = true;
        let r = f(self, args);
        self.n_retrying = false;
        match r {
            Err(e) if e.is_out_of_memory() && !self.host_oom => {
                let base = self.sp;
                let n = args.len() as u32;
                // Room for the arguments, within the frame's reserved operand stack.
                if base + n > self.heap.value_array_capacity(self.stack) {
                    return Err(e);
                }
                for &a in args {
                    self.push(a);
                }
                self.safepoint_collect();
                let again: alloc::vec::Vec<Value> = (0..n).map(|i| self.get(base + i)).collect();
                self.sp = base;
                if self.host_oom {
                    return Err(e);
                }
                self.n_retrying = true;
                let r = f(self, &again);
                self.n_retrying = false;
                r
            }
            r => r,
        }
    }

    /// `n_leaf` of an intrinsic that allocates before it changes anything, called as a
    /// whole statement: retried once after a collection at the heap cap (D1900).
    pub(crate) fn n_leaf_gc(&mut self, id: u16, args: &[Value]) -> Result<Value, Fault> {
        debug_assert!(super::retry::INTRINSICS.contains(&id), "n_leaf_gc {id}");
        self.n_retry(args, |ex, a| ex.n_leaf(id, a))
    }

    /// `Str + Str` as a whole statement (`r += s`): retried as `n_leaf_gc` is.
    pub(crate) fn n_concat_gc(&mut self, a: Value, b: Value) -> Result<Value, Fault> {
        self.n_retry(&[a, b], |ex, v| ex.n_concat(v[0], v[1]))
    }

    /// A reentrant intrinsic or an `@intrinsic` builtin through `call_intrinsic`,
    /// resuming at state `next`. The intrinsics D1700 retries are retried here too.
    pub(crate) fn n_call(&mut self, id: u16, args: &[Value], next: u32) -> Result<Flow, Fault> {
        // (Off with the hot loop: `hot_loop_is_invisible` compares the two paths.)
        if id == const { iid("Call") }
            && args.len() >= 2
            && self.opts.hot_loop
            && let Some(r) = self.n_call_closure(args, next)
        {
            return r;
        }
        if super::retry::retries(id) {
            return self.n_retry(args, |ex, a| ex.n_call_once(id, a, next));
        }
        self.n_call_once(id, args, next)
    }

    /// `%Call(f, this, ..)` of a plain closure (no FAST/NATIVE/SLOPPY_THIS flag): its
    /// frame is written in place, arguments and all, as `Exec::call` would after the
    /// pushes of `n_call_once` (D2060). `None`: not such a closure, nothing changed.
    fn n_call_closure(&mut self, args: &[Value], next: u32) -> Option<Result<Flow, Fault>> {
        let f = args[0].as_object()?;
        if !self.is_class(f, ObjectClass::Function) {
            return None;
        }
        let t = self.template_of(f);
        let th = super::hot::template_head(self.heap.arena().bytes(), t);
        if th.flags & self.slow_call_flags != 0 {
            return None;
        }
        self.fuel -= CALL_FUEL;
        let base = self.sp;
        let argc = args.len() as u32 - 2;
        let top = base + 2 + argc.max(th.params) + HEADER + th.locals + th.stack;
        // The two checks of `n_call_once` + `Exec::call`, so that the stack grows the same.
        if let Err(e) = self
            .ensure(base + args.len() as u32 + 2)
            .and_then(|_| self.ensure(top))
        {
            return Some(Err(e));
        }
        let vals = crate::heap::cells::ValueArray::VALUES.at(self.stack, 0);
        super::hot::write_values(self.heap.arena_mut().hot_bytes(), vals + 8 * base, args);
        self.push_frame_head(t, &th, base, argc, Continuation::PushResult, next, None);
        Some(Ok(Flow::Leave(None)))
    }

    fn n_call_once(&mut self, id: u16, args: &[Value], next: u32) -> Result<Flow, Fault> {
        self.fuel -= CALL_FUEL;
        let base = self.sp;
        let n = args.len() as u32;
        self.ensure(base + n + 2)?;
        for &a in args {
            self.push(a);
        }
        let bp = self.bp;
        // `%Call(f, this, ..)`, the callback calls, skips the intrinsic dispatch: its
        // arguments are already a call's stack layout (as in `call_intrinsic`).
        let r = if id == const { iid("Call") } && n >= 2 {
            self.call(base, n - 2, super::frame::Continuation::PushResult, next)
        } else {
            self.call_intrinsic(id, n, next)
        };
        match r {
            Ok(Some(st)) => Ok(Flow::Leave(Some(st))),
            Ok(None) if self.bp == bp && self.native != 0 => {
                self.pc = next;
                Ok(Flow::Inline)
            }
            Ok(None) => Ok(Flow::Leave(None)),
            Err(f) => {
                if self.bp == bp {
                    self.sp = base;
                }
                Err(f)
            }
        }
    }

    /// An inline result of a reentrant call (a fast path): as if the call completed.
    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    fn n_inline(&mut self, v: Value, next: u32) -> Result<Flow, Fault> {
        debug_assert!(self.n_res.is_none(), "an inline result was not taken");
        self.n_res = Some(v);
        self.pc = next;
        Ok(Flow::Inline)
    }

    // ---- inline fast paths (each mirrors `call_intrinsic` / the builtin) -----------------

    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_is_callable(&self, v: Value) -> bool {
        self.is_callable(v)
    }

    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_to_boolean(&self, v: Value) -> bool {
        conv::to_boolean(self.heap, v)
    }

    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_strict_eq(&self, a: Value, b: Value) -> bool {
        conv::strict_equals(self.heap, a, b)
    }

    /// `%AsNumber` (as `call_intrinsic`; 0107), as the f64 its callers take.
    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_as_number(&self, v: Value) -> Result<f64, Fault> {
        if let Some(n) = v.as_number() {
            Ok(n)
        } else {
            intrinsics::bad_argument(const { crate::bytecode::intrinsics::const_id("AsNumber") })
        }
    }

    /// `%AsFn` (as `call_intrinsic`; 0107).
    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_as_fn(&self, v: Value) -> Result<Value, Fault> {
        if self.is_callable(v) {
            Ok(v)
        } else {
            intrinsics::bad_argument(const { crate::bytecode::intrinsics::const_id("AsFn") })
        }
    }

    /// `%AsStr` (as `call_intrinsic`; 0107).
    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_as_str(&self, v: Value) -> Result<Value, Fault> {
        if v.as_string().is_some() {
            Ok(v)
        } else {
            intrinsics::bad_argument(const { crate::bytecode::intrinsics::const_id("AsStr") })
        }
    }

    /// `%AsObj`.
    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_as_obj(&self, v: Value) -> Result<Value, Fault> {
        if v.as_object().is_some() {
            Ok(v)
        } else {
            intrinsics::bad_argument(const { crate::bytecode::intrinsics::const_id("AsObj") })
        }
    }

    /// `%ToObject` of an object; anything else through the intrinsic.
    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_to_object(&mut self, v: Value) -> Result<Value, Fault> {
        if v.as_object().is_some() {
            return Ok(v);
        }
        self.n_leaf(const { iid("ToObject") }, &[v])
    }

    /// `%SortStep(st, v)` (as `call_intrinsic`): once per comparison of a sort, so not
    /// through the intrinsic dispatch.
    pub(crate) fn n_sort_step(&mut self, st: Value, v: Value) -> Result<bool, Fault> {
        match (st.as_object(), v.as_number()) {
            (Some(o), Some(v)) => crate::builtins::array::sort_step(self.heap, o, v),
            _ => intrinsics::bad_argument(const { iid("SortStep") }),
        }
    }

    /// `%SortOperand(st, which)` (as `call_intrinsic`).
    pub(crate) fn n_sort_operand(&self, st: Value, which: Value) -> Result<Value, Fault> {
        match (st.as_object(), which.as_number()) {
            (Some(o), Some(w)) => Ok(crate::builtins::array::sort_operand(self.heap, o, w != 0.0)),
            _ => intrinsics::bad_argument(const { iid("SortOperand") }),
        }
    }

    /// `Str + Str`.
    pub(crate) fn n_concat(&mut self, a: Value, b: Value) -> Result<Value, Fault> {
        match (a.as_string(), b.as_string()) {
            (Some(x), Some(y)) => Ok(Value::string(self.heap.concat(x, y)?)),
            _ => intrinsics::bad_argument_named("concat"),
        }
    }

    /// A string literal: the atom's string for well-known names, else a new string.
    pub(crate) fn n_str(&mut self, s: &'static str) -> Result<Value, Fault> {
        use crate::heap::atoms;
        let atom = match s {
            "" => atoms::EMPTY,
            "length" => atoms::LENGTH,
            "constructor" => atoms::CONSTRUCTOR,
            "value" => atoms::VALUE,
            "done" => atoms::DONE,
            "next" => atoms::NEXT,
            "return" => atoms::RETURN,
            "then" => atoms::THEN,
            "string" => atoms::STRING,
            "number" => atoms::NUMBER,
            "default" => atoms::DEFAULT,
            // Property keys of the JSON natives (`GetV(value, "toJSON")`): a new string
            // would be hashed twice per lookup with the keyed hash (D2433).
            "toJSON" => atoms::TO_JSON,
            "rawJSON" => atoms::RAW_JSON,
            _ => return Ok(Value::string(self.heap.alloc_str(s)?)),
        };
        Ok(self.heap.atom_to_value(atom)?)
    }

    /// A string literal with lone surrogates.
    pub(crate) fn n_str16(&mut self, units: &'static [u16]) -> Result<Value, Fault> {
        Ok(Value::string(self.heap.alloc_utf16(units)?))
    }

    /// `%Get(o, k)`: a dense element of an Array inline (as `fast_intrinsic`).
    pub(crate) fn n_get(&mut self, args: &[Value; 2], next: u32) -> Result<Flow, Fault> {
        if let Some(o) = args[0]
            .as_object()
            .filter(|&o| self.is_class(o, ObjectClass::Array))
            && let Some(i) = index_of(args[1])
            && let Some(v) = self.heap.get_element_fast(o, i)
        {
            return self.n_inline(v, next);
        }
        self.n_call(const { iid("Get") }, args, next)
    }

    /// The fused `if (HasProperty(o, k)) { .. Get(o, k)` of djs-bgen (D2090): for an
    /// element of an Array, what both fast paths answer, with the element left as the
    /// Get's result (`n_res`, taken by its resume state right after).
    #[cfg_attr(target_arch = "wasm32", inline(never))]
    #[cfg_attr(not(target_arch = "wasm32"), inline(always))]
    pub(crate) fn n_dense_pre(&mut self, o: Value, k: Value) -> bool {
        if let Some(o) = o.as_object()
            && self.is_class(o, ObjectClass::Array)
            && let Some(i) = index_of(k)
            && let Some(v) = self.heap.get_element_fast(o, i)
        {
            debug_assert!(self.n_res.is_none(), "an inline result was not taken");
            self.n_res = Some(v);
            return true;
        }
        false
    }

    /// `%HasProperty(o, k)`: true for a dense element of an Array (an own property).
    pub(crate) fn n_has_property(&mut self, args: &[Value; 2], next: u32) -> Result<Flow, Fault> {
        if let Some(o) = args[0]
            .as_object()
            .filter(|&o| self.is_class(o, ObjectClass::Array))
            && let Some(i) = index_of(args[1])
            && self.heap.get_element_fast(o, i).is_some()
        {
            return self.n_inline(Value::TRUE, next);
        }
        self.n_call(const { iid("HasProperty") }, args, next)
    }

    /// `%CreateDataPropertyOrThrow(a, k, v)`: an Array's dense element or the next one
    /// (as `call_intrinsic`), whatever its prototypes hold.
    pub(crate) fn n_create_data_property_or_throw(
        &mut self,
        args: &[Value; 3],
        next: u32,
    ) -> Result<Flow, Fault> {
        self.n_retry(args, |ex, a| ex.n_cdp_once(a, next))
    }

    fn n_cdp_once(&mut self, args: &[Value], next: u32) -> Result<Flow, Fault> {
        if let Some(o) = args[0]
            .as_object()
            .filter(|&o| self.is_class(o, ObjectClass::Array))
            && let Some(i) = index_of(args[1])
            && self.heap.create_element_fast(o, i, args[2])?
        {
            return self.n_inline(Value::UNDEFINED, next);
        }
        self.n_call(const { iid("CreateDataPropertyOrThrow") }, args, next)
    }

    /// `%Set(o, k, v)` on an Array: an index through the heap's [[Set]] fast path, and
    /// `length` set to its current value (writable) is a no-op.
    pub(crate) fn n_set(&mut self, args: &[Value; 3], next: u32) -> Result<Flow, Fault> {
        self.n_retry(args, |ex, a| ex.n_set_once(a, next))
    }

    fn n_set_once(&mut self, args: &[Value], next: u32) -> Result<Flow, Fault> {
        if let Some(o) = args[0]
            .as_object()
            .filter(|&o| self.is_class(o, ObjectClass::Array))
        {
            if let Some(i) = index_of(args[1]) {
                if self.heap.set_element_fast(o, i, args[2])? {
                    return self.n_inline(Value::UNDEFINED, next);
                }
            } else if args[1] == self.heap.atom_to_value(crate::heap::atoms::LENGTH)?
                && args[2].as_number() == Some(self.heap.array_length(o) as f64)
                && self.heap.cell_flags(o.offset()) & crate::heap::cells::flags::OBJ_LENGTH_READONLY
                    == 0
            {
                return self.n_inline(Value::UNDEFINED, next);
            }
        }
        self.n_call(const { iid("Set") }, args, next)
    }

    /// The length of a virtual rest parameter (the arguments from `index` on, never
    /// materialized: djs-bgen uses it when the rest array is only read).
    pub(crate) fn n_rest_len(&self, index: u32) -> Value {
        u32_value(self.header_int(frame::ARGC).saturating_sub(index))
    }

    /// Element `i` of a virtual rest parameter.
    pub(crate) fn n_rest_get(&self, index: u32, i: Value) -> Value {
        let n = self.header_int(frame::ARGC).saturating_sub(index);
        match index_of(i) {
            Some(i) if i < n => self.get(self.bp + 2 + index + i),
            _ => Value::UNDEFINED,
        }
    }

    /// `LengthOfArrayLike(o)` of an Array: its length (an own data property holding a
    /// uint32, so Get and ToLength cannot run user code).
    pub(crate) fn n_length_of_array_like(
        &mut self,
        args: &[Value; 1],
        next: u32,
    ) -> Result<Flow, Fault> {
        if let Some(o) = args[0]
            .as_object()
            .filter(|&o| self.is_class(o, ObjectClass::Array))
        {
            let n = self.heap.array_length(o);
            return self.n_inline(u32_value(n), next);
        }
        self.n_call(const { iid("LengthOfArrayLike") }, args, next)
    }
}
