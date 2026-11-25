mod cells;
mod host;
mod machine;
pub mod sim;
mod stream;
mod worker;

use std::{io, rc::Rc, sync::Arc, time::Duration};

use nanocode_shell::{Commands, Credentials, Fetch, FileSystem, Identity, Limits, Machine};
use nanocode_sync::{mpsc, oneshot};

pub use cells::WorkspaceHost;
pub use machine::Platform;
pub use nanocode_shell::{Changes, Ended, Kind, Limit, Ran};
pub use stream::Uploading;

use worker::Job;

const JOBS_HANDED_OVER: usize = 1;

pub type Make<T> = Arc<dyn Fn() -> Rc<T> + Send + Sync>;

#[derive(Clone)]
pub struct Config {
    pub files: Make<dyn FileSystem>,
    pub machine: Make<dyn Machine>,
    pub fetch: Option<Make<dyn Fetch>>,
    pub credentials: Option<Make<dyn Credentials>>,
    pub identity: Identity,
    pub limits: Limits,
    pub commands: Make<dyn Commands>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub without: Vec<Vec<u8>>,
    pub strict: bool,
    pub git: bool,
    pub slice: u32,
}

pub struct Command {
    pub script: String,
    pub cwd: Vec<u8>,
    pub env: Vec<(Vec<u8>, Vec<u8>)>,
    pub stdin: Option<Vec<u8>>,
    pub timeout: Option<Duration>,
}

#[derive(Debug)]
pub struct Finished {
    pub ran: Ran,
    pub timed_out: bool,
}

#[derive(Clone)]
pub struct Workspace {
    jobs: mpsc::Sender<Job>,
    env: Arc<[(Vec<u8>, Vec<u8>)]>,
    found: Arc<Found>,
}

pub(crate) struct Found {
    pub(crate) network: bool,
    pub(crate) commands: Vec<Vec<u8>>,
    pub(crate) without: Vec<Vec<u8>>,
    pub(crate) git: bool,
}

impl Workspace {
    pub fn start(config: Config) -> io::Result<Self> {
        let (jobs, received) = mpsc::channel(JOBS_HANDED_OVER);
        let found = worker::start(config, received)?;
        Ok(Self {
            jobs,
            env: Arc::from(Vec::new()),
            found: Arc::new(found),
        })
    }

    #[must_use]
    pub fn has(&self, command: &str) -> bool {
        let name = command.as_bytes();
        let off = self.found.without.iter().any(|off| off == name);
        if off || (!self.found.git && name == b"git") {
            return false;
        }
        nanocode_shell::has(name, self.found.network)
            || self.found.commands.iter().any(|known| known == name)
    }

    #[must_use]
    pub fn with_env(&self, env: Vec<(Vec<u8>, Vec<u8>)>) -> Self {
        let mut merged = self.env.to_vec();
        merged.extend(env);
        Self {
            jobs: self.jobs.clone(),
            env: Arc::from(merged),
            found: Arc::clone(&self.found),
        }
    }

    pub async fn read(&self, path: &[u8]) -> Result<Option<Vec<u8>>, String> {
        self.ask(|reply| Job::Read(path.to_vec(), reply)).await
    }

    pub async fn list(&self, path: &[u8]) -> Result<Option<Vec<(Vec<u8>, Kind)>>, String> {
        self.ask(|reply| Job::List(path.to_vec(), reply)).await
    }

    pub async fn write(&self, path: &[u8], data: &[u8]) -> Result<(), String> {
        self.ask(|reply| Job::Write(path.to_vec(), data.to_vec(), reply))
            .await
    }

    pub async fn remove(&self, path: &[u8]) -> Result<(), String> {
        self.ask(|reply| Job::Remove(path.to_vec(), reply)).await
    }

    pub async fn run(&self, mut command: Command) -> Result<Finished, String> {
        let mut env = self.env.to_vec();
        env.append(&mut command.env);
        command.env = env;
        self.ask(|reply| Job::Run(command, reply)).await
    }

    async fn ask<T>(
        &self,
        job: impl FnOnce(oneshot::Sender<Result<T, String>>) -> Job,
    ) -> Result<T, String> {
        let (reply, answer) = oneshot::channel();
        if self.jobs.send(job(reply)).await.is_err() {
            return Err(stopped());
        }
        answer.await.unwrap_or_else(|_| Err(stopped()))
    }
}

fn stopped() -> String {
    "the workspace's worker stopped".to_owned()
}
