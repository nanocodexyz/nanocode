use super::auth::{PlanAuth, PlanCredentials};
use crate::subscription::storage::{read, write};
use nanocode_http::layer::{BoxFuture, Sender};
use std::fmt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

pub const PLAN_STATE_FILE: &str = "google-ai-plan.json";

pub trait PlanStorage: Send + Sync {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<u8>, String>>;
    fn write<'a>(&'a self, path: &'a Path, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), String>>;
    fn compare_and_swap<'a>(
        &'a self,
        path: &'a Path,
        expected: PlanCredentials,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<bool, String>>;
}

#[derive(Clone)]
pub struct PlanStore {
    path: PathBuf,
    storage: Arc<dyn PlanStorage>,
}

impl fmt::Debug for PlanStore {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PlanStore")
            .field("path", &self.path)
            .finish_non_exhaustive()
    }
}

impl PlanStore {
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            storage: Arc::new(FileStorage),
        }
    }

    #[must_use]
    pub fn with_storage(mut self, storage: Arc<dyn PlanStorage>) -> Self {
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
                .join(PLAN_STATE_FILE),
        };
        Ok(Self::new(path))
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub async fn load(&self) -> Result<PlanCredentials, String> {
        let bytes = self.storage.read(&self.path).await?;
        let credentials: PlanCredentials = serde_json::from_slice(&bytes).map_err(|_| {
            "the Google AI plan credentials file is invalid; sign in again".to_owned()
        })?;
        credentials.validate()?;
        Ok(credentials)
    }

    pub async fn save(&self, credentials: &PlanCredentials) -> Result<(), String> {
        credentials.validate()?;
        let bytes = serde_json::to_vec(credentials)
            .map_err(|_| "could not encode the Google AI plan credentials")?;
        self.storage.write(&self.path, bytes).await
    }

    pub(super) async fn replace(
        &self,
        expected: &PlanCredentials,
        credentials: &PlanCredentials,
    ) -> Result<(), String> {
        credentials.validate()?;
        let bytes = serde_json::to_vec(credentials)
            .map_err(|_| "could not encode the Google AI plan credentials")?;
        if self
            .storage
            .compare_and_swap(&self.path, expected.clone(), bytes)
            .await?
        {
            Ok(())
        } else {
            Err("the Google AI plan credentials changed in another session; reload them or sign in again".into())
        }
    }

    pub async fn load_auth(&self, sender: Arc<dyn Sender>) -> Result<PlanAuth, String> {
        Ok(PlanAuth::new(self.load().await?, sender).with_store(self.clone()))
    }
}

struct FileStorage;

impl PlanStorage for FileStorage {
    fn read<'a>(&'a self, path: &'a Path) -> BoxFuture<'a, Result<Vec<u8>, String>> {
        Box::pin(read(path.to_owned()))
    }

    fn write<'a>(&'a self, path: &'a Path, bytes: Vec<u8>) -> BoxFuture<'a, Result<(), String>> {
        Box::pin(async move {
            write::<PlanCredentials>(path.to_owned(), bytes, None)
                .await
                .map(drop)
        })
    }

    fn compare_and_swap<'a>(
        &'a self,
        path: &'a Path,
        expected: PlanCredentials,
        bytes: Vec<u8>,
    ) -> BoxFuture<'a, Result<bool, String>> {
        Box::pin(write(path.to_owned(), bytes, Some(expected)))
    }
}
