//! Host half (feature "ssr"). No HTTP framework: `server_fn` hands us its
//! registry of server functions (via `inventory`) keyed by (path, method) and
//! we call the handler with our own request type.

use crate::channel::MessageSink;
use crate::wire::Frame;
use bytes::Bytes;
use futures::{Sink, Stream};
use http::Method;
use serde::Serialize;
use server_fn::error::{FromServerFnError, ServerFnErrorErr};
use server_fn::request::Req;
use server_fn::response::{Res, TryRes};
use server_fn::{initialize_server_fn_map, ServerFnTraitObj};
use std::borrow::Cow;
use std::collections::HashMap;
use std::future::Future;
use std::sync::{Arc, LazyLock, RwLock};

/// What a server function receives.
pub struct ServerReq {
    method: Method,
    path: String,
    content_type: String,
    accepts: String,
    body: Bytes,
}

/// What a server function produces.
pub struct ServerRes {
    status: u16,
    content_type: String,
    body: Bytes,
}

impl<E, I, O> Req<E, I, O> for ServerReq
where
    E: FromServerFnError + Send,
    I: FromServerFnError + Send,
    O: FromServerFnError + Send,
{
    type WebsocketResponse = ServerRes;

    fn as_query(&self) -> Option<&str> {
        self.path.split_once('?').map(|(_, q)| q)
    }
    fn to_content_type(&self) -> Option<Cow<'_, str>> {
        Some(Cow::Borrowed(&self.content_type))
    }
    fn accepts(&self) -> Option<Cow<'_, str>> {
        Some(Cow::Borrowed(&self.accepts))
    }
    fn referer(&self) -> Option<Cow<'_, str>> {
        None
    }
    async fn try_into_bytes(self) -> Result<Bytes, E> {
        Ok(self.body)
    }
    async fn try_into_string(self) -> Result<String, E> {
        String::from_utf8(self.body.to_vec()).map_err(|e| {
            E::from_server_fn_error(ServerFnErrorErr::Deserialization(e.to_string()))
        })
    }
    fn try_into_stream(
        self,
    ) -> Result<impl Stream<Item = Result<Bytes, Bytes>> + Send + 'static, E> {
        Ok(futures::stream::iter(vec![Ok(self.body)]))
    }
    async fn try_into_websocket(
        self,
    ) -> Result<
        (
            impl Stream<Item = Result<Bytes, Bytes>> + Send + 'static,
            impl Sink<Bytes> + Send + 'static,
            ServerRes,
        ),
        E,
    > {
        Err::<
            (
                futures::stream::Empty<Result<Bytes, Bytes>>,
                futures::sink::Drain<Bytes>,
                ServerRes,
            ),
            E,
        >(E::from_server_fn_error(ServerFnErrorErr::Request(
            "websocket protocol not supported".into(),
        )))
    }
}

impl<E: FromServerFnError> TryRes<E> for ServerRes {
    fn try_from_string(content_type: &str, data: String) -> Result<Self, E> {
        Self::try_from_bytes(content_type, Bytes::from(data))
    }
    fn try_from_bytes(content_type: &str, data: Bytes) -> Result<Self, E> {
        Ok(ServerRes {
            status: 200,
            content_type: content_type.into(),
            body: data,
        })
    }
    fn try_from_stream(
        _content_type: &str,
        _data: impl Stream<Item = Result<Bytes, Bytes>> + Send + 'static,
    ) -> Result<Self, E> {
        Err(E::from_server_fn_error(ServerFnErrorErr::Response(
            "streaming responses not supported".into(),
        )))
    }
}

impl Res for ServerRes {
    fn error_response(_path: &str, err: Bytes) -> Self {
        ServerRes {
            status: 500,
            content_type: String::new(),
            body: err,
        }
    }
    fn content_type(&mut self, content_type: &str) {
        self.content_type = content_type.into();
    }
    fn redirect(&mut self, path: &str) {
        self.status = 302;
        self.content_type = path.into();
    }
}

