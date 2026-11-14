use super::{
    Context, PyConfig, PyGlobalState, VirtualMachine,
    runtime::{self, InterpreterWhence},
    setting::Settings,
    thread,
};
use crate::{
    PyResult, builtins, common::rc::PyRc, frozen::FrozenModule, getpath, py_freeze, stdlib::atexit,
    vm::PyBaseExceptionRef,
};
use alloc::collections::BTreeMap;
use core::sync::atomic::Ordering;

type InitFunc = Box<dyn FnOnce(&mut VirtualMachine)>;

/// Exit code used when stdout/stderr flush fails during interpreter shutdown.
/// Matches CPython's behavior (see cpython/Python/pylifecycle.c).
const EXITCODE_FLUSH_FAILURE: u32 = 120;

/// Configuration builder for constructing an Interpreter.
///
/// This is the preferred way to configure and create an interpreter with custom modules.
/// Modules must be registered before the interpreter is built,
/// similar to CPython's `PyImport_AppendInittab` which must be called before `Py_Initialize`.
///
/// # Example
/// ```
/// use rustpython_vm::Interpreter;
///
/// let builder = Interpreter::builder(Default::default());
/// // In practice, add stdlib: builder.add_native_modules(&stdlib_module_defs(&builder.ctx))
/// let interp = builder.build();
/// ```
pub struct InterpreterBuilder {
    settings: Settings,
    pub ctx: PyRc<Context>,
    module_defs: Vec<&'static builtins::PyModuleDef>,
    frozen_modules: Vec<(&'static str, FrozenModule)>,
    init_hooks: Vec<InitFunc>,
}

/// Options for constructing a main or sub-interpreter VM.
struct InitializeVmOpts<'a> {
    settings: Settings,
    ctx: PyRc<Context>,
    module_defs: Vec<&'static builtins::PyModuleDef>,
    frozen_modules: Vec<(&'static str, FrozenModule)>,
    init_hooks: Vec<InitFunc>,
    is_main: bool,
    whence: InterpreterWhence,
    /// When `Some`, reuse parent module_defs/frozen/config seeds for a subinterpreter.
    parent_state: Option<&'a PyGlobalState>,
    interp_config: runtime::InterpreterConfig,
}

/// Shared constructor for main and sub-interpreters.
fn initialize_vm<F>(opts: InitializeVmOpts<'_>, init: F) -> (VirtualMachine, PyRc<PyGlobalState>)
where
    F: FnOnce(&mut VirtualMachine),
{
    let InitializeVmOpts {
        settings,
        ctx,
        module_defs,
        frozen_modules,
        init_hooks,
        is_main,
        whence,
        parent_state,
        interp_config,
    } = opts;
    use crate::codecs::CodecsRegistry;
    use crate::common::lock::PyMutex;
    use crate::warn::WarningsState;
    use core::sync::atomic::{AtomicBool, AtomicI64, AtomicU64};
    use rustpython_common::atomic::AtomicCell;

    // Before any lock this interpreter's threads can contend on exists.

    let (config, all_module_defs, frozen, int_max_str_digits) = if let Some(parent) = parent_state {
        // Subinterpreter: clone config and module tables from parent, fresh runtime state.
        let int_max_str_digits = AtomicCell::new(parent.int_max_str_digits.load());
        (
            parent.config.clone(),
            parent.module_defs.clone(),
            parent.frozen.clone(),
            int_max_str_digits,
        )
    } else {
        // First top-level interpreter wins. Later seeds are ignored.
        super::init_hash_secret(settings.hash_seed);
        let paths = getpath::init_path_config(&settings);
        let config = PyConfig::new(settings, paths);

        // Build module_defs map from builtin modules + additional modules
        let mut all_module_defs: BTreeMap<&'static str, &'static builtins::PyModuleDef> =
            crate::stdlib::builtin_module_defs(&ctx)
                .into_iter()
                .chain(module_defs)
                .map(|def| (def.name.as_str(), def))
                .collect();

        // Register sysconfigdata under platform-specific name as well
        if let Some(&sysconfigdata_def) = all_module_defs.get("_sysconfigdata") {
            use std::sync::OnceLock;
            static SYSCONFIGDATA_NAME: OnceLock<&'static str> = OnceLock::new();
            let leaked_name = *SYSCONFIGDATA_NAME.get_or_init(|| {
                let name = crate::stdlib::sys::sysconfigdata_name();
                Box::leak(name.into_boxed_str())
            });
            all_module_defs.insert(leaked_name, sysconfigdata_def);
        }

        let int_max_str_digits = AtomicCell::new(match config.settings.int_max_str_digits {
            -1 => 4300,
            other => other,
        } as usize);

        let mut frozen: std::collections::HashMap<
            &'static str,
            FrozenModule,
            rustpython_common::random_state::RandomState,
        > = core_frozen_inits().collect();
        frozen.extend(frozen_modules);

        (config, all_module_defs, frozen, int_max_str_digits)
    };

    // Per-interpreter ephemeral state (must not be shared across interpreters).
    let codec_registry = CodecsRegistry::new(&ctx);
    let warnings = WarningsState::init_state(&ctx);

    let interpreter_id = runtime::alloc_interpreter_id();
    let runtime_root_id = parent_state.map_or(interpreter_id, |parent| parent.runtime_root_id);

    // Process main OS thread identity is process-global; subinterpreters inherit
    // it from the parent so `is_main_thread()` stays correct when running on the
    // main OS thread under a subinterpreter.

    let feature_flags = interp_config.feature_flags();
    let own_gil = interp_config.own_gil();

    // Create PyGlobalState (≈ PyInterpreterState)
    let global_state = PyRc::new(PyGlobalState {
        gc: crate::gc_state::GcInterpreterState::new(&ctx),
        interpreter_id,
        runtime_root_id,
        whence,
        is_main,
        config,
        module_defs: all_module_defs,
        frozen,
        stacksize: AtomicCell::new(0),
        thread_count: AtomicCell::new(0),
        atexit_funcs: PyMutex::default(),
        audit_hooks: PyMutex::default(),
        codec_registry,
        struct_format_cache: crate::buffer::FormatSpecCache::default(),
        finalizing: AtomicBool::new(false),
        warnings,
        override_frozen_modules: AtomicCell::new(0),
        before_forkers: PyMutex::default(),
        after_forkers_child: PyMutex::default(),
        after_forkers_parent: PyMutex::default(),
        int_max_str_digits,
        switch_interval: AtomicCell::new(0.005),
        global_trace_func: PyMutex::default(),
        global_profile_func: PyMutex::default(),
        type_mutex: PyMutex::default(),
        monitoring: PyMutex::default(),
        monitoring_events: AtomicCell::new(0),
        instrumentation_version: AtomicU64::new(0),
        feature_flags,
        own_gil,
        running_main: AtomicBool::new(false),
        ready: AtomicBool::new(false),
        id_refcount: AtomicI64::new(0),
        require_idref: AtomicBool::new(false),
    });

    // Create VM with the global state
    // Note: Don't clone here - init_hooks need exclusive access to mutate state
    let mut vm = VirtualMachine::new(ctx, global_state);

    // Execute initialization hooks (can mutate vm.state)
    for hook in init_hooks {
        hook(&mut vm);
    }

    // Call custom init function (can mutate vm.state)
    init(&mut vm);

    // Register before `initialize()` runs any Python: it allocates GC-tracked
    // objects, so a collection on another thread has to be able to stop this
    // interpreter while that happens. It cannot be registered earlier — the
    // hooks above take `PyRc::get_mut` on the state, which fails once the
    // registry holds a weak reference to it.
    runtime::register_interpreter(&vm.state);

    // `initialize()` runs Python bytecode directly (e.g. importing `codecs`
    // and `encodings`) before any `enter_vm` scope exists, so attach this
    // thread for the duration so type cache reads see it as ATTACHED.
    let vm_guard = thread::VmBootstrapGuard::new(&vm);
    vm.initialize();
    vm.state.ready.store(true, Ordering::Release);
    drop(vm_guard);

    // Clone global_state for Interpreter after all initialization is done
    let global_state = vm.state.clone();
    (vm, global_state)
}

/// Bootstrap a subinterpreter from an already-entered parent VM.
fn create_subinterpreter_from_parent(
    parent: &VirtualMachine,
    config: runtime::InterpreterConfig,
) -> Result<Interpreter, &'static str> {
    config.check()?;
    // Suspend the caller's current VM attachment (if any) for the duration
    // of subinterpreter initialization. Nested bootstrap would otherwise
    // swap `CURRENT_THREAD_SLOT` to the new interpreter while leaving the
    // outer interpreter's attach state inconsistent. Always restore, even
    // if initialization panics.

    let (vm, global_state) = initialize_vm(
        InitializeVmOpts {
            // settings unused when parent_state is Some
            settings: Settings::default(),
            ctx: parent.ctx.clone(),
            module_defs: Vec::new(),
            frozen_modules: Vec::new(),
            init_hooks: Vec::new(),
            is_main: false,
            whence: InterpreterWhence::Stdlib,
            parent_state: Some(&parent.state),
            interp_config: config,
        },
        |_| {},
    );
    let interp = Interpreter { global_state, vm };
    // Every interpreter has a `__main__` module once it is initialized.
    interp.enter(|vm| {
        let _ = vm.ensure_main_module();
    });
    Ok(interp)
}

impl InterpreterBuilder {
    /// Create a new interpreter configuration with default settings.
    #[must_use]
    pub fn new() -> Self {
        Self {
            settings: Settings::default(),
            ctx: Context::genesis().clone(),
            module_defs: Vec::new(),
            frozen_modules: Vec::new(),
            init_hooks: Vec::new(),
        }
    }

