use super::{SUBSCRIPTION_STATE_FILE, SubscriptionAuth, SubscriptionCredentials};
use nanocode_http::layer::{BoxFuture, Sender};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub trait SubscriptionStorage: Send + Sync {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<u8>, String>>;
    fn write<'a>(&'a self, path: &'a Path, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), String>>;
    fn compare_and_swap<'a>(
        &'a self,
        path: &'a Path,
        expected: SubscriptionCredentials,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<bool, String>>;
}

#[derive(Clone)]
pub struct SubscriptionStore {
    path: PathBuf,
    storage: Arc<dyn SubscriptionStorage>,
}

impl fmt::Debug for SubscriptionStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SubscriptionStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl SubscriptionStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            storage: Arc::new(FileStorage),
        }
    }

    #[must_use]
    pub fn with_storage(mut self, storage: Arc<dyn SubscriptionStorage>) -> Self {
        self.storage = storage;
        self
    }

    pub fn locate(
        configured: Option<PathBuf>,
        lookup: &dyn Fn(&str) -> Option<String>,
    ) -> Result<Self, String> {
        let path = match configured {
            Some(path) => path,
            None => nanocode_home::AgentHome::from_vars(|name| lookup(name).map(Into::into))
                .map_err(|error| error.to_string())?
                .nanocode_home()
                .join(SUBSCRIPTION_STATE_FILE),
        };
        Ok(Self::new(path))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn load(&self) -> Result<SubscriptionCredentials, String> {
        let bytes = self.storage.read(&self.path).await?;
        let credentials: SubscriptionCredentials =
            serde_json::from_slice(&bytes).map_err(|_| {
                "Antigravity subscription credentials file is invalid; sign in again".to_owned()
            })?;
        credentials.validate()?;
        Ok(credentials)
    }

    pub async fn save(&self, credentials: &SubscriptionCredentials) -> Result<(), String> {
        credentials.validate()?;
        let bytes = serde_json::to_vec(credentials)
            .map_err(|_| "could not encode Antigravity credentials")?;
        self.storage.write(&self.path, bytes).await
    }

    pub(super) async fn replace(
        &self,
        expected: &SubscriptionCredentials,
        credentials: &SubscriptionCredentials,
    ) -> Result<(), String> {
        credentials.validate()?;
        let bytes = serde_json::to_vec(credentials)
            .map_err(|_| "could not encode Antigravity credentials")?;
        if self
            .storage
            .compare_and_swap(&self.path, expected.clone(), bytes)
            .await?
        {
            Ok(())
        } else {
            Err(
                "Antigravity credentials changed in another session; reload them or sign in again"
                    .into(),
            )
        }
    }

    pub async fn load_auth(&self, sender: Arc<dyn Sender>) -> Result<SubscriptionAuth, String> {
        Ok(SubscriptionAuth::new(self.load().await?, sender).with_store(self.clone()))
    }
}

struct FileStorage;

impl SubscriptionStorage for FileStorage {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(read(path.to_owned()))
    }

    fn write<'a>(&'a self, path: &'a Path, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            write::<SubscriptionCredentials>(path.to_owned(), bytes, None)
                .await
                .map(drop)
        })
    }
    fn compare_and_swap<'a>(
        &'a self,
        path: &'a Path,
        expected: SubscriptionCredentials,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(write(path.to_owned(), bytes, Some(expected)))
    }
}

#[cfg(not(target_family = "wasm"))]
pub(crate) async fn read(path: PathBuf) -> Result<Vec<u8>, String> {
    nanocode_platform::spawn_blocking(move || {
        use std::io::Read;
        #[cfg(unix)]
        let mut file = nanocode_home::private::open(&path).map_err(|error| error.to_string())?;
        #[cfg(not(unix))]
        let mut file = std::fs::File::open(&path).map_err(|error| error.to_string())?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)
            .map_err(|error| error.to_string())?;
        Ok(bytes)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(not(target_family = "wasm"))]
pub(crate) async fn write<T>(
    path: PathBuf,
    bytes: Vec<u8>,
    expected: Option<T>,
) -> Result<bool, String>
where
    T: serde::de::DeserializeOwned + PartialEq + Send + 'static,
{
    nanocode_platform::spawn_blocking(move || {
        use std::io::Write;
        let parent = path
            .parent()
            .filter(|parent| !parent.as_os_str().is_empty())
            .unwrap_or_else(|| Path::new("."));
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            std::fs::DirBuilder::new()
                .recursive(true)
                .mode(0o700)
                .create(parent)
                .map_err(|error| error.to_string())?;
        }
        #[cfg(not(unix))]
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
        let mut lock_path = path.as_os_str().to_os_string();
        lock_path.push(".lock");
        let mut options = std::fs::OpenOptions::new();
        options.read(true).write(true).create(true).truncate(false);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options
                .mode(0o600)
                .custom_flags(nanocode_sys::libc::O_NOFOLLOW);
        }
        let lock = options
            .open(PathBuf::from(lock_path))
            .map_err(|error| error.to_string())?;
        lock.lock().map_err(|error| error.to_string())?;
        if let Some(expected) = expected {
            let current_bytes = std::fs::read(&path).map_err(|error| error.to_string())?;
            let current: T = serde_json::from_slice(&current_bytes)
                .map_err(|_| "saved credentials changed or became invalid".to_owned())?;
            let proposed: T = serde_json::from_slice(&bytes)
                .map_err(|_| "invalid proposed credentials".to_owned())?;
            if current != expected && current != proposed {
                return Ok(false);
            }
        }
        let mut staged =
            nanocode_platform::TemporaryFile::new_in(parent).map_err(|error| error.to_string())?;
        staged
            .write_all(&bytes)
            .map_err(|error| error.to_string())?;
        staged
            .as_file()
            .sync_all()
            .map_err(|error| error.to_string())?;
        staged.persist(&path).map_err(|error| error.to_string())?;
        #[cfg(unix)]
        std::fs::File::open(parent)
            .and_then(|directory| directory.sync_all())
            .map_err(|error| error.to_string())?;
        Ok(true)
    })
    .await
    .map_err(|error| error.to_string())?
}

#[cfg(target_family = "wasm")]
pub(crate) async fn read(_path: PathBuf) -> Result<Vec<u8>, String> {
    Err("install a credential storage host to load Google credentials in WASM".into())
}

#[cfg(target_family = "wasm")]
pub(crate) async fn write<T>(
    _path: PathBuf,
    _bytes: Vec<u8>,
    _expected: Option<T>,
) -> Result<bool, String> {
    Err("install a credential storage host to save Google credentials in WASM".into())
}
