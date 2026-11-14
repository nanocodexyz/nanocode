use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use nanocode_http::header;
use nanocode_http::server::{Options, Reply, Request, serve};
use nanocode_http::wire;
use nanocode_io::{AsyncRead, AsyncWrite, ReadBuf};
use nanocode_sync::{Duplex, SinkExt, StreamExt};
use nanocode_websocket::{Message, WebSocketConfig, accept_hdr_async_with_config, server};

use super::registry::{Acceptor, Binding};
use crate::remote::{MessageWriter, SocketMessage, read_catalog, websocket};

struct Replayed {
    io: Box<dyn Duplex>,
    unread: Vec<u8>,
}

impl AsyncRead for Replayed {
    fn poll_read(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        if self.unread.is_empty() {
            return Pin::new(&mut self.io).poll_read(context, buffer);
        }
        let count = self.unread.len().min(buffer.remaining());
        buffer.put_slice(&self.unread[..count]);
        self.unread.drain(..count);
        Poll::Ready(Ok(()))
    }
}

impl AsyncWrite for Replayed {
    fn poll_write(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
        bytes: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        Pin::new(&mut self.io).poll_write(context, bytes)
    }

    fn poll_flush(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_flush(context)
    }

    fn poll_shutdown(
        mut self: Pin<&mut Self>,
        context: &mut Context<'_>,
    ) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.io).poll_shutdown(context)
    }
}

struct Frame(Message);

impl From<String> for Frame {
    fn from(text: String) -> Self {
        Self(Message::text(text))
    }
}

fn classify(frame: Frame) -> SocketMessage<Frame> {
    match frame.0 {
        Message::Text(text) => SocketMessage::Text(text.into()),
        Message::Ping(bytes) => SocketMessage::Ping(Frame(Message::Pong(bytes))),
        Message::Pong(_) => SocketMessage::Pong,
        Message::Close(_) => SocketMessage::Close,
        Message::Binary(_) | Message::Frame(_) => SocketMessage::Invalid("not a text frame"),
    }
}

const fn ping() -> Frame {
    Frame(Message::Ping(Vec::new()))
}

impl Acceptor {
    pub async fn serve<H, F>(self: &Arc<Self>, io: Box<dyn Duplex>, other: H)
    where
        H: FnMut(Request) -> F,
        F: Future<Output = Reply>,
    {
        let mut connection = wire::Connection::new(io);
        let deadline = self.options.head_timeout;
        let Ok(Ok(head)) = nanocode_platform::timeout(deadline, connection.request_head()).await
        else {
            return;
        };
        let (io, unread) = connection.into_parts();
        let mut replay = wire::request_head(&head.method, &head.target, &head.headers);
        replay.extend_from_slice(&unread);
        let io = Replayed { io, unread: replay };
        let upgrade = head
            .headers
            .get(header::UPGRADE)
            .and_then(|value| value.to_str().ok());
        if head.target == self.options.path
            && upgrade.is_some_and(|value| value.eq_ignore_ascii_case("websocket"))
        {
            if let Err(error) = self.upgrade(io).await {
                nanocode_trace::warn!(%error, "hand socket ended");
            }
            return;
        }
        let options = Options {
            header_read_timeout: Some(deadline),
        };
        drop(serve(Box::new(io), options, other).await);
    }

    async fn upgrade(self: &Arc<Self>, io: Replayed) -> Result<(), String> {
        let mut bound: Option<Binding> = None;
        let callback = |request: &server::Request, response: server::Response| match (self
            .authorize)(
            request.uri().path(),
            request.headers(),
        ) {
            Ok(binding) => {
                bound = Some(binding);
                Ok(response)
            }
            Err(refusal) => {
                let mut rejection = server::ErrorResponse::new(Some(refusal.body));
                *rejection.status_mut() = refusal.status;
                Err(rejection)
            }
        };
        let limit = self.options.frame_bytes_max;
        let config = WebSocketConfig {
            message_bytes_max: limit,
            frame_bytes_max: limit,
            ..WebSocketConfig::default()
        };
        let socket = accept_hdr_async_with_config(io, callback, Some(config))
            .await
            .map_err(|error| error.to_string())?;
        let key = bound
            .ok_or("a handshake that passed names its binding")?
            .key;
        let (sink, stream) = socket.split();
        let mut stream = stream.map(|message| message.map(Frame));
        let first = async {
            match stream.next().await {
                Some(Ok(Frame(Message::Text(first)))) => Some(Some(first.into())),
                Some(Ok(_)) => Some(None),
                Some(Err(_)) | None => None,
            }
        };
        let catalog = read_catalog(first, self.options.catalog_timeout, limit)
            .await
            .map_err(|error| format!("hand {key}: {error}"))?;
        let sink = sink
            .with(|frame: Frame| std::future::ready(Ok::<_, nanocode_websocket::Error>(frame.0)));
        let (writer, writing) = MessageWriter::new(sink);
        let beat = (self.options.ping, self.options.heartbeat_timeout);
        let (link, heartbeat) = websocket(
            stream,
            writer,
            Some(catalog.frame_limit(limit)),
            catalog.resumes().map(str::to_owned),
            classify,
            ping,
            beat,
        );
        let attached = self.attach(key, catalog, link).await?;
        let why = nanocode_sync::select! {
            why = heartbeat => why,
            () = writing => "socket write failed",
            () = attached.ended() => "socket closed",
        };
        drop(attached);
        Err(why.to_owned())
    }
}
