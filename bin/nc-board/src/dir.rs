use std::{
    collections::BTreeMap,
    fs::{self, File, OpenOptions},
    io::{self, Write as _},
    ops::Range,
    path::{Path, PathBuf},
    sync::atomic::{AtomicU64, Ordering},
    thread::{self, JoinHandle},
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::{mpsc, oneshot};
use nanocode_turnlog::{Image, ImageOptions, LogHost};

const ROWS: &str = "board.rows";
const LEGACY: &str = "board.log";
const TEMPORARY: &str = ".tmp-";
const JOBS_WAITING: usize = 1;

static MADE: AtomicU64 = AtomicU64::new(0);

pub struct DirLog {
    jobs: mpsc::Sender<Job>,
    scope: Box<str>,
    worker: Option<JoinHandle<()>>,
}

type Job = Box<dyn FnOnce(&mut Result<Rows, StoreError>) + Send>;

struct Rows {
    dir: PathBuf,
    legacy: Option<Image<File>>,
}

impl DirLog {
    pub fn open(dir: &Path, scope: &str) -> Result<Self, StoreError> {
        let (jobs, mut receiver) = mpsc::channel::<Job>(JOBS_WAITING);
        let dir = dir.to_path_buf();
        let worker = thread::Builder::new()
            .name("nc-board-dir".into())
            .spawn(move || {
                let mut rows = Rows::open(&dir);
                while let Some(job) = receiver.blocking_recv() {
                    job(&mut rows);
                }
            })
            .map_err(|error| failed(Path::new("worker"), &error))?;
        Ok(Self {
            jobs,
            scope: scope.into(),
            worker: Some(worker),
        })
    }

    fn run<T: Send + 'static>(
        &self,
        scope: &str,
        job: impl FnOnce(&mut Rows) -> Result<T, StoreError> + Send + 'static,
    ) -> StoreFuture<'_, Result<T, StoreError>> {
        let (reply, answer) = oneshot::channel();
        let job: Result<Job, StoreError> = if scope == &*self.scope {
            Ok(Box::new(move |rows: &mut Result<Rows, StoreError>| {
                let _ = reply.send(match rows {
                    Ok(rows) => job(rows),
                    Err(error) => Err(error.clone()),
                });
            }))
        } else {
            Err(StoreError::Backend(format!(
                "board directory holds scope {} and not {scope}",
                self.scope
            )))
        };
        Box::pin(async move {
            self.jobs.send(job?).await.map_err(|_| stopped())?;
            answer.await.map_err(|_| stopped())?
        })
    }
}

impl Drop for DirLog {
    fn drop(&mut self) {
        drop(std::mem::replace(
            &mut self.jobs,
            mpsc::channel(JOBS_WAITING).0,
        ));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl LogHost for DirLog {
    fn call_bound(&self) -> Option<std::time::Duration> {
        None
    }

    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.run(scope, move |rows| rows.scan(from, to))
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.run(scope, Rows::load)
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        let rows = rows.to_vec();
        self.run(scope, move |dir| {
            rows.iter()
                .try_for_each(|(seq, body)| dir.append(*seq, body))
        })
    }
}

impl Rows {
    fn open(dir: &Path) -> Result<Self, StoreError> {
        let rows = dir.join(ROWS);
        fs::create_dir_all(&rows).map_err(|error| failed(&rows, &error))?;
        Ok(Self {
            legacy: legacy(dir)?,
            dir: rows,
        })
    }

    fn load(&mut self) -> Result<Vec<(u64, String)>, StoreError> {
        let legacy = match &mut self.legacy {
            Some(image) => image.load()?,
            None => Vec::new(),
        };
        self.merged(legacy, 0..u64::MAX)
    }

    fn scan(&mut self, from: u64, to: u64) -> Result<Vec<(u64, String)>, StoreError> {
        if from >= to {
            return Ok(Vec::new());
        }
        let legacy = match &mut self.legacy {
            Some(image) => image.scan(from, to)?,
            None => Vec::new(),
        };
        self.merged(legacy, from..to)
    }

