//! Abstract ordered reliable message channel, an in-memory implementation
//! that can be cut or tampered with, and a driver that pumps a sender and a
//! receiver over it.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::Rc;

use crate::receiver::{RecvError, Receiver};
use crate::sender::Sender;
use crate::storage::Storage;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Closed;

/// What the core needs from a WebRTC data channel (or any ordered reliable
/// message pipe). `send` must not reorder; `buffered` lets the driver apply
/// backpressure (RTCDataChannel.bufferedAmount in the browser).
pub trait Channel {
    fn send(&mut self, msg: Vec<u8>) -> Result<(), Closed>;
    fn try_recv(&mut self) -> Result<Option<Vec<u8>>, Closed>;
    fn buffered(&self) -> usize;
}

type Tamper = Box<dyn FnMut(u64, &mut Vec<u8>)>;

struct LinkState {
    q: [VecDeque<Vec<u8>>; 2],
    queued_bytes: [usize; 2],
    closed: bool,
    /// Messages (both directions) still allowed before the link is cut.
    budget: Option<u64>,
    /// Messages sent from side 0 (sender) to side 1, counting from 0.
    sent_0_to_1: u64,
    tamper: Option<Tamper>,
}

/// One end of an in-memory link. Side 0 is the sender, side 1 the receiver.
pub struct MemEnd {
    st: Rc<RefCell<LinkState>>,
    side: usize,
}

pub fn mem_link(budget: Option<u64>, tamper: Option<Tamper>) -> (MemEnd, MemEnd) {
    let st = Rc::new(RefCell::new(LinkState {
        q: [VecDeque::new(), VecDeque::new()],
        queued_bytes: [0, 0],
        closed: false,
        budget,
        sent_0_to_1: 0,
        tamper,
    }));
    (
        MemEnd {
            st: st.clone(),
            side: 0,
        },
        MemEnd { st, side: 1 },
    )
}

impl MemEnd {
    pub fn cut(&self) {
        let mut s = self.st.borrow_mut();
        s.closed = true;
        s.q[0].clear();
        s.q[1].clear();
    }
}

impl Channel for MemEnd {
    fn send(&mut self, mut msg: Vec<u8>) -> Result<(), Closed> {
        let mut s = self.st.borrow_mut();
        if s.closed {
            return Err(Closed);
        }
        if let Some(b) = &mut s.budget {
            if *b == 0 {
                s.closed = true;
                s.q[0].clear();
                s.q[1].clear();
                return Err(Closed);
            }
            *b -= 1;
        }
        let to = 1 - self.side;
        if self.side == 0 {
            let idx = s.sent_0_to_1;
            s.sent_0_to_1 += 1;
            if let Some(t) = &mut s.tamper {
                t(idx, &mut msg);
            }
        }
        s.queued_bytes[to] += msg.len();
        s.q[to].push_back(msg);
        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<Vec<u8>>, Closed> {
        let mut s = self.st.borrow_mut();
        if s.closed {
            return Err(Closed);
        }
        let m = s.q[self.side].pop_front();
        if let Some(m) = &m {
            s.queued_bytes[self.side] -= m.len();
        }
        Ok(m)
    }

    fn buffered(&self) -> usize {
        self.st.borrow().queued_bytes[1 - self.side]
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Outcome {
    /// Receiver verified everything and the sender got DONE.
    Completed,
    /// Channel closed underneath us.
    Cut,
    /// Receiver saw a protocol violation and dropped the connection.
    ProtocolReset(&'static str),
    /// Sender said it cannot serve the blob.
    PeerError(u8),
}

/// Pump messages until completion or the channel dies. `max_buffered` is
/// the backpressure threshold in bytes (like bufferedAmountLowThreshold).
pub fn run_session<S1: Storage, S2: Storage>(
    sender: &mut Sender<S1>,
    receiver: &mut Receiver<S2>,
    s_end: &mut impl Channel,
    r_end: &mut impl Channel,
    max_buffered: usize,
) -> Result<Outcome, RecvError> {
    let outcome = loop {
        let mut progress = false;
        // receiver -> sender
        let mut cut = false;
        while let Some(m) = receiver.poll_transmit()? {
            progress = true;
            if r_end.send(m).is_err() {
                cut = true;
                break;
            }
        }
        if cut {
            break Outcome::Cut;
        }
        loop {
            match s_end.try_recv() {
                Ok(Some(m)) => {
                    progress = true;
                    if sender.handle(&m).is_err() {
                        cut = true;
                        break;
                    }
                }
                Ok(None) => break,
                Err(Closed) => {
                    cut = true;
                    break;
                }
            }
        }
        if cut {
            break Outcome::Cut;
        }
        // sender -> receiver, respecting backpressure
        while s_end.buffered() < max_buffered {
            let Some(m) = sender.poll_transmit() else { break };
            progress = true;
            if s_end.send(m).is_err() {
                cut = true;
                break;
            }
        }
        if cut {
            break Outcome::Cut;
        }
        let mut reset = None;
        loop {
            match r_end.try_recv() {
                Ok(Some(m)) => {
                    progress = true;
                    match receiver.handle(&m) {
                        Ok(()) => {}
                        Err(RecvError::Protocol(p)) => {
                            reset = Some(p);
                            break;
                        }
                        Err(RecvError::Wire(w)) => {
                            reset = Some(w.0);
                            break;
                        }
                        Err(e) => return Err(e),
                    }
                }
                Ok(None) => break,
                Err(Closed) => {
                    cut = true;
                    break;
                }
            }
        }
        if let Some(p) = reset {
            break Outcome::ProtocolReset(p);
        }
        if cut {
            break Outcome::Cut;
        }
        if let crate::receiver::State::PeerError(c) = receiver.state() {
            break Outcome::PeerError(c);
        }
        if receiver.is_finished() && sender.is_idle() {
            break Outcome::Completed;
        }
        if !progress {
            // nothing moved: treat as a dead link
            break Outcome::Cut;
        }
    };
    // The caller decides what a non-completed outcome means: a graceful
    // disconnect (call on_disconnect on both) or a crash (drop everything).
    Ok(outcome)
}
