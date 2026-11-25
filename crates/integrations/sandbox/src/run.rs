use crate::{
    bubblewrap::Bubblewrap,
    budget::{Admitted, Budget, Busy, Stopped},
    cgroup::{self, Usage},
    confine,
    egress::{Allowlist, Egress, Limits, Secret, Tls, tailnet_host},
};
use nanocode_executor::JoinHandle;
use nanocode_executor::process::{Child, Command};
use nanocode_executor::{UnixStream, net::unix::Pipe};
use nanocode_io::{AsyncRead, AsyncReadExt as _, AsyncWrite, AsyncWriteExt as _};
use nanocode_platform::{Elapsed, timeout};
use nanocode_sync::Signal;
use nanocode_sync::future::BoxFuture;
use std::{
    collections::VecDeque,
    io,
    iter::once,
    net::SocketAddr,
    os::fd::{OwnedFd, RawFd},
    path::{Path, PathBuf},
    process::{ExitStatus, Stdio},
    sync::Arc,
    time::Duration,
};

const PIPE: usize = 64 * 1024;
const SOCKET_NAME: usize = 16;
const PROXY_SOCKET: &str = "proxy.sock";

pub enum Confinement {
    Bubblewrap {
        bwrap: PathBuf,
        launcher: PathBuf,
        root: PathBuf,
        work: PathBuf,
    },
    Landlock {
        launcher: PathBuf,
    },
}

#[derive(Clone, Copy, Debug)]
pub struct Caps {
    pub output: usize,
    pub errors: usize,
    pub header: usize,
    pub body: usize,
}

pub struct Reach {
    pub tls: Arc<Tls>,
    pub tailnet: Option<SocketAddr>,
    pub limits: Limits,
}

pub struct Runner {
    confinement: Confinement,
    scratch: PathBuf,
    sockets: PathBuf,
    reach: Reach,
    caps: Caps,
    budget: Budget,
    cgroups: Option<Cgroups>,
}

pub struct Cgroups {
    pub tree: cgroup::Tree,
    pub limits: cgroup::Limits,
    pub settle: Duration,
}

impl Drop for Runner {
    fn drop(&mut self) {
        std::fs::remove_dir_all(&self.sockets).ok();
    }
}

#[derive(Clone, Copy)]
pub struct Read<'a> {
    pub path: &'a Path,
    pub name: &'a str,
}

pub struct Call<'a> {
    pub program: Read<'a>,
    pub reads: &'a [Read<'a>],
    pub input: Option<&'a str>,
    pub within: Duration,
    pub admitted: Option<&'a Admitted<'a>>,
    pub hosts: &'a [String],
    pub private_hosts: &'a [String],
    pub tailnet: bool,
    pub refusal: fn(&str) -> String,
    pub secrets: Vec<Secret>,
    pub host: Option<Arc<dyn Host>>,
}

pub enum Seen<'a> {
    Mounted {
        root: &'a Path,
        work: &'a Path,
        proxy: PathBuf,
        host: Option<RawFd>,
    },
    InPlace {
        work: &'a Path,
        port: u16,
        host: Option<RawFd>,
    },
}

impl Seen<'_> {
    #[must_use]
    pub fn read(&self, read: Read<'_>) -> PathBuf {
        match self {
            Self::Mounted { root, .. } => root.join(read.name),
            Self::InPlace { .. } => read.path.into(),
        }
    }
}

pub enum Ran {
    Exited {
        output: Vec<u8>,
        errors: Vec<u8>,
        denied: Vec<String>,
        unreachable: Vec<String>,
    },
    TimedOut,
    Overflowed,
    Stopped(Stopped),
}

pub struct Reply {
    pub header: Vec<u8>,
    pub parts: Vec<Arc<[u8]>>,
}

pub trait Host: Send + Sync {
    fn answer(&self, header: Vec<u8>, body: Vec<u8>) -> BoxFuture<'_, io::Result<Reply>>;

    fn busy(&self, busy: Busy) -> Vec<u8>;

    fn failed(&self, failure: io::Error);
}

impl Runner {
    pub fn new(
        confinement: Confinement,
        scratch: PathBuf,
        sockets: PathBuf,
        reach: Reach,
        caps: Caps,
        budget: Budget,
    ) -> io::Result<Self> {
        std::fs::create_dir(&sockets)?;
        Ok(Self {
            confinement,
            scratch,
            sockets,
            reach,
            caps,
            budget,
            cgroups: None,
        })
    }

