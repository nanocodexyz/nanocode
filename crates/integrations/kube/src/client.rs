use serde_json::Value;

use crate::error::Error;
use crate::resource::{Resource, check_field_manager};
use crate::transport::{APPLY_PATCH, Chunks, Request, Response, Transport, Verb};
use crate::types::object_resource_version;
use crate::watch::{Watch, decode_status};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Limits {
    pub response_bytes_max: u64,
    pub line_bytes_max: u64,
    pub events_buffered_max: usize,
}

#[derive(Clone, Debug, PartialEq)]
pub struct List {
    pub resource_version: String,
    pub items: Vec<Value>,
}

#[derive(Clone, Debug)]
pub struct Client<T> {
    transport: T,
    limits: Limits,
}

pub fn apply_request(
    resource: Resource,
    namespace: Option<&str>,
    name: &str,
    body: &Value,
    field_manager: &str,
    force: bool,
) -> Result<Request, Error> {
    check_field_manager(field_manager)?;
    let path = resource.item(namespace, name)?;
    let field = |key: &str| body.get(key).and_then(Value::as_str);
    if field("apiVersion") != Some(resource.api_version().as_str())
        || field("kind") != Some(resource.kind)
    {
        return Err(Error::config(format!(
            "apply body for {name} is not a {} {}",
            resource.api_version(),
            resource.kind
        )));
    }
    let metadata_name = body
        .get("metadata")
        .and_then(|metadata| metadata.get("name"))
        .and_then(Value::as_str);
    if metadata_name != Some(name) {
        return Err(Error::config(format!("apply body is not named {name}")));
    }
    let body = serde_json::to_vec(body).map_err(|error| Error::config(error.to_string()))?;
    let mut query = vec![("fieldManager", field_manager.to_owned())];
    if force {
        query.push(("force", "true".to_owned()));
    }
    Ok(Request {
        verb: Verb::Patch,
        path,
        query,
        content_type: Some(APPLY_PATCH),
        body,
    })
}

pub fn get_request(
    resource: Resource,
    namespace: Option<&str>,
    name: &str,
) -> Result<Request, Error> {
    Ok(Request {
        verb: Verb::Get,
        path: resource.item(namespace, name)?,
        query: Vec::new(),
        content_type: None,
        body: Vec::new(),
    })
}

pub fn list_request(resource: Resource, namespace: Option<&str>) -> Result<Request, Error> {
    Ok(Request {
        verb: Verb::Get,
        path: resource.collection(namespace)?,
        query: Vec::new(),
        content_type: None,
        body: Vec::new(),
    })
}

pub fn watch_request(
    resource: Resource,
    namespace: Option<&str>,
    resource_version: &str,
    timeout_seconds: u32,
) -> Result<Request, Error> {
    if resource_version.is_empty() {
        return Err(Error::config("watch needs the resourceVersion of a list"));
    }
    Ok(Request {
        verb: Verb::Get,
        path: resource.collection(namespace)?,
        query: vec![
            ("watch", "true".to_owned()),
            ("allowWatchBookmarks", "true".to_owned()),
            ("resourceVersion", resource_version.to_owned()),
            ("timeoutSeconds", timeout_seconds.to_string()),
        ],
        content_type: None,
        body: Vec::new(),
    })
}

impl<T: Transport> Client<T> {
    pub const fn new(transport: T, limits: Limits) -> Self {
        Self { transport, limits }
    }

    pub const fn transport(&self) -> &T {
        &self.transport
    }

    pub const fn limits(&self) -> Limits {
        self.limits
    }

    async fn read(&self, body: &mut T::Body) -> Result<Vec<u8>, Error> {
        let mut bytes = Vec::new();
        while let Some(chunk) = body.chunk().await? {
            if (bytes.len() + chunk.len()) as u64 > self.limits.response_bytes_max {
                return Err(Error::TooLarge {
                    bound: "response_bytes_max",
                    limit: self.limits.response_bytes_max,
                });
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    }

    async fn json(&self, request: Request) -> Result<Option<Value>, Error> {
        let Response { status, mut body } = self.transport.send(request).await?;
        let bytes = self.read(&mut body).await?;
        if status == 404 {
            return Ok(None);
        }
        let value: Value = serde_json::from_slice(&bytes).map_err(|error| {
            Error::malformed(format!("status {status} body is not JSON: {error}"))
        })?;
        if (200..300).contains(&status) {
            return Ok(Some(value));
        }
        match decode_status(&value) {
            Error::Status {
                code: 0,
                reason,
                message,
            } => Err(Error::Status {
                code: status,
                reason,
                message,
            }),
            error => Err(error),
        }
    }

    pub async fn apply(
        &self,
        resource: Resource,
        namespace: Option<&str>,
        name: &str,
        body: &Value,
        field_manager: &str,
        force: bool,
    ) -> Result<Value, Error> {
        let request = apply_request(resource, namespace, name, body, field_manager, force)?;
        self.json(request).await?.ok_or_else(|| Error::Status {
            code: 404,
            reason: "NotFound".to_owned(),
            message: format!("apply of {name} found no collection"),
        })
    }

    pub async fn get(
        &self,
        resource: Resource,
        namespace: Option<&str>,
        name: &str,
    ) -> Result<Option<Value>, Error> {
        self.json(get_request(resource, namespace, name)?).await
    }

    pub async fn list(&self, resource: Resource, namespace: Option<&str>) -> Result<List, Error> {
        let mut value = self
            .json(list_request(resource, namespace)?)
            .await?
            .ok_or_else(|| Error::Status {
                code: 404,
                reason: "NotFound".to_owned(),
                message: format!("no {} collection", resource.kind),
            })?;
        let resource_version = object_resource_version(&value)
            .ok_or_else(|| Error::malformed("list has no resourceVersion"))?
            .to_owned();
        let Some(Value::Array(items)) = value.get_mut("items").map(Value::take) else {
            return Err(Error::malformed("list has no items"));
        };
        Ok(List {
            resource_version,
            items,
        })
    }

    pub async fn watch(
        &self,
        resource: Resource,
        namespace: Option<&str>,
        resource_version: &str,
        timeout_seconds: u32,
    ) -> Result<Watch<T::Body>, Error> {
        let request = watch_request(resource, namespace, resource_version, timeout_seconds)?;
        let Response { status, mut body } = self.transport.send(request).await?;
        if (200..300).contains(&status) {
            return Ok(Watch::new(body, self.limits));
        }
        let bytes = self.read(&mut body).await?;
        let value: Value = serde_json::from_slice(&bytes)
            .map_err(|error| Error::malformed(format!("watch status {status}: {error}")))?;
        match decode_status(&value) {
            Error::Status {
                code: 0,
                reason,
                message,
            } => Err(Error::Status {
                code: status,
                reason,
                message,
            }),
            error => Err(error),
        }
    }
}

impl<B: Chunks> std::fmt::Debug for Watch<B> {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Watch")
            .field("buffered", &self.buffered())
            .finish_non_exhaustive()
    }
}
