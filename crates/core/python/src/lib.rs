mod capi;
mod host;
mod imports;
mod kiwisolver;
mod matplotlib;
#[cfg(not(target_family = "wasm"))]
mod native;
mod psh;
mod shell;
mod sitedata;
mod tree;

use std::{collections::BTreeSet, io, rc::Rc, sync::Arc, time::Duration};

use rustpython_vm::{
    AsObject, Interpreter, PyObjectRef, PyResult, Settings, VirtualMachine,
    builtins::{PyBaseExceptionRef, PyBytes, PyCode, PyDictRef},
    common::rc::PyRc,
    frozen::FrozenLib,
    function::IntoFuncArgs,
    scope::Scope,
    vm::limit::Limiter,
};

pub use capi::{Extension, add_extensions};
pub use host::held;
pub use psh::UTILITY;
#[cfg(not(target_family = "wasm"))]
pub use psh::serve;
pub use rustpython_vm;
pub use rustpython_vm::vm::limit::Metered;
pub use rustpython_vm::vm::region;
pub use rustpython_vm::vm::region::{Kept, Regions};
pub use shell::{Budget, Command};
pub use tree::{Delta, Entry, Tree};

const _: [&str; 3] = [
    include_str!("posix.py"),
    include_str!("boot.py"),
    include_str!("subprocess.py"),
];

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    File,
    Dir,
    Symlink,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stat {
    pub kind: Kind,
    pub size: u64,
    pub mode: u32,
    pub mtime_ns: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Write {
    Replace,
    Append,
    New,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Remove {
    File,
    Dir,
}

pub struct Child {
    pub argv: Vec<Vec<u8>>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub cwd: Vec<u8>,
    pub stdin: Vec<u8>,
    pub merged: bool,
}

#[derive(Debug)]
pub struct Exited {
    pub status: i32,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
}

pub trait Files {
    fn stat(&self, path: &[u8]) -> io::Result<Stat>;
    fn read(&self, path: &[u8], limit: u64) -> io::Result<Vec<u8>>;
    fn write(&self, path: &[u8], data: &[u8], how: Write) -> io::Result<()>;
    fn list(&self, path: &[u8]) -> io::Result<Vec<Vec<u8>>>;
    fn mkdir(&self, path: &[u8], mode: u32) -> io::Result<()>;
    fn remove(&self, path: &[u8], what: Remove) -> io::Result<()>;
    fn rename(&self, from: &[u8], to: &[u8]) -> io::Result<()>;

    fn read_at(&self, path: &[u8], offset: u64, len: usize, limit: u64) -> io::Result<Vec<u8>> {
        let data = self.read(path, limit)?;
        let start = usize::try_from(offset)
            .unwrap_or(usize::MAX)
            .min(data.len());
        let end = start.saturating_add(len).min(data.len());
        Ok(data[start..end].to_vec())
    }

    fn set_mode(&self, _path: &[u8], _mode: u32) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn set_mtime(&self, _path: &[u8], _mtime_ns: i64) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// The entry at a path itself: a symlink rather than what it points at.
    fn lstat(&self, path: &[u8]) -> io::Result<Stat> {
        self.stat(path)
    }

    fn symlink(&self, _target: &[u8], _link: &[u8]) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    fn read_link(&self, _path: &[u8]) -> io::Result<Vec<u8>> {
        Err(io::ErrorKind::InvalidInput.into())
    }

    /// Makes `link` another name of the file `existing` (not following a symlink
    /// there), so a change through either shows through both.
    fn hard_link(&self, _existing: &[u8], _link: &[u8]) -> io::Result<()> {
        Err(io::ErrorKind::Unsupported.into())
    }

    /// The hard-link group of the file at `path` and how many names it has, or
    /// `None` when it has only its own.
    fn links(&self, _path: &[u8], _follow: bool) -> Option<(u64, u64)> {
        None
    }

    fn ranged(&self) -> bool {
        false
    }

    fn spawns(&self) -> bool {
        false
    }

    fn spawn(&self, _child: Child) -> io::Result<Exited> {
        Err(io::ErrorKind::Unsupported.into())
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub output: u64,
    pub file: u64,
    pub sleep: Duration,
    pub steps: u64,
    pub time: Duration,
    pub memory: Option<u64>,
    pub depth: usize,
}

pub type Stop = Arc<dyn Fn() -> bool + Send + Sync>;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Limit {
    Output,
    Sleep,
    Steps,
    Time,
    Memory,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Why {
    Package,
    Machine,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Missing {
    pub module: String,
    pub why: Why,
    pub error: String,
}

impl std::fmt::Display for Missing {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let lacks = match self.why {
            Why::Package => "Python packages, only the standard library",
            Why::Machine => "processes, sockets or native extensions for Python",
        };
        write!(
            formatter,
            "python3: {}: {} (this simulated shell has no {lacks})",
            self.module, self.error
        )
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Held {
    pub bytes: u64,
    pub asked: u64,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Ended {
    Exited(i32),
    Needs(Vec<Missing>),
    Refused(Limit),
    Cancelled,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Changes {
    pub written: BTreeSet<Vec<u8>>,
    pub removed: BTreeSet<Vec<u8>>,
    pub made: BTreeSet<Vec<u8>>,
    pub moved: Vec<(Vec<u8>, Vec<u8>)>,
}

pub enum Stdio {
    Held(Vec<u8>),
    Process,
}

pub struct Run {
    pub argv: Vec<String>,
    pub stdio: Stdio,
    pub cwd: Vec<u8>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub limits: Limits,
    pub stop: Stop,
}

#[derive(Debug)]
pub struct Ran {
    pub ended: Ended,
    pub stdout: Vec<u8>,
    pub stderr: Vec<u8>,
    /// The order the program wrote stdout and stderr in, as (fd, length) runs over
    /// them; empty when unknown, which segments reads as all stdout then all stderr.
    pub order: Vec<(u8, usize)>,
    pub changes: Changes,
    pub steps: u64,
    pub held: Held,
    pub kept: Kept,
}

impl Ran {
    /// stdout and stderr as (fd, bytes) runs in the order the program wrote them, so
    /// 2>&1 keeps CPython's order (stderr line-buffered, stdout flushed later).
    #[must_use]
    pub fn segments(&self) -> Vec<(u8, &[u8])> {
        let mut out = Vec::new();
        let (mut at_out, mut at_err) = (0_usize, 0_usize);
        for &(fd, len) in &self.order {
            let (bytes, at) = if fd == 2 {
                (&self.stderr, &mut at_err)
            } else {
                (&self.stdout, &mut at_out)
            };
            let end = (*at).saturating_add(len).min(bytes.len());
            out.push((fd, &bytes[*at..end]));
            *at = end;
        }
        if at_out < self.stdout.len() {
            out.push((1, &self.stdout[at_out..]));
        }
        if at_err < self.stderr.len() {
            out.push((2, &self.stderr[at_err..]));
        }
        out
    }
}

enum Program {
    Command(String),
    Module(String),
    Stdin,
    File(String),
    Version,
}

struct Invocation {
    program: Program,
    argv: Vec<String>,
    /// -u: stdout written through, not held until it fills or the program ends.
    unbuffered: bool,
}

fn invocation(args: &[String]) -> Result<Invocation, String> {
    let mut rest = args.iter();
    let mut unbuffered = false;
    while let Some(arg) = rest.next() {
        let program = match arg.as_str() {
            "-c" => {
                let code = rest.next().ok_or("Argument expected for the -c option")?;
                Program::Command(code.clone())
            }
            "-m" => {
                let module = rest.next().ok_or("Argument expected for the -m option")?;
                Program::Module(module.clone())
            }
            "-" => Program::Stdin,
            "-V" | "--version" => Program::Version,
            "-W" | "-X" => {
                rest.next();
                continue;
            }
            "-u" => {
                unbuffered = true;
                continue;
            }
            "-B" | "-E" | "-s" | "-S" | "-I" | "-O" | "-OO" | "-q" | "-b" | "-P" => continue,
            option if option.starts_with('-') => return Err(format!("Unknown option: {option}")),
            path => Program::File(path.to_owned()),
        };
        let first = match &program {
            Program::Command(_) => "-c",
            Program::Module(module) => module,
            Program::Stdin | Program::File(_) | Program::Version => arg,
        };
        let argv = std::iter::once(first.to_owned())
            .chain(rest.cloned())
            .collect();
        return Ok(Invocation {
            program,
            argv,
            unbuffered,
        });
    }
    Ok(Invocation {
        program: Program::Stdin,
        argv: vec![String::new()],
        unbuffered,
    })
}

enum Prepared {
    Source(String, String),
    /// A compiled .pyc: its marshalled code after the 16-byte header, and its path.
    Compiled(Vec<u8>, String),
    Module(String),
    Exited(i32),
}

static FROZEN: std::sync::OnceLock<&'static FrozenLib> = std::sync::OnceLock::new();

pub fn add_frozen(lib: &'static FrozenLib) -> Result<(), &'static FrozenLib> {
    FROZEN.set(lib)
}

fn added_frozen() -> Option<&'static FrozenLib> {
    capi::run_constructors();
    FROZEN.get().copied()
}

pub struct Python {
    interpreter: Option<Interpreter>,
    boot: PyDictRef,
    depth: usize,
}

impl Drop for Python {
    fn drop(&mut self) {
        if let Some(interpreter) = self.interpreter.take() {
            interpreter.enter(|vm| {
                // Give back what boot froze, so finalize collects its cycles.
                vm.state.gc.unfreeze();
                self.boot.clear();
                vm.set_limiter(Some(PyRc::new(host::Shut) as PyRc<dyn Limiter>));
            });
            interpreter.finalize(None);
        }
    }
}

fn decoded(source: Vec<u8>, name: &str) -> Prepared {
    match String::from_utf8(source) {
        Ok(source) => Prepared::Source(source, name.to_owned()),
        Err(_) => {
            host::print(&format!("python3: {name} is not UTF-8\n"));
            Prepared::Exited(1)
        }
    }
}

impl Python {
    pub fn new() -> io::Result<Self> {
        let mut seed = [0; 4];
        nanocode_platform::fill_random(&mut seed)?;
        Self::seeded(u32::from_le_bytes(seed))
    }

    /// An interpreter whose string hashes use hash_seed, drawing none from the host.
    pub(crate) fn seeded(hash_seed: u32) -> io::Result<Self> {
        let mut settings = Settings::default();
        settings.install_signal_handlers = false;
        settings.import_site = false;
        settings.user_site_directory = false;
        settings.write_bytecode = false;
        settings.ignore_environment = true;
        settings.isolated = true;
        settings.hash_seed = Some(hash_seed);
        rustpython_vm::import::set_external_frozen(rustpython_vm::import::ExternalFrozen {
            find: nanocode_python_data::find_module,
            load: nanocode_python_data::load_module,
        });
        let builder = Interpreter::builder(settings);
        let defs = rustpython_stdlib::stdlib_module_defs(&builder.ctx);
        let host = host::module_def(&builder.ctx);
        let extensions = capi::module_defs(&builder.ctx);
        let kiwisolver = kiwisolver::module_def(&builder.ctx);
        let matplotlib = matplotlib::module_def(&builder.ctx);
        let interpreter = builder
            .add_native_modules(&defs)
            .add_native_modules(&extensions)
            .add_native_module(host)
            .add_native_module(kiwisolver)
            .add_native_module(matplotlib)
            .add_frozen_modules(added_frozen().into_iter().flat_map(|lib| lib.decode()))
            .build();
        let (boot, depth) = interpreter.enter(|vm| {
            // What boot left alive lives as long as the interpreter: keep it out of
            // every collection's scan, as gc.freeze() does after a fork server boots.
            vm.state.gc.freeze();
            (vm.ctx.new_dict(), vm.recursion_limit.get())
        });
        Ok(Self {
            interpreter: Some(interpreter),
            boot,
            depth,
        })
    }

    pub fn run(&self, run: Run, files: Rc<dyn Files>) -> Ran {
        let refused = |message: String| Ran {
            ended: Ended::Exited(2),
            stdout: Vec::new(),
            stderr: format!("python3: {message}\n").into_bytes(),
            order: Vec::new(),
            changes: Changes::default(),
            steps: 0,
            held: Held::default(),
            kept: Kept::default(),
        };
        if !run.cwd.starts_with(b"/") {
            return refused("the working directory is not an absolute path".to_owned());
        }
        if run.limits.memory.is_some() && host::held().is_none() {
            return refused(
                "a memory limit needs this program's allocator to be Metered".to_owned(),
            );
        }
        let invocation = match invocation(&run.argv) {
            Ok(invocation) => invocation,
            Err(message) => return refused(message),
        };
        let depth = run.limits.depth;
        let entered = host::enter(host::State::new(files, run));
        let (ended, left) = self
            .interpreter
            .as_ref()
            .expect("the interpreter lives as long as its owner")
            .enter(|vm| {
                vm.recursion_cap.set(depth);
                vm.recursion_limit.set(depth.min(self.depth));
                vm.set_limiter(Some(PyRc::new(host::Budget) as PyRc<dyn Limiter>));
                let ended =
                    vm.with_memory_ceiling(host::memory(), || self.execute(vm, &invocation));
                let left = vm.fuel.get();
                vm.set_limiter(None);
                (ended, left)
            });
        let kept = region::leave();
        let state = entered.leave();
        let steps = state.spent(left);
        Ran {
            ended: match state.refused {
                _ if state.cancelled => Ended::Cancelled,
                Some(limit) => Ended::Refused(limit),
                // A process's status is one byte, as in exit() below.
                None => state
                    .exited
                    .map_or(ended, |code| Ended::Exited(code & 0xff)),
            },
            stdout: state.stdout,
            stderr: state.stderr,
            order: state.order,
            changes: state.changes,
            steps,
            held: state.held_at_refusal,
            kept,
        }
    }

    fn execute(&self, vm: &VirtualMachine, invocation: &Invocation) -> Ended {
        let prepared = match self.prepare(vm, invocation) {
            Ok(prepared) => prepared,
            Err(_) if vm.stopped.borrow().is_some() => return Ended::Exited(1),
            Err(error) => {
                let mut text = String::new();
                let _ = vm.write_exception(&mut text, &error);
                host::print(&text);
                return Ended::Exited(1);
            }
        };
        region::enter(cfg!(debug_assertions));
        let (source, name, compiled) = match prepared {
            Prepared::Source(source, name) => (source, name, None),
            Prepared::Compiled(code, name) => (String::new(), name, Some(code)),
            Prepared::Module(module) => return self.execute_module(vm, module),
            Prepared::Exited(code) => return Ended::Exited(code),
        };
        let names = imports::top_level(&source)
            .into_iter()
            .map(|name| vm.ctx.new_str(name).into())
            .collect();
        let missing = self.missing(vm, "_missing", vm.ctx.new_list(names).into());
        // A budget that stopped while looking for the imports (a file the host has
        // yet to fetch, a cancel, a limit) ends the run here: the script must not
        // start on a machine that refuses every step.
        if vm.stopped.borrow().is_some() {
            return Ended::Exited(1);
        }
        if !missing.is_empty() {
            return Ended::Needs(missing);
        }
        let result = vm
            .new_scope_with_main()
            .and_then(|scope| self.run_main(vm, invocation, scope, &source, &name, &compiled));
        Ended::Exited(self.exit(vm, result.err()))
    }

    fn prepare(&self, vm: &VirtualMachine, invocation: &Invocation) -> PyResult<Prepared> {
        if self.boot.is_empty() {
            self.boot_modules(vm)?;
        }
        // CPython 3.12 runs a directory by its __main__.py with the directory
        // first on sys.path, and a module from -m with the working directory there.
        let directory = match &invocation.program {
            Program::File(path) => matches!(
                host::source(path),
                Err(error) if error.kind() == io::ErrorKind::IsADirectory
            ),
            _ => false,
        };
        let first = match &invocation.program {
            Program::File(path) if directory => path.as_str(),
            Program::File(path) => path.rfind('/').map_or(".", |cut| &path[..cut.max(1)]),
            Program::Module(_) => ".",
            _ => "",
        };
        let argv: Vec<PyObjectRef> = invocation
            .argv
            .iter()
            .map(|arg| vm.ctx.new_str(arg.as_str()).into())
            .collect();
        let first = vm.ctx.new_str(first);
        self.call(
            vm,
            "_start",
            (vm.ctx.new_list(argv), first, invocation.unbuffered),
        )?;
        self.program(vm, invocation, directory)
    }

    fn execute_module(&self, vm: &VirtualMachine, module: String) -> Ended {
        let missing = self.missing(vm, "_absent", vm.ctx.new_str(module.as_str()).into());
        if vm.stopped.borrow().is_some() {
            return Ended::Exited(1);
        }
        // A module that is no package of the standard library is missing as
        // CPython says it is (status 1), not a gap of this shell.
        if !missing.is_empty() && missing.iter().all(|missing| missing.why == Why::Package) {
            let said = if module.contains('.') {
                format!(
                    "/usr/bin/python3: Error while finding module specification for {module:?} (ModuleNotFoundError: {})\n",
                    missing[0].error
                )
            } else {
                format!("/usr/bin/python3: No module named {module}\n")
            };
            host::print(&said);
            return Ended::Exited(1);
        }
        if !missing.is_empty() {
            return Ended::Needs(missing);
        }
        let result = vm
            .new_scope_with_main()
            .and_then(|_| vm.run_module(&module));
        Ended::Exited(self.exit(vm, result.err()))
    }

    fn run_main(
        &self,
        vm: &VirtualMachine,
        invocation: &Invocation,
        scope: Scope,
        source: &str,
        name: &str,
        compiled: &Option<Vec<u8>>,
    ) -> PyResult {
        if let Program::File(_) = &invocation.program {
            // CPython 3.12 names a script by its path joined to the working
            // directory, unnormalized ("./x.py" stays), in __file__ and in its
            // tracebacks; a directory runs as its __main__.py.
            let path = name;
            let os = vm.import("os", 0)?;
            let cwd = os.get_attr("getcwd", vm)?.call((), vm)?;
            let path = os
                .get_attr("path", vm)?
                .get_attr("join", vm)?
                .call((cwd, vm.ctx.new_str(path)), vm)?
                .str(vm)?
                .to_string();
            scope
                .globals
                .set_item("__file__", vm.ctx.new_str(path.as_str()).into(), vm)?;
            if let Some(compiled) = compiled {
                // CPython 3.12 runs a .pyc's code object itself as __main__, with
                // __cached__ None.
                scope.globals.set_item("__cached__", vm.ctx.none(), vm)?;
                let code = vm
                    .import("marshal", 0)?
                    .get_attr("loads", vm)?
                    .call((vm.ctx.new_bytes(compiled.clone()),), vm)?
                    .downcast::<PyCode>()
                    .map_err(|_| vm.new_runtime_error("Bad code object in .pyc file".to_owned()))?;
                return vm.run_code_obj(code, scope);
            }
            return vm.run_string(scope, source, &path);
        }
        // CPython 3.12 keeps no source for a program from -c or stdin, so its
        // tracebacks name the lines without showing them: skip linecache.
        let code = vm
            .compile(source, rustpython_vm::compiler::Mode::Exec, name)
            .map_err(|err| err.into_pyexception(vm, Some(source)))?;
        vm.run_code_obj(code, scope)
    }

    fn boot_modules(&self, vm: &VirtualMachine) -> PyResult<()> {
        let modules = vm.sys_module.get_attr("modules", vm)?;
        let posix = vm.new_module("posix", vm.ctx.new_dict(), None);
        modules.set_item("posix", posix.clone().into(), vm)?;
        modules.set_item("_datetime", vm.ctx.none(), vm)?;
        let code = rustpython_vm::py_compile!(file = "posix.py", module_name = "posix");
        let scope = Scope::with_builtins(None, posix.dict(), vm);
        vm.run_code_obj(vm.ctx.new_code(code), scope)?;
        let code = rustpython_vm::py_compile!(file = "subprocess.py", module_name = "subprocess");
        self.boot
            .set_item("_subprocess", vm.ctx.new_code(code).into(), vm)?;
        let code = rustpython_vm::py_compile!(file = "boot.py", module_name = "python");
        let scope = Scope::with_builtins(None, self.boot.clone(), vm);
        vm.run_code_obj(vm.ctx.new_code(code), scope)?;
        Ok(())
    }

    fn program(
        &self,
        vm: &VirtualMachine,
        invocation: &Invocation,
        directory: bool,
    ) -> PyResult<Prepared> {
        Ok(match &invocation.program {
            Program::Command(code) => Prepared::Source(code.clone(), "<string>".to_owned()),
            Program::Stdin => decoded(host::input(), "<stdin>"),
            Program::File(path) if directory => self.directory_main(vm, path)?,
            Program::File(path) => match host::source(path) {
                // A zip archive runs its __main__ with the archive first on sys.path.
                Ok(source) if source.starts_with(b"PK\x03\x04") => Prepared::Source(
                    format!(
                        "__import__('sys').path.__setitem__(0, {path:?}); __import__('runpy')._run_module_as_main('__main__', False)\n"
                    ),
                    path.clone(),
                ),
                // CPython 3.12 runs a .pyc (by its name, or by the magic number's
                // b"\r\n" on a file that is no UTF-8 text) as compiled code, and
                // refuses one whose magic number is another version's.
                Ok(source)
                    if path.ends_with(".pyc")
                        || (source.get(2..4) == Some(&b"\r\n"[..])
                            && std::str::from_utf8(&source).is_err()) =>
                {
                    self.compiled(vm, path, source)?
                }
                Ok(source) => decoded(source, path),
                Err(error) => {
                    host::print(&format!(
                        "python3: can't open file {path:?}: {}\n",
                        host::text(&error)
                    ));
                    Prepared::Exited(2)
                }
            },
            Program::Module(module) => Prepared::Module(module.clone()),
            Program::Version => {
                let version = vm.sys_module.get_attr("version", vm)?.str(vm)?.to_string();
                let number = version.split(' ').next().unwrap_or_default();
                host::say(&format!("Python {number}\n"));
                Prepared::Exited(0)
            }
        })
    }

    fn directory_main(&self, vm: &VirtualMachine, path: &str) -> PyResult<Prepared> {
        let main = format!("{}/__main__.py", path.trim_end_matches('/'));
        Ok(match host::source(&main) {
            Ok(source) => decoded(source, &main),
            Err(_) => {
                let shown = vm
                    .import("os", 0)?
                    .get_attr("path", vm)?
                    .get_attr("abspath", vm)?
                    .call((vm.ctx.new_str(path),), vm)?
                    .str(vm)?
                    .to_string();
                host::print(&format!(
                    "python3: can't find '__main__' module in {shown:?}
"
                ));
                Prepared::Exited(1)
            }
        })
    }

    fn compiled(&self, vm: &VirtualMachine, path: &str, source: Vec<u8>) -> PyResult<Prepared> {
        let magic = vm
            .import("importlib.util", 0)?
            .get_attr("util", vm)?
            .get_attr("MAGIC_NUMBER", vm)?;
        let magic = magic
            .downcast_ref::<PyBytes>()
            .map(|magic| magic.as_bytes().to_vec())
            .unwrap_or_default();
        Ok(if source.len() < 4 || source[..4] != magic[..] {
            host::print("RuntimeError: Bad magic number in .pyc file\n");
            Prepared::Exited(1)
        } else {
            Prepared::Compiled(
                source.get(16..).unwrap_or_default().to_vec(),
                path.to_owned(),
            )
        })
    }

    fn call(&self, vm: &VirtualMachine, name: &str, args: impl IntoFuncArgs) -> PyResult {
        self.boot.get_item(name, vm)?.call(args, vm)
    }

    fn missing(&self, vm: &VirtualMachine, check: &str, of: PyObjectRef) -> Vec<Missing> {
        let found = self
            .call(vm, check, (of,))
            .and_then(|list| list.try_to_value::<Vec<String>>(vm));
        match found {
            Ok(found) => found
                .as_chunks::<3>()
                .0
                .iter()
                .map(|found| Missing {
                    module: found[0].clone(),
                    why: if found[1] == "standard" {
                        Why::Machine
                    } else {
                        Why::Package
                    },
                    error: found[2].clone(),
                })
                .collect(),
            Err(_) if vm.stopped.borrow().is_some() => Vec::new(),
            Err(error) => {
                vm.print_exception(&error);
                Vec::new()
            }
        }
    }

    fn exit(&self, vm: &VirtualMachine, error: Option<PyBaseExceptionRef>) -> i32 {
        let stopped = host::ending();
        vm.set_limiter(Some(PyRc::new(host::Budget) as PyRc<dyn Limiter>));
        // CPython flushes sys.stdout before it prints an uncaught exception, so a
        // traceback follows what the program printed before it in 2>&1 output.
        if error.is_some() && !stopped {
            let _ = self.call(vm, "_flush", ());
        }
        let code = match error {
            Some(_) if stopped => 0,
            Some(error) if error.fast_isinstance(vm.ctx.exceptions.keyboard_interrupt) => {
                vm.print_exception(&error);
                130
            }
            Some(error) => vm.handle_exit_exception(error),
            None => 0,
        };
        if let Err(error) = self.call(vm, "_finish", (stopped,))
            && vm.stopped.borrow().is_none()
        {
            vm.print_exception(&error);
        }
        // A process's status is one byte: sys.exit(256) ends with 0, sys.exit(-1)
        // with 255.
        (code & 0xff).cast_signed()
    }
}

pub fn run(run: Run, files: Rc<dyn Files>) -> io::Result<Ran> {
    Ok(Python::new()?.run(run, files))
}
