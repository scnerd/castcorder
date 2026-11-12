//! Signaling through the rendezvous, for a page opened from a call link
//! (`.../v/<version>/#<host-id>.<call-secret>`): one short-lived WebSocket per
//! connection attempt. The server's hello carries the ICE servers the
//! RTCPeerConnection should use; the offer goes out sealed to the host key
//! with a fresh ephemeral key, and the answer comes back sealed to that key
//! (castcorder-link). The call secret travels only inside the sealed offer.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use castcorder_link::wire::{
    AnswerPlain, ClientMsg, OfferPlain, ServerMsg, b64, from_json, to_json, unb64,
};
use castcorder_link::{CallLink, GuestSeal};
use js_sys::Promise;
use wasm_bindgen::JsCast;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::{MessageEvent, WebSocket};

/// How long to wait for the hello, and for the host's answer.
const HELLO_MS: f64 = 10_000.0;
const ANSWER_MS: f64 = 15_000.0;

/// How a refusal by the host (not a network failure) begins; not worth retrying.
pub const REJECTED: &str = "rejected by the host";

/// Resolve after `ms` milliseconds.
pub async fn sleep_ms(ms: i32) {
    let p = Promise::new(&mut |resolve, _| {
        let _ = web_sys::window()
            .expect("window")
            .set_timeout_with_callback_and_timeout_and_arguments_0(&resolve, ms);
    });
    let _ = JsFuture::from(p).await;
}

/// One open mailbox connection.
pub struct Mailbox {
    ws: WebSocket,
    inbox: Rc<RefCell<VecDeque<String>>>,
    closed: Rc<RefCell<bool>>,
    /// The ICE servers from the hello, as a JS array for RTCConfiguration.
    pub ice_servers: JsValue,
}

impl Drop for Mailbox {
    fn drop(&mut self) {
        self.ws.set_onmessage(None);
        self.ws.set_onclose(None);
        let _ = self.ws.close();
    }
}

fn now() -> f64 {
    js_sys::Date::now()
}

impl Mailbox {
    /// Connect to `url` (`ws://` or `wss://.../ws`) and wait for the hello.
    pub async fn open(url: &str) -> Result<Mailbox, JsValue> {
        let ws = WebSocket::new(url)?;
        let inbox = Rc::new(RefCell::new(VecDeque::new()));
        let closed = Rc::new(RefCell::new(false));
        let i = inbox.clone();
        let onmsg = Closure::<dyn FnMut(MessageEvent)>::new(move |ev: MessageEvent| {
            if let Some(t) = ev.data().as_string() {
                i.borrow_mut().push_back(t);
            }
        });
        ws.set_onmessage(Some(onmsg.as_ref().unchecked_ref()));
        onmsg.forget();
        let c = closed.clone();
        let onclose = Closure::<dyn FnMut()>::new(move || *c.borrow_mut() = true);
        ws.set_onclose(Some(onclose.as_ref().unchecked_ref()));
        ws.set_onerror(Some(onclose.as_ref().unchecked_ref()));
        onclose.forget();
        let mut mb = Mailbox {
            ws,
            inbox,
            closed,
            ice_servers: js_sys::Array::new().into(),
        };
        match mb.next(HELLO_MS).await? {
            ServerMsg::Hello { ice_servers, .. } => {
                mb.ice_servers = js_sys::JSON::parse(&to_json(&ice_servers))?;
                Ok(mb)
            }
            ServerMsg::Error { code, message } => Err(JsValue::from_str(&format!(
                "rendezvous refused: {code}: {message}"
            ))),
            other => Err(JsValue::from_str(&format!(
                "rendezvous sent {other:?} before hello"
            ))),
        }
    }

    async fn next(&self, timeout_ms: f64) -> Result<ServerMsg, JsValue> {
        let start = now();
        loop {
            if let Some(t) = self.inbox.borrow_mut().pop_front() {
                return from_json::<ServerMsg>(&t)
                    .ok_or_else(|| JsValue::from_str("unrecognised frame from the rendezvous"));
            }
            if *self.closed.borrow() {
                return Err(JsValue::from_str("rendezvous connection closed"));
            }
            if now() - start > timeout_ms {
                return Err(JsValue::from_str("rendezvous timed out"));
            }
            sleep_ms(30).await;
        }
    }

    /// Seal `sdp` (with the call secret and `name`) to the link's host, send
    /// it, and open the host's answer SDP. A refusal (wrong call secret, host
    /// offline) is an error naming why.
    pub async fn exchange(
        &self,
        link: &CallLink,
        name: &str,
        sdp: &str,
    ) -> Result<String, JsValue> {
        let mut ephemeral = [0u8; 32];
        web_sys::window()
            .expect("window")
            .crypto()?
            .get_random_values_with_u8_array(&mut ephemeral)?;
        let plain = OfferPlain {
            secret: link.secret.encode(),
            name: name.to_string(),
            sdp: sdp.to_string(),
        };
        let (sealed, guest) = GuestSeal::seal(&link.host, ephemeral, to_json(&plain).as_bytes())
            .map_err(|e| JsValue::from_str(&e.to_string()))?;
        let frame = ClientMsg::Offer {
            to: link.host.encode(),
            sealed: b64(&sealed),
        };
        self.ws.send_with_str(&to_json(&frame))?;
        match self.next(ANSWER_MS).await? {
            ServerMsg::Answer { sealed } => {
                let bytes = unb64(&sealed)
                    .and_then(|s| guest.open_reply(&s).ok())
                    .ok_or_else(|| {
                        JsValue::from_str("the answer did not open: not from this host")
                    })?;
                match serde_json::from_slice::<AnswerPlain>(&bytes) {
                    Ok(AnswerPlain::Sdp(sdp)) => Ok(sdp),
                    Ok(AnswerPlain::Rejected(why)) => {
                        Err(JsValue::from_str(&format!("{REJECTED}: {why}")))
                    }
                    Err(e) => Err(JsValue::from_str(&format!("bad answer: {e}"))),
                }
            }
            ServerMsg::Error { code, message } => {
                Err(JsValue::from_str(&format!("rendezvous: {code}: {message}")))
            }
            other => Err(JsValue::from_str(&format!("unexpected {other:?}"))),
        }
    }
}
