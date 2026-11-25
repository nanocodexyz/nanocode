use std::{
    future::{Future, poll_fn},
    io,
    pin::Pin,
    rc::Rc,
    task::Poll,
};

use nanocode_bytes::Bytes;
use nanocode_digest::Sha256;
use nanocode_shell::{
    Commands, Config as ShellConfig, Credentials, Fetch, FileDownload, FileSystem, FileUpload,
    Identity, Kind, Limits, Machine, Remove, Reply as Answer, SHA256_BYTES, Script, Shell, Step,
    Write,
};
use nanocode_sync::{Stream, StreamExt, oneshot, stream::FuturesUnordered, unbounded};

use crate::{
    Command, Config, Finished, Found,
    sim::{self, Mutation},
};

pub type Reply<T> = oneshot::Sender<Result<T, String>>;

pub type Listing = Vec<(Vec<u8>, Kind)>;

pub enum Job {
    Read(Vec<u8>, Reply<Option<Vec<u8>>>),
    List(Vec<u8>, Reply<Option<Listing>>),
    Write(Vec<u8>, Vec<u8>, Reply<()>),
    Remove(Vec<u8>, Reply<()>),
    Run(Command, Reply<Finished>),
    Upload(
        Vec<u8>,
        [u8; SHA256_BYTES],
        unbounded::Receiver<Part>,
        Reply<()>,
    ),
    Download(Vec<u8>, Reply<Option<FileDownload>>),
}

pub enum Part {
    Chunk(Vec<u8>, Reply<()>),
    Seal(Reply<[u8; SHA256_BYTES]>),
    Commit(Reply<()>),
}

struct Buffered {
    files: Rc<dyn FileSystem>,
    path: Vec<u8>,
    expected: [u8; SHA256_BYTES],
    data: Vec<u8>,
    hasher: Sha256,
}

impl FileUpload for Buffered {
    fn push(&mut self, chunk: &[u8]) -> io::Result<()> {
        self.hasher.update(chunk);
        self.data.extend_from_slice(chunk);
        Ok(())
    }

    fn sha256(&self) -> [u8; SHA256_BYTES] {
        self.hasher.finalize()
    }

    fn commit(self: Box<Self>) -> Answer<()> {
        if self.sha256() != self.expected {
            return Answer::Now(Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "the uploaded bytes do not hash to the expected sha256",
            )));
        }
        self.files.write(&self.path, &self.data, Write::Replace)
    }
}

#[cfg(not(target_family = "wasm"))]
pub fn start(config: Config, jobs: unbounded::Receiver<Job>) -> io::Result<Found> {
    let handle = nanocode_platform::Handle::try_current().ok();
    let (told, found) = oneshot::channel();
    std::thread::Builder::new()
        .name("workspace".to_owned())
        .stack_size(nanocode_shell::ENGINE_STACK)
        .spawn(move || {
            let _entered = handle.as_ref().map(nanocode_platform::Handle::enter);
            let kept = Kept::new(config);
            if told.send(kept.offers()).is_ok() {
                nanocode_sync::block_on(serve(Rc::new(kept), jobs));
            }
        })?;
    nanocode_sync::block_on(found)
        .map_err(|_| io::Error::other("the workspace's worker stopped before it started"))
}

#[cfg(target_family = "wasm")]
pub fn start(config: Config, jobs: unbounded::Receiver<Job>) -> io::Result<Found> {
    let kept = Kept::new(config);
    let found = kept.offers();
    nanocode_platform::spawn_local(serve(Rc::new(kept), jobs));
    Ok(found)
}

struct Kept {
    files: Rc<dyn FileSystem>,
    machine: Rc<dyn Machine>,
    commands: Rc<dyn Commands>,
    fetch: Option<Rc<dyn Fetch>>,
    credentials: Option<Rc<dyn Credentials>>,
    identity: Identity,
    limits: Limits,
    env: Vec<(Vec<u8>, Vec<u8>)>,
    without: Vec<Vec<u8>>,
    strict: bool,
    git: bool,
    slice: u32,
}

impl Kept {
    fn new(config: Config) -> Self {
        Self {
            files: (config.files)(),
            machine: (config.machine)(),
            commands: (config.commands)(),
            fetch: config.fetch.as_ref().map(|fetch| fetch()),
            credentials: config.credentials.as_ref().map(|credentials| credentials()),
            identity: config.identity,
            limits: config.limits,
            env: config.env,
            without: config.without,
            strict: config.strict,
            git: config.git,
            slice: config.slice,
        }
    }

    fn offers(&self) -> Found {
        Found {
            network: self.fetch.is_some(),
            commands: self.commands.names(),
            without: self.without.clone(),
            git: self.git,
        }
    }
}

