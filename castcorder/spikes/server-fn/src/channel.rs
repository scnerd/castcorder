//! The abstract transport. A WebRTC data channel (or a loopback socket) is
//! "send bytes" plus "a callback when bytes arrive"; that is all we need.

use futures::channel::mpsc;

/// Outgoing half: one call = one message. Real impls: `RtcDataChannel::send`
/// wrapped in `SendWrapper` (browser), a loopback websocket/TCP writer (desktop).
pub trait MessageSink: Send + Sync + 'static {
    fn send(&self, msg: Vec<u8>);
}

impl<F: Fn(Vec<u8>) + Send + Sync + 'static> MessageSink for F {
    fn send(&self, msg: Vec<u8>) {
        self(msg)
    }
}

/// One end of an in-memory duplex pipe (stand-in for the data channel).
pub struct MemEnd {
    pub tx: mpsc::UnboundedSender<Vec<u8>>,
    pub rx: mpsc::UnboundedReceiver<Vec<u8>>,
}

pub fn duplex() -> (MemEnd, MemEnd) {
    let (a_tx, b_rx) = mpsc::unbounded();
    let (b_tx, a_rx) = mpsc::unbounded();
    (MemEnd { tx: a_tx, rx: a_rx }, MemEnd { tx: b_tx, rx: b_rx })
}

impl MessageSink for mpsc::UnboundedSender<Vec<u8>> {
    fn send(&self, msg: Vec<u8>) {
        let _ = self.unbounded_send(msg);
    }
}