    /// Set custom settings for the interpreter.
    ///
    /// If called multiple times, only the last settings will be used.
    #[must_use]
    pub fn settings(mut self, settings: Settings) -> Self {
        self.settings = settings;
        self
    }

    /// Add a single native module definition.
    ///
    /// # Example
    /// ```
    /// use rustpython_vm::{Interpreter, builtins::PyModuleDef};
    ///
    /// let builder = Interpreter::builder(Default::default());
    /// // Note: In practice, use module_def from your #[pymodule]
    /// // let def = mymodule::module_def(&builder.ctx);
    /// // let interp = builder.add_native_module(def).build();
    /// let interp = builder.build();
    /// ```
    #[must_use]
    pub fn add_native_module(self, def: &'static builtins::PyModuleDef) -> Self {
        self.add_native_modules(&[def])
    }

    /// Add multiple native module definitions.
    ///
    /// # Example
    /// ```
    /// use rustpython_vm::Interpreter;
    ///
    /// let builder = Interpreter::builder(Default::default());
    /// // In practice, use module_defs from rustpython_stdlib:
    /// // let defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
    /// // let interp = builder.add_native_modules(&defs).build();
    /// let interp = builder.build();
    /// ```
    #[must_use]
    pub fn add_native_modules(mut self, defs: &[&'static builtins::PyModuleDef]) -> Self {
        self.module_defs.extend_from_slice(defs);
        self
    }

    /// Add a custom initialization hook.
    ///
    /// Hooks are executed in the order they are added during interpreter creation.
    /// This function will be called after modules are registered but before
    /// the VM is initialized, allowing for additional customization.
    ///
    /// # Example
    /// ```
    /// use rustpython_vm::Interpreter;
    ///
    /// let interp = Interpreter::builder(Default::default())
    ///     .init_hook(|vm| {
    ///         // Custom initialization
    ///     })
    ///     .build();
    /// ```
    #[must_use]
    pub fn init_hook<F>(mut self, init: F) -> Self
    where
        F: FnOnce(&mut VirtualMachine) + 'static,
    {
        self.init_hooks.push(Box::new(init));
        self
    }

    /// Add frozen modules to the interpreter.
    ///
    /// Frozen modules are Python modules compiled into the binary.
    /// This method accepts any iterator of (name, FrozenModule) pairs.
    ///
    /// # Example
    /// ```
    /// use rustpython_vm::Interpreter;
    ///
    /// let interp = Interpreter::builder(Default::default())
    ///     // In practice: .add_frozen_modules(rustpython_pylib::FROZEN_STDLIB)
    ///     .build();
    /// ```
    #[must_use]
    pub fn add_frozen_modules<I>(mut self, frozen: I) -> Self
    where
        I: IntoIterator<Item = (&'static str, FrozenModule)>,
    {
        self.frozen_modules.extend(frozen);
        self
    }

    /// Build the interpreter.
    ///
    /// This consumes the configuration and returns a fully initialized Interpreter.
    #[must_use]
    pub fn build(self) -> Interpreter {
        let (vm, global_state) = initialize_vm(
            InitializeVmOpts {
                settings: self.settings,
                ctx: self.ctx,
                module_defs: self.module_defs,
                frozen_modules: self.frozen_modules,
                init_hooks: self.init_hooks,
                is_main: true,
                whence: InterpreterWhence::Runtime,
                parent_state: None,
                interp_config: runtime::InterpreterConfig::MAIN,
            },
            |_| {}, // No additional init needed
        );
        Interpreter { global_state, vm }
    }

    /// Alias for `build()` for compatibility with the `interpreter()` pattern.
    #[must_use]
    pub fn interpreter(self) -> Interpreter {
        self.build()
    }
}

impl Default for InterpreterBuilder {
    fn default() -> Self {
        Self::new()
    }
}

/// One isolated Python interpreter in the process (≈ CPython `PyInterpreterState` + main tstate).
///
/// Historically RustPython exposed a single process-level `Interpreter`. For PEP 734
/// (multiple interpreters / subinterpreters) this type is now the owned handle for
/// **one** interpreter. Use [`Interpreter::create_subinterpreter`] to create additional
/// isolated interpreters that share the process-wide type context but not modules or
/// `PyGlobalState`.
///
/// # Examples
/// Runs a simple embedded hello world program.
/// ```
/// use rustpython_vm::Interpreter;
/// use rustpython_vm::compiler::Mode;
/// Interpreter::without_stdlib(Default::default()).enter(|vm| {
///     let scope = vm.new_scope_with_builtins();
///     let source = r#"print("Hello World!")"#;
///     let code_obj = vm.compile(
///             source,
///             Mode::Exec,
///             "<embedded>",
///     ).map_err(|err| err.into_pyexception(vm, Some(source))).unwrap();
///     vm.run_code_obj(code_obj, scope).unwrap();
/// });
/// ```
pub struct Interpreter {
    pub global_state: PyRc<PyGlobalState>,
    vm: VirtualMachine,
}

impl Interpreter {
    /// Create a new interpreter configuration builder.
    ///
    /// # Example
    /// ```
    /// use rustpython_vm::Interpreter;
    ///
    /// let builder = Interpreter::builder(Default::default());
    /// // In practice, add stdlib: builder.add_native_modules(&stdlib_module_defs(&builder.ctx))
    /// let interp = builder.build();
    /// ```
    #[must_use]
    pub fn builder(settings: Settings) -> InterpreterBuilder {
        InterpreterBuilder::new().settings(settings)
    }

    /// This is a bare unit to build up an interpreter without the standard library.
    /// To create an interpreter with the standard library with the `rustpython` crate, use `rustpython::InterpreterBuilder`.
    /// To create an interpreter without the `rustpython` crate, but only with `rustpython-vm`,
    /// try to build one from the source code of `InterpreterBuilder`. It will not be a one-liner but it also will not be too hard.
    #[must_use]
    pub fn without_stdlib(settings: Settings) -> Self {
        Self::with_init(settings, |_| {})
    }

    /// Create with initialize function taking mutable vm reference.
    ///
    /// Note: This is a legacy API. To add stdlib, use `Interpreter::builder()` instead.
    pub fn with_init<F>(settings: Settings, init: F) -> Self
    where
        F: FnOnce(&mut VirtualMachine),
    {
        let (vm, global_state) = initialize_vm(
            InitializeVmOpts {
                settings,
                ctx: Context::genesis().clone(),
                module_defs: Vec::new(),
                frozen_modules: Vec::new(),
                init_hooks: Vec::new(),
                is_main: true,
                whence: InterpreterWhence::Runtime,
                parent_state: None,
                interp_config: runtime::InterpreterConfig::MAIN,
            },
            init,
        );
        Self { global_state, vm }
    }

    /// Process-global interpreter id (main is [`super::MAIN_INTERPRETER_ID`]).
    #[inline]
    #[must_use]
    pub fn id(&self) -> i64 {
        self.global_state.interpreter_id
    }

    /// Where this interpreter was created.
    #[inline]
    #[must_use]
    pub fn whence(&self) -> InterpreterWhence {
        self.global_state.whence
    }

    /// Whether this is a top-level interpreter rather than a subinterpreter.
    ///
    /// Every top-level interpreter answers `true`; for *the* process main, use
    /// [`Interpreter::is_process_main`].
    #[inline]
    #[must_use]
    pub fn is_main(&self) -> bool {
        self.global_state.is_main
    }

    /// Whether this is the PEP 734 process main interpreter (`get_main()`).
    ///
    /// Unlike [`Interpreter::is_main`], which is set for every top-level
    /// interpreter, this is true for only the single first-registered main.
    #[inline]
    #[must_use]
    pub fn is_process_main(&self) -> bool {
        runtime::main_interpreter_id() == Some(self.id())
    }

    /// Create an isolated subinterpreter sharing this interpreter's type context
    /// (`Context`) and module definitions, but with its own `sys.modules`,
    /// builtins module instance, thread registry, and stop-the-world state.
    ///
    /// May be called while the parent is entered (matching CPython, where
    /// `_interpreters.create()` runs under the main interpreter). When the
    /// calling thread is currently attached to a VM, that attachment is
    /// temporarily saved so the subinterpreter can bootstrap as an outermost
    /// enter (correct thread-slot / stop-the-world state).
    #[must_use]
    pub fn create_subinterpreter(&self) -> Self {
        self.create_subinterpreter_with_config(runtime::InterpreterConfig::ISOLATED)
            .expect("the isolated config is always valid")
    }

    /// Create a subinterpreter from a parent VM (the currently entered one).
    pub fn create_subinterpreter_from_vm(
        parent: &VirtualMachine,
        config: runtime::InterpreterConfig,
    ) -> Result<Self, &'static str> {
        create_subinterpreter_from_parent(parent, config)
    }

    /// Create a subinterpreter with an explicit PEP 734 / `PyInterpreterConfig`.
    pub fn create_subinterpreter_with_config(
        &self,
        config: runtime::InterpreterConfig,
    ) -> Result<Self, &'static str> {
        create_subinterpreter_from_parent(&self.vm, config)
    }

