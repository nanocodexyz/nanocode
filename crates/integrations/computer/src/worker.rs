use crate::tool::STOPPED;
use nanocode_sync::{Actor, FutureExt, OnceCell, Slot, mpsc, oneshot};
use std::{any::Any, future::Future, panic::AssertUnwindSafe, pin::Pin, sync::Arc, time::Duration};

type Job<S> = Box<dyn FnOnce(&mut S) + Send>;

const JOBS_QUEUED: usize = 1;

pub(crate) struct Worker<S> {
    jobs: mpsc::Sender<Job<S>>,
}

pub(crate) enum Refused {
    Busy,
    Stopped,
}

pub(crate) type Finished<R> = oneshot::Receiver<Result<R, Box<dyn Any + Send>>>;

impl<S: Send + 'static> Worker<S> {
    pub(crate) fn spawn(name: &str, mut state: S) -> std::io::Result<Self> {
        let (jobs, mut received) = mpsc::channel::<Job<S>>(JOBS_QUEUED);
        std::thread::Builder::new()
            .name(name.to_owned())
            .spawn(move || {
                while let Some(job) = received.blocking_recv() {
                    job(&mut state);
                }
            })?;
        Ok(Self { jobs })
    }

    pub(crate) async fn run<R: Send + 'static>(
        self: &Arc<Self>,
        queued: Option<Duration>,
        job: impl FnOnce(&mut S) -> R + Send + 'static,
    ) -> Result<Finished<R>, Refused> {
        let (started, mut start) = oneshot::channel();
        let (finished, finish) = oneshot::channel();
        let kept = Arc::clone(self);
        let job: Job<S> = Box::new(move |state| {
            let _kept = kept;
            if started.send(()).is_err() {
                return;
            }
            let ran = std::panic::catch_unwind(AssertUnwindSafe(|| job(state)));
            drop(finished.send(ran));
        });
        let queue = async {
            self.jobs.send(job).await.map_err(|_| Refused::Stopped)?;
            (&mut start).await.map_err(|_| Refused::Stopped)
        };
        let started = match queued {
            Some(queued) => nanocode_platform::timeout(queued, queue).await,
            None => Ok(queue.await),
        };
        match started {
            Ok(Ok(())) => Ok(finish),
            Ok(Err(refused)) => Err(refused),
            Err(_) => {
                start.close();
                match start.try_recv() {
                    Ok(()) => Ok(finish),
                    Err(_) => Err(Refused::Busy),
                }
            }
        }
    }
}

type Work<'a, R> = Pin<Box<dyn Future<Output = R> + Send + 'a>>;

pub struct Owner<S> {
    parked: Slot<S>,
    actor: OnceCell<Actor<S>>,
}

impl<S: Send + 'static> Owner<S> {
    pub(crate) fn new(state: S) -> Self {
        Self {
            parked: Slot::new(Some(state)),
            actor: OnceCell::new(),
        }
    }

    pub(crate) async fn run<R, F>(&self, job: F) -> Result<R, String>
    where
        R: Send + 'static,
        F: for<'a> FnOnce(&'a mut S) -> Work<'a, R> + Send + 'static,
    {
        let actor = self
            .actor
            .get_or_try_init(|| async {
                let state = self.parked.take().ok_or_else(|| STOPPED.to_owned())?;
                Actor::spawn(state).map_err(|error| {
                    format!("Computer use could not start ({error}), so nothing ran.")
                })
            })
            .await?;
        let ran = actor
            .run(move |state| {
                Box::pin(AssertUnwindSafe(job(state)).catch_unwind())
                    as Work<'_, Result<R, Box<dyn Any + Send>>>
            })
            .await
            .map_err(|_| STOPPED.to_owned())?;
        match ran {
            Ok(output) => Ok(output),
            Err(panic) => std::panic::resume_unwind(panic),
        }
    }
}
