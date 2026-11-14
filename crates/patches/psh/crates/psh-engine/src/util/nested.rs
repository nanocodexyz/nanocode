//! `bash` and `sh` named in a command run in psh (D1410): a new shell, started afresh
//! as a new bash process would be (D1300), with psh's own command line
//! (`invocation::parse_args`). It sits in the lookup after functions and builtins and
//! before the utilities, so wrappers (`env`, `xargs`, `find -exec`, `timeout`) reach it,
//! and reports like a utility (`type`, `command -v`, `which`).
//!
//! It runs as a virtual process (D1411): a task of its own behind an [`ExtProc`],
//! started where `run_external_with` starts programs, so jobs, `$!`, `kill`, `wait` and
//! abandoning (a cancelled `timeout`) treat it as they treat a program. It has a `$$`
//! and a pending-signal mask of its own: a signal sent to it runs its trap or ends it.
//!
//! A path (`/bin/bash`, `./bash`), a hashed name and `command -p bash` run the program,
//! except the four Unix shell paths on Windows, where no such files exist. Options psh
//! does not implement run the real program from `PATH`, as utilities fall through
//! (D1082).

use std::future::Future;
use std::io;
use std::pin::Pin;
use std::rc::Rc;
use std::task::{Context, Poll};
use std::time::Duration;

use super::{LocalBoxFuture, Mode, UtilCtx, UtilEntry};
use crate::Input;
use crate::builtins::trap::{Trap, TrapKey};
use crate::fd::FdObj;
use crate::host::{ExitStatus, ExtProc, FileKind, OpenOptions, Signal, Spawned, error_text};
use crate::invocation::{Invocation, parse_args};
use crate::sched::Join;
use crate::state::{Core, NestedSigs, Sigs, Unwind, Vp};

/// Not in any registry: the engine looks it up itself (`super::lookup`), and
/// `super::run_with` starts it as a virtual process, never through `run`.
pub(crate) static ENTRY: UtilEntry = UtilEntry {
    names: &["bash", "sh"],
    mode: Mode::Always,
    destructive: false,
    run,
};

fn run<'a>(_ctx: &'a mut UtilCtx<'_>, _argv: &'a [Vec<u8>]) -> LocalBoxFuture<'a, i32> {
    Box::pin(async { 2 })
}

/// On Windows these mean psh, as they do in shebangs there.
const WINDOWS_PATHS: [&[u8]; 4] = [b"/bin/sh", b"/bin/bash", b"/usr/bin/sh", b"/usr/bin/bash"];

/// How long an abandoned shell has after its TERM before its task is dropped (KILL),
/// as the OS hosts give a program.
const GRACE: Duration = Duration::from_secs(2);

/// Whether `name` runs as a shell in psh here.
pub(crate) fn is_shell(vp: &Vp, name: &[u8]) -> bool {
    vp.core.nested_shells
        && (matches!(name, b"bash" | b"sh")
            || (cfg!(windows) && WINDOWS_PATHS.contains(&name))
            || vp.core.own_program.as_deref() == Some(name))
}

/// What a nested shell takes from the shell that starts it.
struct Caller {
    /// Its `$$`: the nested shell's `$PPID`.
    shell_pid: Option<u32>,
    /// Its `$BASHPID`: `exec bash` replaces that process, keeping its id as `$$`.
    bashpid: Option<u32>,
    /// Its `$PPID`, for `exec bash`.
    ppid: Option<Vec<u8>>,
    in_place: bool,
}