    /// Run a function with the main virtual machine and return a PyResult of the result.
    ///
    /// To enter vm context multiple times or to avoid buffer/exception management, this function is preferred.
    /// `enter` is lightweight and it returns a python object in PyResult.
    /// You can stop or continue the execution multiple times by calling `enter`.
    ///
    /// To finalize the vm once all desired `enter`s are called, calling `finalize` will be helpful.
    ///
    /// See also [`Interpreter::run`] for managed way to run the interpreter.
    pub fn enter<F, R>(&self, f: F) -> R
    where
        F: FnOnce(&VirtualMachine) -> R,
    {
        thread::enter_vm(&self.vm, || f(&self.vm))
    }

    /// Run [`Interpreter::enter`] and call [`VirtualMachine::expect_pyresult`] for the result.
    ///
    /// This function is useful when you want to expect a result from the function,
    /// but also print useful panic information when exception raised.
    ///
    /// See also [`Interpreter::enter`] and [`VirtualMachine::expect_pyresult`] for more information.
    pub fn enter_and_expect<F, R>(&self, f: F, msg: &str) -> R
    where
        F: FnOnce(&VirtualMachine) -> PyResult<R>,
    {
        self.enter(|vm| {
            let result = f(vm);
            vm.expect_pyresult(result, msg)
        })
    }

