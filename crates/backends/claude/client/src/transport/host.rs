use std::{future::Future, pin::Pin, time::Duration};

use nanocode_http::types::{HeaderMap, StatusCode};
use nanocode_sync::{Stream, StreamExt, TryStreamExt};

#[cfg(not(target_family = "wasm"))]
pub type HttpFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;
#[cfg(target_family = "wasm")]
pub type HttpFuture<'a, T> = Pin<Box<dyn Future<Output = T> + 'a>>;
#[cfg(not(target_family = "wasm"))]
pub type HttpBody = Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpError>> + Send>>;
#[cfg(target_family = "wasm")]
pub type HttpBody = Pin<Box<dyn Stream<Item = Result<Vec<u8>, HttpError>>>>;

pub trait HttpTransport: Send + Sync + 'static {
    fn post(&self, request: HttpRequest) -> HttpFuture<'_, Result<HttpResponse, HttpError>>;
}

pub struct HttpRequest {
    pub endpoint: String,
    pub headers: HeaderMap,
    pub body: String,
}

pub struct SenderTransport<S>(pub S);

impl<S: nanocode_http::layer::Sender + 'static> HttpTransport for SenderTransport<S> {
    fn post(&self, request: HttpRequest) -> HttpFuture<'_, Result<HttpResponse, HttpError>> {
        Box::pin(async move {
            let response = self
                .0
                .send(nanocode_http::layer::Request {
                    method: nanocode_http::types::Method::POST,
                    url: request.endpoint,
                    headers: request.headers,
                    body: request.body.into_bytes(),
                })
                .await?;
            Ok(HttpResponse::from_layer(response))
        })
    }
}

#[cfg(target_family = "wasm")]
#[derive(Clone)]
pub struct HttpClient(pub(super) std::sync::Arc<dyn HttpTransport>);

#[cfg(target_family = "wasm")]
impl HttpClient {
    pub fn new(transport: impl HttpTransport) -> Self {
        Self(std::sync::Arc::new(transport))
    }
}

#[derive(Debug)]
pub enum HttpError {
    Transport {
        detail: String,
        retryable: bool,
        timeout: bool,
        unsent: bool,
    },
    Json(serde_json::Error),
}

impl std::fmt::Display for HttpError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport { detail, .. } => write!(formatter, "{detail}"),
            Self::Json(value) => std::fmt::Display::fmt(value, formatter),
        }
    }
}

impl std::error::Error for HttpError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Json(value) => std::error::Error::source(value),
            _ => None,
        }
    }
}

impl From<serde_json::Error> for HttpError {
    fn from(source: serde_json::Error) -> Self {
        Self::Json(source)
    }
}

impl HttpError {
    #[must_use]
    pub const fn is_unsent(&self) -> bool {
        matches!(self, Self::Transport { unsent: true, .. })
    }
}

impl From<nanocode_http::layer::Error> for HttpError {
    fn from(error: nanocode_http::layer::Error) -> Self {
        Self::Transport {
            retryable: error.is_retryable(),
            timeout: error.is_timeout(),
            unsent: error.is_unsent(),
            detail: error.detail().to_owned(),
        }
    }
}

impl From<HttpError> for nanocode_http::layer::Error {
    fn from(error: HttpError) -> Self {
        match error {
            HttpError::Transport {
                detail,
                retryable,
                timeout,
                unsent,
            } => Self::new(detail)
                .retryable(retryable)
                .timeout(timeout)
                .unsent(unsent),
            HttpError::Json(error) => Self::new(error.to_string()),
        }
    }
}

pub struct HttpResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: HttpBody,
    events: Option<Box<dyn nanocode_http::layer::EventHook>>,
}

impl HttpResponse {
    pub fn new(status: StatusCode, headers: HeaderMap, body: HttpBody) -> Self {
        Self {
            status,
            headers,
            body,
            events: None,
        }
    }

    pub(crate) fn from_layer(response: nanocode_http::layer::Response) -> Self {
        let (status, headers, body, events) = response.into_parts();
        Self {
            status,
            headers,
            body: Box::pin(body.map_err(HttpError::from)),
            events,
        }
    }

    pub(crate) fn idle(self, timeout: Duration) -> Self {
        let body = nanocode_sync::stream::unfold(
            Some((self.body, IdleTimer::default())),
            move |state| async move {
                let (mut body, mut timer) = state?;
                match timer.within(timeout, body.next()).await {
                    Some(Some(chunk)) => Some((chunk, Some((body, timer)))),
                    Some(None) => None,
                    None => Some((Err(stalled(timeout)), None)),
                }
            },
        );
        Self {
            body: Box::pin(body),
            ..self
        }
    }

    pub(crate) fn into_layer(self) -> nanocode_http::layer::Response {
        nanocode_http::layer::Response::new(
            self.status,
            self.headers,
            Box::pin(self.body.map_err(nanocode_http::layer::Error::from)),
        )
    }

    pub(crate) fn take_events(&mut self) -> Option<Box<dyn nanocode_http::layer::EventHook>> {
        self.events.take()
    }

    pub const fn status(&self) -> StatusCode {
        self.status
    }

    pub const fn headers(&self) -> &HeaderMap {
        &self.headers
    }

    pub fn bytes_stream(self) -> HttpBody {
        self.body
    }

    async fn bytes(self) -> Result<Vec<u8>, HttpError> {
        self.body
            .try_fold(Vec::new(), |mut bytes, chunk| async move {
                bytes.extend(chunk);
                Ok(bytes)
            })
            .await
    }

    pub async fn text(self) -> Result<String, HttpError> {
        Ok(String::from_utf8_lossy(&self.bytes().await?).into_owned())
    }

    pub async fn json<T: serde::de::DeserializeOwned>(self) -> Result<T, HttpError> {
        Ok(serde_json::from_slice(&self.bytes().await?)?)
    }
}

pub(crate) fn stalled(timeout: Duration) -> HttpError {
    HttpError::Transport {
        detail: format!(
            "the Messages stream sent no data for {} s; the stream stalled",
            timeout.as_secs()
        ),
        retryable: true,
        timeout: true,
        unsent: false,
    }
}

#[derive(Default)]
struct IdleTimer(Option<nanocode_platform::Sleep>);

impl IdleTimer {
    async fn within<T>(&mut self, timeout: Duration, read: impl Future<Output = T>) -> Option<T> {
        let deadline = nanocode_platform::now() + timeout;
        let sleep = self
            .0
            .get_or_insert_with(|| nanocode_platform::Sleep::until(deadline));
        let mut read = std::pin::pin!(read);
        loop {
            nanocode_sync::select! {
                value = &mut read => return Some(value),
                () = &mut *sleep => {
                    if nanocode_platform::now() >= deadline {
                        self.0 = None;
                        return None;
                    }
                    sleep.reset(deadline);
                }
            }
        }
    }
}