/// Starts `argv` (`bash …`) as a virtual process: a subshell of `vp` with the prefix
/// assignments exported, then (unless it falls through) a shell started afresh, as its
/// own task. `ignore`: the signals it starts with ignored (`trap '' SIG` passes on,
/// D1284). `in_place`: run by `exec`, it keeps the caller's `$$` and `SHLVL`.
pub(crate) async fn start(
    vp: &mut Vp,
    argv: Vec<Vec<u8>>,
    assigns: &[(String, Vec<u8>)],
    ignore: Vec<Signal>,
    in_place: bool,
    process: bool,
) -> Result<Spawned, Unwind> {
    let mut sub = crate::exec::enter(vp).await?;
    for (name, value) in assigns {
        let var = sub.vars.entry(name);
        var.set(value.clone());
        var.exported = true;
        sub.assigned_dynamic(name);
    }
    // Its own process: a job it runs in is the caller's business (a killed subshell's
    // children run on).
    sub.job = None;
    sub.exec_tail = None;
    sub.exec_now = false;
    sub.top = true;
    let sigs = Rc::new(NestedSigs::default());
    let own = (in_place || process) && vp.top && matches!(vp.sigs, Sigs::Shell);
    sub.sigs = match own {
        true => Sigs::Shell,
        false => Sigs::Nested(sigs.clone()),
    };
    let caller = Caller {
        shell_pid: vp.shell_pid,
        bashpid: vp.bashpid,
        ppid: vp.var("PPID").map(<[u8]>::to_vec),
        in_place,
    };
    // Its job id for `$!`, `kill` and `wait`; never its `$$` (D1413).
    let pid = vp.core.next_job_id();
    let watch = sigs.clone();
    let core = vp.core.clone();
    let (id, join) = vp.core.sched.spawn_with_id(async move {
        let status = shell(sub, argv, ignore, caller).await;
        let died = match own {
            true => core.died_of.get(),
            false => watch.died_of.get(),
        };
        match died {
            Some(sig) if status == 128 + sig => ExitStatus::Signaled(sig),
            _ => ExitStatus::Exited(status),
        }
    });
    let proc = NestedProc {
        core: vp.core.clone(),
        id,
        join: Some(join),
        sigs,
        pid,
        status: None,
        released: false,
    };
    Ok(Spawned {
        proc: Box::new(proc),
        pipes: Vec::new(),
    })
}

/// The shell's task: its command line, then its script.
async fn shell(mut sub: Vp, argv: Vec<Vec<u8>>, ignore: Vec<Signal>, caller: Caller) -> i32 {
    let (input, arg0, args, options, dash_c) = match parse_args(&argv[1..]) {
        Ok(Invocation::Command {
            source,
            arg0,
            args,
            options,
        }) => (
            Input::Text(source),
            arg0.unwrap_or_else(|| argv[0].clone()),
            args,
            options,
            true,
        ),
        Ok(Invocation::File {
            path,
            args,
            options,
        }) => match read_script(&sub, &argv[0], &path).await {
            Ok(source) => (Input::Text(source), path, args, options, false),
            Err(status) => return status,
        },
        Ok(Invocation::Version) => {
            let text = format!("psh {}\n", env!("CARGO_PKG_VERSION"));
            return match sub.fd(1) {
                Some(f) => i32::from(f.write_all(text.as_bytes()).await.is_err()),
                None => 1,
            };
        }
        // Read one command at a time as it runs (D1414): nothing to scan ahead.
        Ok(Invocation::Stdin { args, options }) => {
            (Input::Stdin, argv[0].clone(), args, options, false)
        }
        Ok(Invocation::Help) => return fall_through(&mut sub, argv, "--help: not supported").await,
        Err(msg) => return fall_through(&mut sub, argv, &msg).await,
    };
    // No process id to give (D1413): a script that reads one runs under the real
    // program, if there is one; else reading it fails.
    if let Input::Text(source) = &input
        && crate::pidscan::uses_pid(source)
        && let Some(status) = real_program(&mut sub, &argv).await
    {
        return status;
    }
    let shlvl = sub.var("SHLVL").map(<[u8]>::to_vec);
    crate::exec::start_afresh(&mut sub);
    if caller.in_place {
        // `exec bash`: the process it replaces goes on, and its id is the new `$$`;
        // bash lowers `SHLVL` before it execs, the new shell raises it again.
        // From a subshell, the process it replaces is the shell's child.
        let ppid = match caller.bashpid {
            Some(_) => caller.ppid,
            None => caller.shell_pid.map(|p| p.to_string().into_bytes()),
        };
        sub.shell_pid = caller.bashpid;
        sub.bashpid = caller.bashpid;
        set_ppid(&mut sub, ppid);
        if let (Some(value), Some(var)) = (shlvl, sub.vars.get_mut("SHLVL")) {
            var.value = crate::vars::Value::Str(value);
        }
    } else {
        sub.shell_pid = None;
        sub.bashpid = None;
        set_ppid(
            &mut sub,
            caller.shell_pid.map(|p| p.to_string().into_bytes()),
        );
    }
    // Signals ignored when a shell starts stay ignored (bash).
    if !ignore.is_empty() {
        let traps = Rc::make_mut(&mut sub.traps);
        for sig in ignore {
            traps.locked |= 1u64 << sig;
            traps.map.insert(
                TrapKey::Signal(sig),
                Trap {
                    action: Rc::from(&b""[..]),
                    active: true,
                },
            );
        }
    }
    sub.arg0 = arg0;
    sub.script_args = args.clone();
    sub.args = args;
    sub.opts.dash_c = dash_c;
    crate::builtins::apply_invocation_options(&mut sub, &options);
    let (status, _) = Box::pin(crate::run_script(sub, input)).await;
    status
}

