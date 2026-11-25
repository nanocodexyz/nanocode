use std::{num::NonZeroUsize, sync::Arc};

use nanocode_bytes::Bytes;
use nanocode_code_mode::{
    Tool, ToolContext, ToolDefinition, ToolOutput,
    code_mode::LocalCells,
    embedded::{HandCalls, HostFuture, nested_tool_input, tool_not_available},
};
use nanocode_hands::{ChunkError, Chunks, Fetch, GetError, PutError, Sha256Digest, Stored};
use serde_json::Value;

use crate::{
    Workspace,
    sim::{self, Mutation},
};

#[derive(Clone)]
pub struct WorkspaceHost {
    name: Arc<str>,
    root: Arc<[u8]>,
    files: Workspace,
    bytes_max: u64,
    tools: Arc<[(Arc<dyn Tool>, ToolDefinition)]>,
}

impl WorkspaceHost {
    #[must_use]
    pub fn new(
        name: impl Into<Arc<str>>,
        root: impl Into<Vec<u8>>,
        files: Workspace,
        bytes_max: u64,
        tools: impl IntoIterator<Item = Arc<dyn Tool>>,
    ) -> Self {
        Self {
            name: name.into(),
            root: Arc::from(root.into()),
            files,
            bytes_max,
            tools: tools
                .into_iter()
                .map(|tool| {
                    let definition = tool.definition();
                    (tool, definition)
                })
                .collect(),
        }
    }

    #[must_use]
    pub fn name(&self) -> &str {
        &self.name
    }

    #[must_use]
    pub fn cells(&self, cells: LocalCells) -> LocalCells {
        cells.host_tools(self.tools.iter().map(|(tool, _)| Arc::clone(tool)))
    }

    #[must_use]
    pub fn runs(&self, tool: &str) -> bool {
        self.tools
            .iter()
            .any(|(_, definition)| definition.name() == tool)
    }

    pub async fn call(&self, name: &str, input: Value, context: ToolContext<'_>) -> ToolOutput {
        let Some((tool, definition)) = self
            .tools
            .iter()
            .find(|(_, definition)| definition.name() == name)
        else {
            return tool_not_available(name);
        };
        let input = match nested_tool_input(definition, name, input) {
            Ok(input) => input,
            Err(output) => return output,
        };
        match tool.execute(input, context).await {
            Ok(output) => output,
            Err(error) => ToolOutput::error(error.to_string()),
        }
    }

    fn path(&self, path: &str) -> Vec<u8> {
        match path.starts_with('/') {
            true => path.as_bytes().to_vec(),
            false => {
                let mut joined = self.root.to_vec();
                if !joined.ends_with(b"/") {
                    joined.push(b'/');
                }
                joined.extend_from_slice(path.as_bytes());
                joined
            }
        }
    }

    pub async fn put(
        &self,
        path: &str,
        chunks: &mut dyn Chunks,
        sha256: Sha256Digest,
    ) -> Result<Stored, PutError> {
        let failed = |reason: String| PutError::Io {
            reason: reason.into(),
        };
        let uploading = self.files.upload(&self.path(path), sha256.0).await;
        let uploading = uploading.map_err(failed)?;
        let mut size_bytes: u64 = 0;
        while let Some(chunk) = chunks.next().await.map_err(PutError::Source)? {
            size_bytes += chunk.len() as u64;
            if size_bytes > self.bytes_max {
                return Err(PutError::TooLarge {
                    bytes_max: self.bytes_max,
                });
            }
            uploading.push(chunk).await.map_err(failed)?;
        }
        assert!(
            size_bytes <= self.bytes_max,
            "an accepted upload fits its byte limit"
        );
        let actual = Sha256Digest(uploading.sealed().await.map_err(failed)?);
        if actual != sha256 {
            return Err(PutError::ShaMismatch {
                expected: sha256,
                actual,
            });
        }
        let committed = uploading.commit().await;
        committed.map_err(|reason| PutError::Uncertain {
            reason: reason.into(),
        })?;
        Ok(Stored {
            size_bytes,
            sha256: actual,
        })
    }

    pub async fn get(
        &self,
        path: &str,
        chunk_bytes: NonZeroUsize,
    ) -> Result<Fetch<'static>, GetError> {
        let download = self.files.download(&self.path(path)).await;
        let download = download
            .map_err(|reason| GetError::Io {
                reason: reason.into(),
            })?
            .ok_or(GetError::NotFound)?;
        Ok(Fetch {
            size_bytes: download.bytes.len() as u64,
            sha256: Sha256Digest(download.sha256),
            chunks: Box::new(Slices {
                bytes: download.bytes,
                sent: 0,
                chunk_bytes,
            }),
        })
    }
}

struct Slices {
    bytes: Bytes,
    sent: usize,
    chunk_bytes: NonZeroUsize,
}

impl Chunks for Slices {
    fn next(&mut self) -> HostFuture<'_, Result<Option<Vec<u8>>, ChunkError>> {
        assert!(
            self.sent <= self.bytes.len(),
            "a download never sends past its bytes"
        );
        let end = self.bytes.len().min(self.sent + self.chunk_bytes.get());
        let chunk = (self.sent < end).then(|| self.bytes[self.sent..end].to_vec());
        self.sent = end;
        Box::pin(async move { Ok(chunk) })
    }
}

impl HandCalls for WorkspaceHost {
    fn here(&self, hand: &str) -> bool {
        hand == self.name.as_ref()
    }

    fn call<'a>(
        &'a self,
        hand: &'a str,
        name: &'a str,
        input: Value,
        context: ToolContext<'a>,
    ) -> HostFuture<'a, ToolOutput> {
        Box::pin(async move {
            match hand == self.name.as_ref() {
                true => Self::call(self, name, input, context).await,
                false => ToolOutput::error(format!(
                    "this workspace is {}, not the hand {hand}",
                    self.name
                )),
            }
        })
    }
}

impl nanocode_hands::Workspace for WorkspaceHost {
    fn runs(&self, tool: &str) -> bool {
        Self::runs(self, tool)
    }

    fn has(&self, command: &str) -> bool {
        (self.files.has(command) || sim::mutated(Mutation::HasEverything))
            && !sim::mutated(Mutation::HasNothing)
    }

    fn run<'a>(
        &'a self,
        _session: &'a str,
        tool: &'a str,
        input: Value,
        context: ToolContext<'a>,
    ) -> HostFuture<'a, ToolOutput> {
        Box::pin(Self::call(self, tool, input, context))
    }

    fn put<'a>(
        &'a self,
        _session: &'a str,
        path: &'a str,
        mut chunks: Box<dyn Chunks + 'a>,
        sha256: Sha256Digest,
    ) -> HostFuture<'a, Result<Stored, PutError>> {
        Box::pin(async move { Self::put(self, path, chunks.as_mut(), sha256).await })
    }

    fn get<'a>(
        &'a self,
        _session: &'a str,
        path: &'a str,
        chunk_bytes: NonZeroUsize,
    ) -> HostFuture<'a, Result<Fetch<'a>, GetError>> {
        Box::pin(async move {
            let fetched: Result<Fetch<'a>, GetError> = Self::get(self, path, chunk_bytes).await;
            fetched
        })
    }
}