    /// Run a function with the main virtual machine and return exit code.
    ///
    /// To enter vm context only once and safely terminate the vm, this function is preferred.
    /// Unlike [`Interpreter::enter`], `run` calls finalize and returns exit code.
    /// You will not be able to obtain Python exception in this way.
    ///
    /// See [`Interpreter::finalize`] for the finalization steps.
    /// See also [`Interpreter::enter`] for pure function call to obtain Python exception.
    pub fn run<F>(self, f: F) -> u32
    where
        F: FnOnce(&VirtualMachine) -> PyResult<()>,
    {
        let res = self.enter(|vm| f(vm));
        self.finalize(res.err())
    }

    /// Finalize vm and turns an exception to exit code.
    ///
    /// Finalization steps (matching Py_FinalizeEx):
    /// 1. Flush stdout and stderr.
    /// 1. Handle exit exception and turn it to exit code.
    /// 1. Call threading._shutdown() to join non-daemon threads.
    /// 1. Run atexit exit functions.
    /// 1. Set finalizing flag and hang remaining daemon threads.
    /// 1. Forced GC collection pass (collect cycles while builtins are available).
    /// 1. Module finalization (finalize_modules).
    /// 1. Clear interpreter-owned cross-interpreter data.
    /// 1. Final stdout/stderr flush.
    ///
    /// Note that calling `finalize` is not necessary by purpose though.
    pub fn finalize(self, exc: Option<PyBaseExceptionRef>) -> u32 {
        self.enter(|vm| {
            let mut flush_status = vm.flush_std();

            // See if any exception leaked out:
            let exit_code = if let Some(exc) = exc {
                vm.handle_exit_exception(exc)
            } else {
                0
            };

            // Wait for thread shutdown - call threading._shutdown() if available.
            // This waits for all non-daemon threads to complete.
            // threading module may not be imported, so ignore import errors.

            // Run atexit handlers before setting finalizing flag.
            // This allows unraisable exceptions from atexit handlers to be reported.
            atexit::_run_exitfuncs(vm);

            // Clean up any lingering subinterpreters. This has to happen before
            // the finalizing flag is set, or else threads might get prematurely
            // blocked.

            // Remaining daemon threads hang in `_PyThreadState_HangThread` when
            // they next attach (or at the eval-breaker if they are already
            // running). Their `_ThreadHandle` stays not-done so `is_alive()`
            // is still true for `join()` during the GC that follows.
            vm.state.finalizing.store(true, Ordering::Release);

            // GC pass - collect cycles before module cleanup
            vm.state.gc.collect_force(2);

            // Module finalization: remove modules from sys.modules, GC collect
            // (while builtins is still available for __del__), then clear module dicts.
            vm.finalize_modules();

            // CPython clears low-level cross-interpreter container data from
            // _PyAtExit_Fini(), after Python atexit callbacks and module
            // finalization.  In particular, values sent by an atexit callback
            // must also become unbound when this interpreter goes away.
            let interpreter_id = vm.state.interpreter_id;
            crate::stdlib::_interpchannels::clear_interpreter(interpreter_id);
            crate::stdlib::_interpqueues::clear_interpreter(interpreter_id);

            if vm.flush_std() < 0 && flush_status == 0 {
                flush_status = -1;
            }

            // Match CPython: if exit_code is 0 and stdout flush failed, exit 120
            let exit_code = if exit_code == 0 && flush_status < 0 {
                EXITCODE_FLUSH_FAILURE
            } else {
                exit_code
            };

            // Daemon threads may still exist, so use the safe `process()`,
            // not `drain_all()`.

            exit_code
        })
    }
}

fn core_frozen_inits() -> impl Iterator<Item = (&'static str, FrozenModule)> {
    let iter = core::iter::empty();
    macro_rules! ext_modules {
        ($iter:ident, $($t:tt)*) => {
            let $iter = $iter.chain(py_freeze!($($t)*));
        };
    }

    // Python modules that the vm calls into, but are not actually part of the stdlib. They could
    // in theory be implemented in Rust, but are easiest to do in Python for one reason or another.
    // Includes _importlib_bootstrap and _importlib_bootstrap_external
    ext_modules!(
        iter,
        dir = "../../Lib/python_builtins",
        crate_name = "rustpython_compiler_core"
    );

    // core stdlib Python modules that the vm calls into, but are still used in Python
    // application code, e.g. copyreg
    // FIXME: Initializing core_modules here results duplicated frozen module generation for core_modules.
    // We need a way to initialize this modules for both `Interpreter::without_stdlib()` and `InterpreterBuilder::new().init_stdlib().interpreter()`
    // #[cfg(not(feature = "freeze-stdlib"))]
    ext_modules!(
        iter,
        dir = "../../Lib/core_modules",
        crate_name = "rustpython_compiler_core"
    );

    // Collect frozen module entries
    let mut entries: Vec<_> = iter.collect();

    // Add test module aliases
    if let Some(hello_code) = entries
        .iter()
        .find(|(n, _)| *n == "__hello__")
        .map(|(_, m)| m.code)
    {
        entries.push((
            "__hello_alias__",
            FrozenModule {
                code: hello_code,
                package: false,
            },
        ));
        entries.push((
            "__phello_alias__",
            FrozenModule {
                code: hello_code,
                package: true,
            },
        ));
        entries.push((
            "__phello_alias__.spam",
            FrozenModule {
                code: hello_code,
                package: false,
            },
        ));
        entries.push((
            "__hello_only__",
            FrozenModule {
                code: hello_code,
                package: false,
            },
        ));
    }
    if let Some(code) = entries
        .iter()
        .find(|(n, _)| *n == "__phello__")
        .map(|(_, m)| m.code)
    {
        entries.push((
            "__phello__.__init__",
            FrozenModule {
                code,
                package: false,
            },
        ));
    }
    if let Some(code) = entries
        .iter()
        .find(|(n, _)| *n == "__phello__.ham")
        .map(|(_, m)| m.code)
    {
        entries.push((
            "__phello__.ham.__init__",
            FrozenModule {
                code,
                package: false,
            },
        ));
    }
    entries.into_iter()
}