/// The `server =` type for `#[server]`.
pub struct HostServer;

impl<E, I, O> server_fn::server::Server<E, I, O> for HostServer
where
    E: FromServerFnError + Send + Sync,
    I: FromServerFnError + Send + Sync,
    O: FromServerFnError + Send + Sync,
{
    type Request = ServerReq;
    type Response = ServerRes;

    fn spawn(future: impl Future<Output = ()> + Send + 'static) -> Result<(), E> {
        tokio::spawn(future);
        Ok(())
    }
}

// The #[server] macro `inventory::submit!`s each function; this collects the
// ones whose Server type uses our Request/Response types. No axum/actix.
/// Number of requests that arrived over the channel (test aid).
pub static DISPATCHED: std::sync::atomic::AtomicUsize = std::sync::atomic::AtomicUsize::new(0);

static REGISTRY: LazyLock<RwLock<HashMap<(String, Method), ServerFnTraitObj<ServerReq, ServerRes>>>> =
    initialize_server_fn_map!(ServerReq, ServerRes);

/// One connected peer (browser guest or the desktop UI's loopback link).
#[derive(Clone)]
pub struct Peer {
    sink: Arc<dyn MessageSink>,
}

tokio::task_local! {
    static CURRENT_PEER: Peer;
}

/// Inside a server fn body: who called me. (HTTP server_fn gets this from
/// framework extractors; here the dispatcher provides it per call.)
pub fn current_peer() -> Peer {
    CURRENT_PEER.with(|p| p.clone())
}

impl Peer {
    pub fn new(sink: impl MessageSink) -> Self {
        Peer {
            sink: Arc::new(sink),
        }
    }

    /// Server push: independent of any request.
    pub fn push_event<T: Serialize>(&self, topic: &str, value: &T) -> Result<(), String> {
        let body = postcard::to_allocvec(value).map_err(|e| e.to_string())?;
        self.sink.send(
            Frame::Event {
                topic: topic.into(),
                body,
            }
            .encode(),
        );
        Ok(())
    }

    /// Feed every message that arrives from this peer's channel here.
    pub fn on_message(&self, bytes: Vec<u8>) {
        let peer = self.clone();
        let Ok(Frame::Req {
            id,
            method,
            path,
            content_type,
            accepts,
            body,
        }) = Frame::decode(&bytes)
        else {
            return;
        };
        tokio::spawn(async move {
            let res = CURRENT_PEER
                .scope(peer.clone(), dispatch(method, path, content_type, accepts, body))
                .await;
            peer.sink.send(
                Frame::Res {
                    id,
                    status: res.status,
                    content_type: res.content_type,
                    body: res.body.to_vec(),
                }
                .encode(),
            );
        });
    }
}

async fn dispatch(
    method: String,
    path: String,
    content_type: String,
    accepts: String,
    body: Vec<u8>,
) -> ServerRes {
    DISPATCHED.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let method = method.parse::<Method>().unwrap_or(Method::POST);
    let route = path.split('?').next().unwrap_or("").to_string();
    let handler = REGISTRY
        .read()
        .unwrap()
        .get(&(route.clone(), method.clone()))
        .cloned();
    let Some(handler) = handler else {
        return ServerRes {
            status: 404,
            content_type: String::new(),
            body: Bytes::from(format!("no server function at {route}")),
        };
    };
    let req = ServerReq {
        method,
        path,
        content_type,
        accepts,
        body: Bytes::from(body),
    };
    let _ = (&req.method, &req.path); // kept for middleware/extractor use
    handler.handler(req).await
}

/// All registered (path, method) pairs, for debugging/tests.
pub fn registered_paths() -> Vec<String> {
    let mut v: Vec<_> = REGISTRY.read().unwrap().keys().map(|k| k.0.clone()).collect();
    v.sort();
    v
}