    #[must_use]
    pub fn with_cgroups(mut self, cgroups: Cgroups) -> Self {
        self.cgroups = Some(cgroups);
        self
    }

    #[must_use]
    pub const fn cgroups(&self) -> Option<&Cgroups> {
        self.cgroups.as_ref()
    }

    #[must_use]
    pub const fn budget(&self) -> &Budget {
        &self.budget
    }

    pub async fn run(
        &self,
        call: Call<'_>,
        compose: impl FnOnce(&Seen<'_>, &mut Command),
    ) -> io::Result<Ran> {
        self.run_measured(call, compose).await.map(|(ran, _)| ran)
    }

    pub async fn run_measured(
        &self,
        call: Call<'_>,
        compose: impl FnOnce(&Seen<'_>, &mut Command),
    ) -> io::Result<(Ran, Option<Usage>)> {
        let Some(cgroups) = &self.cgroups else {
            let ran = self.run_in(call, compose, None).await?;
            return Ok((ran, None));
        };
        let name = nanocode_platform::uuid_v4().simple().to_string();
        let group = cgroups.tree.create(&name, &cgroups.limits)?;
        let ran = self.run_in(call, compose, Some(group.procs())).await;
        let usage = group.kill().and_then(|()| group.usage());
        let removed = group.remove(cgroups.settle).await;
        let ran = ran?;
        let usage = usage?;
        removed?;
        Ok((ran, Some(usage)))
    }

    async fn run_in(
        &self,
        mut call: Call<'_>,
        compose: impl FnOnce(&Seen<'_>, &mut Command),
        procs: Option<&Path>,
    ) -> io::Result<Ran> {
        let id = nanocode_platform::uuid_v4().simple().to_string();
        let mut cleanup = Cleanup {
            work: self.scratch.join(&id),
            group: None,
        };
        std::fs::create_dir_all(&cleanup.work)?;
        let socket = self.sockets.join(format!("{}.sock", &id[..SOCKET_NAME]));
        let channel = open_channel(call.host.take())?;
        let host = channel.as_ref().map(|_| confine::CHANNEL_DESCRIPTOR);
        let secrets = std::mem::take(&mut call.secrets);
        let allowlist = self.allowlist(&call, secrets);
        let egress = match self.confinement {
            Confinement::Bubblewrap { .. } => Egress::unix(&socket, allowlist)?,
            Confinement::Landlock { .. } => Egress::tcp(allowlist).await?,
        };
        let command = {
            let (mut command, seen) =
                self.command(&call, procs, host, &cleanup.work, &socket, &egress)?;
            compose(&seen, &mut command);
            command
        };
        let (mut child, channel, stdin) = self.spawn(command, channel, &mut cleanup)?;
        let overflow = Arc::new(Signal::new());
        let output = read_all(child.stdout.take(), self.caps.output, Arc::clone(&overflow));
        let errors = read_tail(child.stderr.take(), self.caps.errors);
        let watch = self.budget.watch(child.id(), call.admitted);
        let exchanged = exchange(&mut child, stdin, call.input, call.within, &overflow, watch);
        let ran = self.serving(exchanged, channel, call.admitted).await;
        cleanup.kill();
        let (wrote, exited) = match ran {
            Ok(ran) => ran,
            Err(stopped) => return Ok(Ran::Stopped(stopped)),
        };
        wrote?;
        let Ok(exited) = exited else {
            return Ok(Ran::TimedOut);
        };
        let Some(exited) = exited else {
            child.start_kill().ok();
            return Ok(Ran::Overflowed);
        };
        exited?;
        let (output, errors) = (output.await??, errors.await??);
        drop(cleanup);
        let Some(output) = output else {
            return Ok(Ran::Overflowed);
        };
        Ok(Ran::Exited {
            output,
            errors,
            denied: egress.denied(),
            unreachable: egress.unreachable(),
        })
    }

    fn allowlist(&self, call: &Call<'_>, secrets: Vec<Secret>) -> Allowlist {
        Allowlist {
            hosts: call
                .hosts
                .iter()
                .filter(|host| call.tailnet || !tailnet_host(host))
                .cloned()
                .collect(),
            private_hosts: call.private_hosts.to_vec(),
            secrets,
            tls: Arc::clone(&self.reach.tls),
            tailnet: self.reach.tailnet.filter(|_| call.tailnet),
            refusal: call.refusal,
            limits: self.reach.limits,
        }
    }

    fn command<'a>(
        &'a self,
        call: &Call<'_>,
        procs: Option<&Path>,
        host: Option<RawFd>,
        scratch: &'a Path,
        socket: &Path,
        egress: &Egress,
    ) -> io::Result<(Command, Seen<'a>)> {
        let reads = || once(call.program).chain(call.reads.iter().copied());
        match &self.confinement {
            Confinement::Bubblewrap {
                bwrap,
                launcher,
                root,
                work,
            } => {
                let mut sandbox = Bubblewrap::new(bwrap, scratch, work);
                for Read { path, name } in reads() {
                    sandbox.read(path, &root.join(name));
                }
                let proxy = root.join(PROXY_SOCKET);
                sandbox.bind(socket, &proxy);
                let program = root.join(call.program.name);
                let child = match (host, procs) {
                    (None, None) => sandbox.command(program),
                    (host, procs) => {
                        sandbox.command_joined(launcher, procs, host.is_some(), program)
                    }
                };
                let seen = Seen::Mounted {
                    root,
                    work,
                    proxy,
                    host,
                };
                Ok((child, seen))
            }
            Confinement::Landlock { launcher } => {
                let port = egress.port();
                let policy = confine::Policy {
                    read: reads().map(|read| read.path.into()).collect(),
                    write: scratch.to_path_buf(),
                    port,
                };
                let child =
                    policy.command_joined(launcher, procs, host.is_some(), call.program.path)?;
                let seen = Seen::InPlace {
                    work: scratch,
                    port,
                    host,
                };
                Ok((child, seen))
            }
        }
    }

    fn spawn(
        &self,
        mut command: Command,
        channel: Option<OpenChannel>,
        cleanup: &mut Cleanup,
    ) -> io::Result<(
        Child,
        Option<HostStream>,
        Box<dyn AsyncWrite + Send + Unpin>,
    )> {
        command.stdout(Stdio::piped()).stderr(Stdio::piped());
        let (channel, input) = match channel {
            Some((stream, host, program, input)) => {
                command.stdin(Stdio::from(program));
                (Some((stream, host)), Some(input))
            }
            None => {
                command.stdin(Stdio::piped());
                (None, None)
            }
        };
        let mut child = command.spawn()?;
        drop(command);
        if let Confinement::Landlock { .. } = self.confinement {
            cleanup.group = child
                .id()
                .and_then(|id| i32::try_from(id).ok())
                .filter(|id| *id != 0);
        }
        let stdin: Box<dyn AsyncWrite + Send + Unpin> = match input {
            Some(input) => Box::new(input),
            None => Box::new(child.stdin.take().ok_or_else(|| missing("no stdin"))?),
        };
        Ok((child, channel, stdin))
    }

    async fn serving(
        &self,
        exchanged: impl Future<Output = Result<Exchanged, Stopped>>,
        channel: Option<HostStream>,
        admitted: Option<&Admitted<'_>>,
    ) -> Result<Exchanged, Stopped> {
        let Some((stream, host)) = channel else {
            return exchanged.await;
        };
        let base = admitted.map_or(0, Admitted::expected);
        let mut held = Vec::new();
        let charge = |bytes| self.budget.charge(&mut held, base, bytes);
        let serving = serve(stream, host.as_ref(), self.caps, charge);
        let mut exchanged = std::pin::pin!(exchanged);
        let mut serving = std::pin::pin!(serving);
        let mut served = false;
        loop {
            nanocode_sync::select! {
                ran = &mut exchanged => break ran,
                failure = &mut serving, if !served => {
                    served = true;
                    if let Err(failure) = failure {
                        host.failed(failure);
                    }
                }
            }
        }
    }
}

type OpenChannel = (UnixStream, Arc<dyn Host>, OwnedFd, Pipe);

type HostStream = (UnixStream, Arc<dyn Host>);

fn open_channel(host: Option<Arc<dyn Host>>) -> io::Result<Option<OpenChannel>> {
    let Some(host) = host else {
        return Ok(None);
    };
    let confine::Channel {
        host: stream,
        program,
        input,
    } = confine::channel()?;
    stream.set_nonblocking(true)?;
    let input = Pipe::from_owned_fd(OwnedFd::from(input))?;
    Ok(Some((UnixStream::from_std(stream)?, host, program, input)))
}

fn missing(what: &'static str) -> io::Error {
    io::Error::other(what)
}

struct Cleanup {
    work: PathBuf,
    group: Option<i32>,
}

impl Cleanup {
    fn kill(&mut self) {
        if let Some(group) = self.group.take() {
            nanocode_platform::unix::signal_group(group, nanocode_sys::libc::SIGKILL).ok();
        }
    }
}

impl Drop for Cleanup {
    fn drop(&mut self) {
        self.kill();
        std::fs::remove_dir_all(&self.work).ok();
    }
}

async fn serve(
    mut stream: UnixStream,
    host: &dyn Host,
    caps: Caps,
    mut charge: impl FnMut(usize) -> Result<(), Busy>,
) -> io::Result<()> {
    while let Some(header) = frame(&mut stream, caps.header).await? {
        let body = frame(&mut stream, caps.body)
            .await?
            .ok_or_else(|| missing("a frame without its body"))?;
        let Reply { header, parts } = host.answer(header, body).await?;
        let length: usize = parts.iter().map(|part| part.len()).sum();
        let (header, parts, length) = match length {
            0 => (header, parts, 0),
            length => match charge(length) {
                Ok(()) => (header, parts, length),
                Err(busy) => (host.busy(busy), Vec::new(), 0),
            },
        };
        stream.write_all(&prefix(header.len())?).await?;
        stream.write_all(&header).await?;
        stream.write_all(&prefix(length)?).await?;
        for part in &parts {
            stream.write_all(part).await?;
        }
    }
    Ok(())
}

fn prefix(length: usize) -> io::Result<[u8; 4]> {
    Ok(u32::try_from(length)
        .map_err(io::Error::other)?
        .to_be_bytes())
}

async fn frame(stream: &mut UnixStream, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let mut length = [0; 4];
    match stream.read_exact(&mut length).await {
        Ok(_) => {}
        Err(failure) if failure.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(failure) => return Err(failure),
    }
    let length = usize::try_from(u32::from_be_bytes(length)).map_err(io::Error::other)?;
    if length > limit {
        return Err(io::Error::other(format!("a host frame of {length} bytes")));
    }
    let mut part = vec![0; length];
    stream.read_exact(&mut part).await?;
    Ok(Some(part))
}

type Exchanged = (
    io::Result<()>,
    Result<Option<io::Result<ExitStatus>>, Elapsed>,
);

async fn exchange(
    child: &mut Child,
    mut stdin: impl AsyncWrite + Unpin,
    input: Option<&str>,
    within: Duration,
    overflow: &Signal,
    watch: impl Future<Output = Stopped>,
) -> Result<Exchanged, Stopped> {
    let ran = async {
        let wrote = match input {
            Some(input) => stdin.write_all(input.as_bytes()).await,
            None => Ok(()),
        };
        drop(stdin);
        let exited = timeout(within, async {
            nanocode_sync::select! {
                status = child.wait() => Some(status),
                () = overflow.wait() => None,
            }
        })
        .await;
        (wrote, exited)
    };
    nanocode_sync::select! {
        ran = ran => Ok(ran),
        stop = watch => Err(stop),
    }
}

type Reading<T> = JoinHandle<io::Result<T>>;

fn read_tail<R>(pipe: Option<R>, most: usize) -> Reading<Vec<u8>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    nanocode_platform::spawn_task(async move {
        let mut pipe = pipe.ok_or_else(|| missing("no pipe"))?;
        let mut tail = VecDeque::new();
        let mut chunk = vec![0; PIPE];
        loop {
            let read = pipe.read(&mut chunk).await?;
            if read == 0 {
                break;
            }
            tail.extend(&chunk[..read]);
            let excess = tail.len().saturating_sub(most);
            tail.drain(..excess);
        }
        Ok(tail.into())
    })
}

fn read_all<R>(pipe: Option<R>, most: usize, overflow: Arc<Signal>) -> Reading<Option<Vec<u8>>>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    nanocode_platform::spawn_task(async move {
        let mut bytes = Vec::new();
        let mut pipe = pipe.ok_or_else(|| missing("no pipe"))?;
        let limit = u64::try_from(most)
            .map_err(io::Error::other)?
            .saturating_add(1);
        pipe.take(limit).read_to_end(&mut bytes).await?;
        if bytes.len() > most {
            overflow.fire();
            return Ok(None);
        }
        Ok(Some(bytes))
    })
}
