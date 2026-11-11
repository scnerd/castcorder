//! Client half. Everything here is wasm-safe (no tokio on wasm32, no HTTP).

use crate::channel::MessageSink;
use crate::wire::Frame;
use bytes::Bytes;
use futures::channel::{mpsc, oneshot};
use futures::{Sink, Stream};
use http::Method;
use server_fn::client::Client;
use server_fn::error::{FromServerFnError, ServerFnErrorErr};
use server_fn::request::ClientReq;
use server_fn::response::ClientRes;
use std::collections::HashMap;
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

/// A server-pushed event.
#[derive(Debug, Clone)]
pub struct Event {
    pub topic: String,
    pub body: Vec<u8>,
}

struct ResFrame {
    status: u16,
    body: Bytes,
}

/// One live link to a host. Feed incoming messages to `on_message`.
pub struct Connection {
    next_id: AtomicU64,
    pending: Mutex<HashMap<u64, oneshot::Sender<ResFrame>>>,
    sink: Box<dyn MessageSink>,
    events: Mutex<Option<mpsc::UnboundedSender<Event>>>,
}

static CONN: OnceLock<Arc<Connection>> = OnceLock::new();

impl Connection {
    pub fn new(sink: impl MessageSink) -> Arc<Self> {
        Arc::new(Self {
            next_id: AtomicU64::new(1),
            pending: Mutex::new(HashMap::new()),
            sink: Box::new(sink),
            events: Mutex::new(None),
        })
    }

    /// Make this the connection all server functions use (like
    /// `server_fn::client::set_server_url`, but for a transport).
    pub fn install(self: &Arc<Self>) -> Result<(), &'static str> {
        CONN.set(self.clone())
            .map_err(|_| "connection already installed")
    }

    /// Subscribe to pushed events (one subscriber; fan out above this layer).
    pub fn events(&self) -> mpsc::UnboundedReceiver<Event> {
        let (tx, rx) = mpsc::unbounded();
        *self.events.lock().unwrap() = Some(tx);
        rx
    }

    /// Call from the data channel's onmessage handler / the read loop.
    pub fn on_message(&self, bytes: &[u8]) {
        match Frame::decode(bytes) {
            Ok(Frame::Res {
                id, status, body, ..
            }) => {
                if let Some(tx) = self.pending.lock().unwrap().remove(&id) {
                    let _ = tx.send(ResFrame {
                        status,
                        body: Bytes::from(body),
                    });
                }
            }
            Ok(Frame::Event { topic, body }) => {
                if let Some(tx) = self.events.lock().unwrap().as_ref() {
                    let _ = tx.unbounded_send(Event { topic, body });
                }
            }
            Ok(Frame::Req { .. }) | Err(_) => {} // guests do not serve requests
        }
    }

    fn call(&self, req: HostReq) -> oneshot::Receiver<ResFrame> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending.lock().unwrap().insert(id, tx);
        self.sink.send(
            Frame::Req {
                id,
                method: req.method.to_string(),
                path: req.path,
                content_type: req.content_type,
                accepts: req.accepts,
                body: req.body.to_vec(),
            }
            .encode(),
        );
        rx
    }
}

// ---- server_fn glue -------------------------------------------------------

/// "Request" as the codec layer builds it. Plain data, no HTTP library.
pub struct HostReq {
    method: Method,
    path: String,
    content_type: String,
    accepts: String,
    body: Bytes,
}

/// "Response" as the codec layer reads it.
pub struct HostRes {
    status: u16,
    body: Bytes,
}

fn unsupported<E: FromServerFnError>(what: &str) -> E {
    E::from_server_fn_error(ServerFnErrorErr::Request(format!(
        "{what} is not supported by HostClient"
    )))
}

impl<E: FromServerFnError> ClientReq<E> for HostReq {
    type FormData = ();

    fn try_new_req_query(
        path: &str,
        content_type: &str,
        accepts: &str,
        query: &str,
        method: Method,
    ) -> Result<Self, E> {
        // GET-style input: the query string rides in the path, as over HTTP.
        Ok(Self {
            method,
            path: format!("{path}?{query}"),
            content_type: content_type.into(),
            accepts: accepts.into(),
            body: Bytes::new(),
        })
    }

