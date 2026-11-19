use std::{
    fs::{File, OpenOptions, TryLockError},
    io,
    path::Path,
    sync::Arc,
    thread::{self, JoinHandle},
};

use nanocode_durability_core::{StoreError, StoreFuture};
use nanocode_sync::{mpsc, oneshot};

use crate::{
    host::{Compaction, Extension, LogHost},
    image::{Counters, FileIo, Image, ImageOptions, Medium, failed},
};

pub struct FileLog {
    counters: Arc<Counters>,
    jobs: mpsc::UnboundedSender<Job>,
    scope: Box<str>,
    worker: Option<JoinHandle<()>>,
}

#[derive(Clone, Copy)]
pub struct FileOptions<'a> {
    pub scope: &'a str,
    pub sync: bool,
}

type Job = Box<dyn FnOnce(&mut Result<Image<File>, StoreError>) + Send>;

impl FileLog {
    pub fn open(path: &Path, options: FileOptions<'_>) -> Result<Self, StoreError> {
        let counters = Arc::<Counters>::default();
        let shared = Arc::clone(&counters);
        let (jobs, mut receiver) = mpsc::unbounded_channel::<Job>();
        let (data, index) = files(path)?;
        let sync = options.sync;
        let worker = thread::Builder::new()
            .name("nanocode-turnlog-file".into())
            .spawn(move || {
                let mut image = Image::counted(data, index, ImageOptions { sync }, shared);
                while let Some(job) = receiver.blocking_recv() {
                    job(&mut image);
                }
            })
            .map_err(|error| failed("worker", error))?;
        Ok(Self {
            counters,
            jobs,
            scope: options.scope.into(),
            worker: Some(worker),
        })
    }

    pub fn io(&self) -> FileIo {
        self.counters.snapshot()
    }

    fn run<T: Send + 'static>(
        &self,
        scope: &str,
        job: impl FnOnce(&mut Image<File>) -> Result<T, StoreError> + Send + 'static,
    ) -> StoreFuture<'_, Result<T, StoreError>> {
        let (reply, answer) = oneshot::channel();
        let sent = if scope == &*self.scope {
            self.jobs
                .send(Box::new(
                    move |image: &mut Result<Image<File>, StoreError>| {
                        let _ = reply.send(match image {
                            Ok(image) => job(image),
                            Err(error) => Err(error.clone()),
                        });
                    },
                ))
                .map_err(|_| stopped())
        } else {
            Err(StoreError::Backend(format!(
                "turn log file holds scope {} and not {scope}",
                self.scope
            )))
        };
        Box::pin(async move {
            sent?;
            answer.await.map_err(|_| stopped())?
        })
    }
}

impl Drop for FileLog {
    fn drop(&mut self) {
        drop(std::mem::replace(
            &mut self.jobs,
            mpsc::unbounded_channel().0,
        ));
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

impl LogHost for FileLog {
    fn scan<'a>(
        &'a self,
        scope: &'a str,
        from: u64,
        to: u64,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.run(scope, move |image| image.scan(from, to))
    }

    fn load<'a>(
        &'a self,
        scope: &'a str,
    ) -> StoreFuture<'a, Result<Vec<(u64, String)>, StoreError>> {
        self.run(scope, Image::load)
    }

    fn append<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        let rows = rows.to_vec();
        self.run(scope, move |image| {
            image.append(
                &rows,
                &Compaction {
                    archived: &[],
                    dropped: 0..0,
                    superseded: &[],
                },
            )
        })
    }

    fn append_compacting<'a>(
        &'a self,
        scope: &'a str,
        rows: &'a [(u64, String)],
        compaction: Compaction<'a>,
    ) -> StoreFuture<'a, Result<(), StoreError>> {
        let rows = rows.to_vec();
        let archived = compaction.archived.to_vec();
        let dropped = compaction.dropped;
        let superseded = compaction.superseded.to_vec();
        self.run(scope, move |image| {
            image.append(
                &rows,
                &Compaction {
                    archived: &archived,
                    dropped,
                    superseded: &superseded,
                },
            )
        })
    }

    fn extend<'a>(
        &'a self,
        scope: &'a str,
        extension: Extension<'a>,
    ) -> Option<StoreFuture<'a, Result<(), StoreError>>> {
        let (cut, row, seq) = (extension.cut, extension.row, extension.seq);
        let tail = extension.tail.to_owned();
        Some(self.run(scope, move |image| {
            image.extend(&Extension {
                cut,
                row,
                seq,
                tail: &tail,
            })
        }))
    }
}

impl Medium for File {
    fn read_at(&self, offset: u64, buffer: &mut [u8]) -> io::Result<usize> {
        let mut read = 0;
        while read < buffer.len() {
            match read_file(self, &mut buffer[read..], offset + read as u64) {
                Ok(0) => break,
                Ok(count) => read += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(read)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.sync_data()
    }

    fn write_at(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()> {
        let mut written = 0;
        while written < bytes.len() {
            match write_file(self, &bytes[written..], offset + written as u64) {
                Ok(0) => return Err(io::ErrorKind::WriteZero.into()),
                Ok(count) => written += count,
                Err(error) if error.kind() == io::ErrorKind::Interrupted => {}
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }
}

fn files(path: &Path) -> Result<(File, File), StoreError> {
    let open = |path: &Path| {
        OpenOptions::new()
            .create(true)
            .read(true)
            .truncate(false)
            .write(true)
            .open(path)
            .map_err(|error| {
                StoreError::Backend(format!("turn log file {}: {error}", path.display()))
            })
    };
    let data = open(path)?;
    match data.try_lock() {
        Ok(()) => {}
        Err(TryLockError::WouldBlock) => {
            return Err(StoreError::Backend(format!(
                "turn log file {} is held by another FileLog, in this process or another: a file has one writer",
                path.display()
            )));
        }
        Err(TryLockError::Error(error)) => {
            return Err(StoreError::Backend(format!(
                "turn log file {} lock failed: {error}",
                path.display()
            )));
        }
    }
    let mut index = path.as_os_str().to_owned();
    index.push(".index");
    Ok((data, open(Path::new(&index))?))
}

#[cold]
fn stopped() -> StoreError {
    StoreError::Backend("turn log file worker stopped".to_owned())
}

#[cfg(unix)]
fn read_file(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::read_at(file, buffer, offset)
}

#[cfg(unix)]
fn write_file(file: &File, bytes: &[u8], offset: u64) -> io::Result<usize> {
    std::os::unix::fs::FileExt::write_at(file, bytes, offset)
}

#[cfg(windows)]
fn read_file(file: &File, buffer: &mut [u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_read(file, buffer, offset)
}

#[cfg(windows)]
fn write_file(file: &File, bytes: &[u8], offset: u64) -> io::Result<usize> {
    std::os::windows::fs::FileExt::seek_write(file, bytes, offset)
}

#[cfg(not(any(unix, windows)))]
fn read_file(_file: &File, _buffer: &mut [u8], _offset: u64) -> io::Result<usize> {
    Err(io::ErrorKind::Unsupported.into())
}

#[cfg(not(any(unix, windows)))]
fn write_file(_file: &File, _bytes: &[u8], _offset: u64) -> io::Result<usize> {
    Err(io::ErrorKind::Unsupported.into())
}
