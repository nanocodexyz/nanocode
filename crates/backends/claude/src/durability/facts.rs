use std::{future::Future, pin::Pin, sync::Arc};

use nanocode_agent_core::{NanocodeError, Result};
use nanocode_durability::facts::{FactLog, Folded, OwnedFact, commit_fact, fold_facts};
use nanocode_sync::{future, mpsc, oneshot};

use super::fold::ClaudeFold;

#[cfg(not(target_family = "wasm"))]
type Work<'a, R> = Pin<Box<dyn Future<Output = R> + Send + 'a>>;
#[cfg(target_family = "wasm")]
type Work<'a, R> = Pin<Box<dyn Future<Output = R> + 'a>>;

type Job = Box<dyn for<'a> FnOnce(&'a mut Owner) -> Work<'a, ()> + Send>;

#[doc(hidden)]
pub struct ClaudeFacts {
    jobs: mpsc::UnboundedSender<Job>,
}

struct Owner {
    log: Arc<dyn FactLog>,
    folded: Option<Folded<ClaudeFold>>,
    synced: Vec<u64>,
}

fn store_error(error: nanocode_durability::StoreError) -> NanocodeError {
    nanocode_durability::agent_store_error(error)
}

fn stopped() -> NanocodeError {
    NanocodeError::InvalidRequest("the Claude fact log's owning task stopped".into())
}

async fn serve(mut owner: Owner, mut jobs: mpsc::UnboundedReceiver<Job>) {
    while let Some(job) = jobs.recv().await {
        job(&mut owner).await;
    }
    if let Some(folded) = owner.folded.take()
        && let Ok(snapshot) = folded.snapshot()
    {
        owner.log.remember(snapshot);
    }
}

#[cfg(not(target_family = "wasm"))]
fn spawn(owner: Owner) -> Result<mpsc::UnboundedSender<Job>> {
    let runtime = nanocode_platform::Handle::try_current()
        .map_err(|error| NanocodeError::InvalidRequest(error.to_string()))?;
    let (jobs, received) = mpsc::unbounded_channel();
    runtime.spawn(serve(owner, received));
    Ok(jobs)
}

#[cfg(target_family = "wasm")]
fn spawn(owner: Owner) -> Result<mpsc::UnboundedSender<Job>> {
    let (jobs, received) = mpsc::unbounded_channel();
    nanocode_platform::spawn_local(serve(owner, received));
    Ok(jobs)
}

impl Owner {
    async fn taken(&mut self) -> Result<Folded<ClaudeFold>> {
        if let Some(folded) = self.folded.take() {
            return Ok(folded);
        }
        if crate::sim::mutated(crate::sim::Mutation::AbandonedCommitPoisons) {
            return Err(NanocodeError::InvalidRequest(
                "the Claude fact log could not be refolded after an abandoned commit".into(),
            ));
        }
        self.synced.clear();
        fold_facts::<ClaudeFold>(&*self.log)
            .await
            .map_err(store_error)
    }

    async fn read<R>(&mut self, read: impl FnOnce(&ClaudeFold) -> R) -> Result<R> {
        let folded = self.taken().await?;
        let value = read(folded.state());
        self.folded = Some(folded);
        Ok(value)
    }

    async fn commit(&mut self, fact: OwnedFact<super::ClaudeDecision>) -> Result<u64> {
        let folded = self.taken().await?;
        match commit_fact(&*self.log, folded, fact).await {
            Ok((folded, seq)) => {
                self.folded = Some(folded);
                Ok(seq)
            }
            Err(error) => {
                self.synced.clear();
                Err(store_error(error))
            }
        }
    }

    async fn refresh(&mut self) -> Result<()> {
        let folded = fold_facts::<ClaudeFold>(&*self.log)
            .await
            .map_err(store_error)?;
        self.synced.clear();
        self.folded = Some(folded);
        Ok(())
    }
}

impl ClaudeFacts {
    #[doc(hidden)]
    pub fn open(log: Arc<dyn FactLog>) -> Result<Self> {
        let jobs = spawn(Owner {
            log,
            folded: None,
            synced: Vec::new(),
        })?;
        Ok(Self { jobs })
    }

    async fn run<R: Send + 'static>(
        &self,
        work: impl for<'a> FnOnce(&'a mut Owner) -> Work<'a, R> + Send + 'static,
    ) -> Result<R> {
        let (mut reply, replied) = oneshot::channel();
        let job: Job = Box::new(move |owner| {
            Box::pin(async move {
                if reply.is_closed() {
                    return;
                }
                let work = work(owner);
                let output = {
                    let abandoned = Box::pin(reply.closed());
                    match future::select(work, abandoned).await {
                        future::Either::Left((output, _)) => Some(output),
                        future::Either::Right(_) => None,
                    }
                };
                if let Some(output) = output {
                    drop(reply.send(output));
                }
            })
        });
        self.jobs.send(job).map_err(|_| stopped())?;
        replied.await.map_err(|_| stopped())
    }

    #[doc(hidden)]
    pub async fn read<R: Send + 'static>(
        &self,
        read: impl FnOnce(&ClaudeFold) -> R + Send + 'static,
    ) -> Result<R> {
        self.run(move |owner| Box::pin(owner.read(read))).await?
    }

    #[doc(hidden)]
    pub async fn seq(&self) -> Result<u64> {
        self.run(|owner| {
            Box::pin(async move {
                let folded = owner.taken().await?;
                let seq = folded.seq();
                owner.folded = Some(folded);
                Ok(seq)
            })
        })
        .await?
    }

    #[doc(hidden)]
    pub async fn commit(&self, fact: OwnedFact<super::ClaudeDecision>) -> Result<u64> {
        self.run(move |owner| Box::pin(owner.commit(fact))).await?
    }

    pub(crate) async fn refresh(&self) -> Result<()> {
        self.run(|owner| Box::pin(owner.refresh())).await?
    }

    pub(crate) async fn take_synced(&self) -> Result<Vec<u64>> {
        self.run(|owner| {
            Box::pin(async move {
                let synced = std::mem::take(&mut owner.synced);
                if owner.folded.is_some() {
                    synced
                } else {
                    Vec::new()
                }
            })
        })
        .await
    }

    pub(crate) async fn put_synced(&self, synced: Vec<u64>) -> Result<()> {
        self.run(move |owner| Box::pin(async move { owner.synced = synced }))
            .await
    }
}