    fn try_new_req_text(
        path: &str,
        content_type: &str,
        accepts: &str,
        body: String,
        method: Method,
    ) -> Result<Self, E> {
        Self::try_new_req_bytes(path, content_type, accepts, Bytes::from(body), method)
    }

    fn try_new_req_bytes(
        path: &str,
        content_type: &str,
        accepts: &str,
        body: Bytes,
        method: Method,
    ) -> Result<Self, E> {
        Ok(Self {
            method,
            path: path.into(),
            content_type: content_type.into(),
            accepts: accepts.into(),
            body,
        })
    }

    fn try_new_req_form_data(
        _: &str,
        _: &str,
        _: &str,
        _: Self::FormData,
        _: Method,
    ) -> Result<Self, E> {
        Err(unsupported("form data"))
    }

    fn try_new_req_multipart(_: &str, _: &str, _: Self::FormData, _: Method) -> Result<Self, E> {
        Err(unsupported("multipart"))
    }

    fn try_new_req_streaming(
        _: &str,
        _: &str,
        _: &str,
        _: impl Stream<Item = Bytes> + Send + 'static,
        _: Method,
    ) -> Result<Self, E> {
        Err(unsupported("streaming request bodies"))
    }
}

impl<E: FromServerFnError> ClientRes<E> for HostRes {
    async fn try_into_string(self) -> Result<String, E> {
        String::from_utf8(self.body.to_vec()).map_err(|e| {
            E::from_server_fn_error(ServerFnErrorErr::Deserialization(e.to_string()))
        })
    }

    async fn try_into_bytes(self) -> Result<Bytes, E> {
        Ok(self.body)
    }

    fn try_into_stream(
        self,
    ) -> Result<impl Stream<Item = Result<Bytes, Bytes>> + Send + Sync + 'static, E> {
        // Whole body at once. Real streaming would need Res frames with
        // chunk/end flags; not needed for the proof.
        Ok(futures::stream::iter(vec![Ok(self.body)]))
    }

    fn status(&self) -> u16 {
        self.status
    }
    fn status_text(&self) -> String {
        String::new()
    }
    fn location(&self) -> String {
        String::new()
    }
    fn has_redirect(&self) -> bool {
        false
    }
}

/// The custom `Client`: instead of fetch/reqwest it writes a frame to the
/// installed `Connection` and awaits the matching `Res` frame.
pub struct HostClient;

impl<E, I, O> Client<E, I, O> for HostClient
where
    E: FromServerFnError,
    I: FromServerFnError,
    O: FromServerFnError,
{
    type Request = HostReq;
    type Response = HostRes;

    fn send(req: HostReq) -> impl Future<Output = Result<HostRes, E>> + Send {
        async move {
            let conn = CONN.get().ok_or_else(|| {
                E::from_server_fn_error(ServerFnErrorErr::Request(
                    "no host connection installed".into(),
                ))
            })?;
            let rx = conn.call(req);
            let res = rx.await.map_err(|_| {
                E::from_server_fn_error(ServerFnErrorErr::Request("connection closed".into()))
            })?;
            Ok(HostRes {
                status: res.status,
                body: res.body,
            })
        }
    }

    fn open_websocket(
        _path: &str,
    ) -> impl Future<
        Output = Result<
            (
                impl Stream<Item = Result<Bytes, Bytes>> + Send + 'static,
                impl Sink<Bytes> + Send + 'static,
            ),
            E,
        >,
    > + Send {
        async move {
            Err::<
                (
                    futures::stream::Empty<Result<Bytes, Bytes>>,
                    futures::sink::Drain<Bytes>,
                ),
                E,
            >(unsupported("websocket protocol"))
        }
    }

    fn spawn(future: impl Future<Output = ()> + Send + 'static) {
        #[cfg(target_arch = "wasm32")]
        wasm_bindgen_futures::spawn_local(future);
        #[cfg(not(target_arch = "wasm32"))]
        {
            tokio::spawn(future);
        }
    }
}