async fn serve(kept: Rc<Kept>, mut jobs: unbounded::Receiver<Job>) {
    let mut running: FuturesUnordered<Pin<Box<dyn Future<Output = ()>>>> = FuturesUnordered::new();
    let mut open = true;
    poll_fn(|context| {
        while open {
            match Pin::new(&mut jobs).poll_next(context) {
                Poll::Ready(Some(job)) => running.push(Box::pin(kept.clone().serve(job))),
                Poll::Ready(None) => open = false,
                Poll::Pending => break,
            }
        }
        while let Poll::Ready(Some(())) = Pin::new(&mut running).poll_next(context) {}
        match open || !running.is_empty() {
            true => Poll::Pending,
            false => Poll::Ready(()),
        }
    })
    .await;
}

impl Kept {
    async fn serve(self: Rc<Self>, job: Job) {
        match job {
            Job::Read(path, reply) => {
                let _ = reply.send(self.read(&path).await);
            }
            Job::List(path, reply) => {
                let _ = reply.send(self.list(&path).await);
            }
            Job::Write(path, data, reply) => {
                let _ = reply.send(self.write(&path, &data).await);
            }
            Job::Remove(path, reply) => {
                let _ = reply.send(self.remove(&path).await);
            }
            Job::Run(command, mut reply) => {
                let again = sim::mutated(Mutation::RunsTwice).then(|| Command {
                    script: command.script.clone(),
                    cwd: command.cwd.clone(),
                    env: command.env.clone(),
                    stdin: command.stdin.clone(),
                    timeout: command.timeout,
                });
                let mut finished = self.run(command, &mut reply).await;
                if let Some(again) = again {
                    finished = self.run(again, &mut reply).await;
                }
                let _ = reply.send(finished);
            }
            Job::Upload(path, sha256, parts, reply) => {
                self.upload(path, sha256, parts, reply).await
            }
            Job::Download(path, reply) => {
                let _ = reply.send(self.download(&path).await);
            }
        }
    }

    async fn found(&self, path: &[u8]) -> Result<bool, String> {
        match self.files.stat(path, true).get().await {
            Ok(found) if found.kind == Kind::Dir => Err(format!("{}: Is a directory", shown(path))),
            Ok(_) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(error) => Err(said(path, &error)),
        }
    }

    async fn download(&self, path: &[u8]) -> Result<Option<FileDownload>, String> {
        if !self.found(path).await? {
            return Ok(None);
        }
        match self.files.download(path, self.limits.file).get().await {
            Ok(download) => Ok(Some(download)),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => {
                let read = self.files.read(path, self.limits.file).get().await;
                let bytes = read.map_err(|error| said(path, &error))?;
                Ok(Some(FileDownload {
                    sha256: Sha256::digest(&bytes),
                    bytes,
                }))
            }
            Err(error) => Err(said(path, &error)),
        }
    }

    async fn opened(
        &self,
        path: &[u8],
        sha256: [u8; SHA256_BYTES],
    ) -> Result<Box<dyn FileUpload>, String> {
        self.parents(path).await?;
        match self.files.upload(path, sha256).get().await {
            Ok(upload) => Ok(upload),
            Err(error) if error.kind() == io::ErrorKind::Unsupported => Ok(Box::new(Buffered {
                files: self.files.clone(),
                path: path.to_vec(),
                expected: sha256,
                data: Vec::new(),
                hasher: Sha256::new(),
            })),
            Err(error) => Err(said(path, &error)),
        }
    }

    async fn upload(
        &self,
        path: Vec<u8>,
        sha256: [u8; SHA256_BYTES],
        mut parts: unbounded::Receiver<Part>,
        reply: Reply<()>,
    ) {
        let mut upload = match self.opened(&path, sha256).await {
            Ok(upload) => upload,
            Err(error) => {
                let _ = reply.send(Err(error));
                return;
            }
        };
        if reply.send(Ok(())).is_err() {
            return;
        }
        while let Some(part) = parts.next().await {
            match part {
                Part::Chunk(chunk, reply) => {
                    let pushed = upload.push(&chunk).map_err(|error| said(&path, &error));
                    let _ = reply.send(pushed);
                }
                Part::Seal(reply) => {
                    let _ = reply.send(Ok(upload.sha256()));
                }
                Part::Commit(reply) => {
                    let committed = upload.commit().get().await;
                    let _ = reply.send(committed.map_err(|error| said(&path, &error)));
                    return;
                }
            }
        }
    }