    fn merged(
        &self,
        legacy: Vec<(u64, String)>,
        range: Range<u64>,
    ) -> Result<Vec<(u64, String)>, StoreError> {
        let mut rows: BTreeMap<u64, String> = legacy.into_iter().collect();
        for seq in self.listed()?.into_iter().filter(|seq| range.contains(seq)) {
            let path = self.dir.join(seq.to_string());
            let bytes = fs::read(&path).map_err(|error| failed(&path, &error))?;
            let body = String::from_utf8(bytes).map_err(|error| {
                StoreError::Backend(format!("board row {}: {error}", path.display()))
            })?;
            if rows.insert(seq, body).is_some() {
                return Err(StoreError::Backend(format!(
                    "board row {seq} is in both {LEGACY} and {ROWS}"
                )));
            }
        }
        Ok(rows.into_iter().collect())
    }

    fn listed(&self) -> Result<Vec<u64>, StoreError> {
        let mut seqs = Vec::new();
        for entry in fs::read_dir(&self.dir).map_err(|error| failed(&self.dir, &error))? {
            let name = entry
                .map_err(|error| failed(&self.dir, &error))?
                .file_name();
            let name = name.to_string_lossy();
            if name.starts_with(TEMPORARY) {
                continue;
            }
            match name.parse::<u64>() {
                Ok(seq) if seq.to_string() == name => seqs.push(seq),
                _ => {
                    return Err(StoreError::Backend(format!(
                        "{} holds {name}, which is not a board row",
                        self.dir.display()
                    )));
                }
            }
        }
        Ok(seqs)
    }

    fn append(&mut self, seq: u64, body: &str) -> Result<(), StoreError> {
        let taken = match &mut self.legacy {
            Some(image) => !image.scan(seq, seq + 1)?.is_empty(),
            None => false,
        };
        if taken {
            return Err(StoreError::Fenced);
        }
        let made = MADE.fetch_add(1, Ordering::Relaxed);
        let temporary = self
            .dir
            .join(format!("{TEMPORARY}{}-{made}", std::process::id()));
        let landed = self.link(&temporary, seq, body);
        let removed = fs::remove_file(&temporary);
        match landed {
            Ok(true) => File::open(&self.dir)
                .and_then(|dir| dir.sync_all())
                .map_err(|error| failed(&self.dir, &error)),
            Ok(false) => {
                removed.map_err(|error| failed(&temporary, &error))?;
                Err(StoreError::Fenced)
            }
            Err(error) => Err(error),
        }
    }

    fn link(&self, temporary: &Path, seq: u64, body: &str) -> Result<bool, StoreError> {
        let mut file = OpenOptions::new()
            .create(true)
            .truncate(true)
            .write(true)
            .open(temporary)
            .map_err(|error| failed(temporary, &error))?;
        file.write_all(body.as_bytes())
            .and_then(|()| file.sync_all())
            .map_err(|error| failed(temporary, &error))?;
        let row = self.dir.join(seq.to_string());
        match fs::hard_link(temporary, &row) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(false),
            Err(error) => Err(failed(&row, &error)),
        }
    }
}

fn legacy(dir: &Path) -> Result<Option<Image<File>>, StoreError> {
    let data = dir.join(LEGACY);
    let mut index = data.as_os_str().to_owned();
    index.push(".index");
    let opened = |path: &Path| File::open(path).map_err(|error| failed(path, &error));
    match fs::symlink_metadata(&data) {
        Ok(_) => Image::open(
            opened(&data)?,
            opened(Path::new(&index))?,
            ImageOptions::default(),
        )
        .map(Some),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(failed(&data, &error)),
    }
}

#[cold]
fn failed(path: &Path, error: &io::Error) -> StoreError {
    StoreError::Backend(format!("board directory {}: {error}", path.display()))
}

#[cold]
fn stopped() -> StoreError {
    StoreError::Backend("board directory worker stopped".to_owned())
}