/// `$PPID`, a readonly integer; unset when there is no such process.
fn set_ppid(sub: &mut Vp, value: Option<Vec<u8>>) {
    match value {
        Some(v) => {
            let ppid = sub.vars.entry("PPID");
            ppid.set(v);
            ppid.integer = true;
            ppid.readonly = true;
        }
        None => {
            sub.vars.remove("PPID");
        }
    }
}

/// An option psh lacks: the real program from `PATH`
/// runs, else the shell fails with status 2 naming psh.
async fn fall_through(sub: &mut Vp, argv: Vec<Vec<u8>>, why: &str) -> i32 {
    if let Some(status) = real_program(sub, &argv).await {
        return status;
    }
    let name = String::from_utf8_lossy(&argv[0]).into_owned();
    let text = format!("{name}: {why} (psh built-in {name})\n");
    sub.error_raw(text.as_bytes()).await;
    2
}

/// Runs the first program named `argv[0]` in `PATH` that is not psh itself (D1413):
/// its status, or `None` when there is none. Searched in a sandbox too: its host may
/// have a `bash`, and a miss is recorded. `$0` stays the name as written.
async fn real_program(sub: &mut Vp, argv: &[Vec<u8>]) -> Option<i32> {
    let name = &argv[0];
    let program = if sub.core.own_program.as_deref() == Some(&name[..]) {
        name.clone()
    } else if name.contains(&b'/') {
        // `/bin/bash` on Windows, which does not exist there.
        crate::builtins::hash::search(sub, name, None).await?
    } else {
        let path = sub
            .var("PATH")
            .map(<[u8]>::to_vec)
            .unwrap_or_else(|| crate::exec::DEFAULT_PATH.to_vec());
        let own = match sub.core.own_program.clone() {
            Some(p) => Some(sub.host().canonicalize(&p).await.unwrap_or(p)),
            None => None,
        };
        let mut found = None;
        for dir in sub.host().path_entries(&path) {
            let Some(p) = crate::builtins::hash::search(sub, name, Some(&dir)).await else {
                continue;
            };
            let p = sub.abs_path(&p);
            if let Some(own) = &own
                && sub
                    .host()
                    .canonicalize(&p)
                    .await
                    .unwrap_or_else(|_| p.clone())
                    == *own
            {
                continue;
            }
            found = Some(p);
            break;
        }
        found?
    };
    let mut argv = argv.to_vec();
    argv[0] = program;
    let opts = crate::exec::ExtOpts {
        arg0: Some(name.clone()),
        real: true,
        ..Default::default()
    };
    let r = crate::exec::run_external_with(sub, argv, Vec::new(), opts).await;
    // A program a signal ended: so did this shell, for the caller's report (D1303).
    if let (Some((sig, pid)), Sigs::Nested(n)) = (sub.core.fg_signal.take(), &sub.sigs) {
        n.died_of.set(Some(sig));
        n.real_pid.set(Some(pid));
    }
    let last = sub.status;
    Some(r.unwrap_or_else(|u| u.status(last)))
}

/// The script `bash FILE` runs: `FILE` from the cwd, else (no slash) from a directory
/// of `PATH`, as bash looks for it (it need not be executable). Errors as bash's: 127
/// for a missing file, 126 for one that cannot be read, a directory or a binary file.
async fn read_script(sub: &Vp, name: &[u8], path: &[u8]) -> Result<Vec<u8>, i32> {
    let shown = String::from_utf8_lossy(path).into_owned();
    let host = sub.host();
    let mut target = sub.abs_path(path);
    if !path.contains(&b'/')
        && host.stat(&target, true).await.is_err()
        && let Some(found) = search_path(sub, path).await
    {
        target = found;
    }
    let mut source = Vec::new();
    let read = match crate::exec::dev_fd(path) {
        // The shell's own descriptors (`bash /dev/stdin`, D1055).
        Some(n) => match sub.fd(n) {
            Some(f) => f.read_to_end(&mut source).await,
            None => Err(crate::fd::bad_fd()),
        },
        None => match host.open(&target, OpenOptions::read()).await {
            Ok(f) => FdObj::File(f).read_to_end(&mut source).await,
            Err(e) => Err(e),
        },
    };
    if let Err(e) = read {
        if matches!(host.stat(&target, true).await, Ok(m) if m.kind == FileKind::Dir) {
            return Err(refused(sub, &shown, "Is a directory").await);
        }
        let name = String::from_utf8_lossy(name);
        let text = format!("{name}: {shown}: {}\n", error_text(&e));
        sub.error_raw(text.as_bytes()).await;
        return Err(if e.kind() == io::ErrorKind::NotFound {
            127
        } else {
            126
        });
    }
    // bash's test, as for a file it runs itself (D1300): a NUL in the first line of the
    // first 80 bytes.
    let head = &source[..source.len().min(80)];
    let first_line = head.split(|&b| b == b'\n').next().unwrap_or(head);
    if first_line.contains(&0) {
        return Err(refused(sub, &shown, "cannot execute binary file").await);
    }
    Ok(source)
}