    async fn read(&self, path: &[u8]) -> Result<Option<Vec<u8>>, String> {
        if !self.found(path).await? {
            return Ok(None);
        }
        let data = self.files.read(path, self.limits.file).get().await;
        data.map(|data| match sim::mutated(Mutation::ReadTruncates) {
            true => Some(data[..data.len() / 2].to_vec()),
            false => Some(data.to_vec()),
        })
        .map_err(|error| said(path, &error))
    }

    async fn list(&self, path: &[u8]) -> Result<Option<Vec<(Vec<u8>, Kind)>>, String> {
        let names = match self.files.list(path).get().await {
            Ok(names) => names,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(said(path, &error)),
        };
        let mut entries = Vec::with_capacity(names.len());
        for name in names {
            let mut entry = path.to_vec();
            if !entry.ends_with(b"/") {
                entry.push(b'/');
            }
            entry.extend_from_slice(&name);
            let found = self.files.stat(&entry, false).get().await;
            entries.push((name, found.map_err(|error| said(&entry, &error))?.kind));
        }
        Ok(Some(entries))
    }

    async fn write(&self, path: &[u8], data: &[u8]) -> Result<(), String> {
        self.parents(path).await?;
        let written = self.files.write(path, data, Write::Replace).get().await;
        written.map_err(|error| said(path, &error))
    }

    async fn parents(&self, path: &[u8]) -> Result<(), String> {
        let mut parent = Vec::new();
        let names: Vec<&[u8]> = path.split(|byte| *byte == b'/').collect();
        let parents = match sim::mutated(Mutation::WriteSkipsParents) {
            true => 0,
            false => names.len().saturating_sub(1),
        };
        for name in names.iter().take(parents) {
            if name.is_empty() {
                continue;
            }
            parent.push(b'/');
            parent.extend_from_slice(name);
            match self.files.stat(&parent, true).get().await {
                Ok(found) if found.kind == Kind::Dir => {}
                Ok(_) => return Err(format!("{}: Not a directory", shown(&parent))),
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    let made = self.files.mkdir(&parent, 0o755).get().await;
                    made.map_err(|error| said(&parent, &error))?;
                }
                Err(error) => return Err(said(&parent, &error)),
            }
        }
        Ok(())
    }

    async fn remove(&self, path: &[u8]) -> Result<(), String> {
        let found = self.files.stat(path, false).get().await;
        let what = match found.map_err(|error| said(path, &error))?.kind {
            Kind::Dir => Remove::Dir,
            Kind::File | Kind::Symlink => Remove::File,
        };
        let removed = self.files.remove(path, what).get().await;
        removed.map_err(|error| said(path, &error))
    }

    async fn run(&self, command: Command, reply: &mut Reply<Finished>) -> Result<Finished, String> {
        let mut env = self.env.clone();
        env.extend(command.env);
        let config = ShellConfig {
            fs: self.files.clone(),
            commands: self.commands.clone(),
            machine: self.machine.clone(),
            identity: self.identity.clone(),
            limits: self.limits,
            env,
            cwd: command.cwd,
            without: self.without.clone(),
            strict: self.strict,
        };
        let mut shell = match self.git {
            true => Shell::new(config),
            false => Shell::without_git(config, None),
        };
        if let Some(fetch) = &self.fetch {
            shell.set_fetch(fetch.clone());
        }
        if let Some(credentials) = &self.credentials {
            shell.set_credentials(credentials.clone());
        }
        shell.start(Script {
            source: command.script.into_bytes(),
            name: b"bash".to_vec(),
            args: Vec::new(),
            options: Vec::new(),
            stdin: command.stdin.map(Bytes::from),
        });
        let mut timer = command.timeout.map(|timeout| self.machine.sleep(timeout));
        let mut timed_out = false;
        let mut stopped = false;
        poll_fn(|context| {
            if !stopped {
                timed_out = timer
                    .as_mut()
                    .is_some_and(|timer| timer.as_mut().poll(context).is_ready());
                if timed_out || reply.poll_closed(context).is_ready() {
                    shell.cancel();
                    stopped = true;
                }
            }
            shell.set_waker(context.waker().clone());
            match shell.step(self.slice) {
                Step::Done(ran) => Poll::Ready(Ok(Finished {
                    ran: *ran,
                    timed_out,
                })),
                Step::Blocked => Poll::Pending,
                Step::Yield => {
                    context.waker().wake_by_ref();
                    Poll::Pending
                }
                Step::Idle => Poll::Ready(Err("the shell had no script to run".to_owned())),
            }
        })
        .await
    }
}

fn shown(path: &[u8]) -> String {
    String::from_utf8_lossy(path).into_owned()
}

fn said(path: &[u8], error: &io::Error) -> String {
    format!("{}: {error}", shown(path))
}