/// bash names the script twice for these: its `$0` is the file already. Status 126.
async fn refused(sub: &Vp, shown: &str, what: &str) -> i32 {
    sub.error_raw(format!("{shown}: {shown}: {what}\n").as_bytes())
        .await;
    126
}

/// The first regular file named `name` in a directory of the command's `PATH`.
async fn search_path(sub: &Vp, name: &[u8]) -> Option<Vec<u8>> {
    let path = sub
        .var("PATH")
        .map(<[u8]>::to_vec)
        .unwrap_or_else(|| crate::exec::DEFAULT_PATH.to_vec());
    for dir in sub.host().path_entries(&path) {
        let mut p = dir;
        if !p.is_empty() {
            p.push(b'/');
        }
        p.extend_from_slice(name);
        let p = sub.abs_path(&p);
        if matches!(sub.host().stat(&p, true).await, Ok(m) if m.kind == FileKind::File) {
            return Some(p);
        }
    }
    None
}

/// A running shell psh started for `bash`/`sh`, seen as a process (D1411).
struct NestedProc {
    core: Rc<Core>,
    /// Its task; the id is its own while `join` is not done.
    id: usize,
    join: Option<Join<ExitStatus>>,
    sigs: Rc<NestedSigs>,
    pid: u32,
    status: Option<ExitStatus>,
    released: bool,
}

impl ExtProc for NestedProc {
    fn poll_wait(&mut self, cx: &mut Context<'_>) -> Poll<ExitStatus> {
        if let Some(s) = self.status {
            return Poll::Ready(s);
        }
        let Some(join) = &mut self.join else {
            return Poll::Pending;
        };
        match Pin::new(join).poll(cx) {
            Poll::Ready(s) => {
                self.status = Some(s);
                Poll::Ready(s)
            }
            Poll::Pending => Poll::Pending,
        }
    }

    fn poll_stop(&mut self, cx: &mut Context<'_>) -> Poll<Signal> {
        self.sigs.poll_stop(cx)
    }

    fn signal(&mut self, sig: Signal) {
        if self.status.is_none() {
            if sig == crate::host::SIGCONT {
                self.sigs.cont.set(true);
            }
            self.sigs.raise(sig);
        }
    }

    fn release(mut self: Box<Self>) {
        self.released = true;
    }

    fn pid(&self) -> Option<u32> {
        Some(self.sigs.real_pid.get().unwrap_or(self.pid))
    }
}

impl Drop for NestedProc {
    /// Abandoned (a cancelled wait, `timeout`): TERM to it and, as to a program's
    /// process group, to what it waits for; its task is dropped after the grace if it
    /// still runs, as a program gets TERM, then KILL. Not while every task
    /// is being dropped (a cancel): its task is gone then, and nothing may spawn.
    fn drop(&mut self) {
        let Some(join) = self.join.take() else {
            return;
        };
        if self.released || self.status.is_some() || join.is_done() {
            return;
        }
        if !self.core.sched.has_task(self.id) {
            return;
        }
        self.sigs.group.set(true);
        self.sigs.raise(crate::host::SIGTERM);
        let sched = self.core.sched.clone();
        let mut sleep = self.core.host.sleep(GRACE);
        let id = self.id;
        let mut join = join;
        self.core.sched.spawn(async move {
            let ended = std::future::poll_fn(|cx| {
                if Pin::new(&mut join).poll(cx).is_ready() {
                    return Poll::Ready(true);
                }
                sleep.as_mut().poll(cx).map(|()| false)
            })
            .await;
            if !ended {
                sched.drop_task(id);
            }
        });
    }
}
