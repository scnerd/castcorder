//! Owned application endpoints for a guest's "control" and "data" data channels.

use std::collections::VecDeque;
use std::fmt;
use std::sync::{Arc, Mutex, MutexGuard};

use castcorder_protocol::rpc::MessageChannel;

use crate::GuestId;

/// The largest message [`GuestChannel::send`] accepts (str0m's SCTP maximum).
/// Prefer 64 KiB messages for bulk data.
pub const MAX_MESSAGE: usize = 256 * 1024;

/// Which of a guest's two data channels.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum ChannelKind {
    /// Label "control": castcorder-protocol messages (binary frames) plus the
    /// transport's own text frames (heartbeat, renegotiation, bye).
    Control,
    /// Label "data": bulk transfer, binary frames only.
    Data,
}

impl ChannelKind {
    /// The data channel label the guest must use.
    pub fn label(self) -> &'static str {
        match self {
            ChannelKind::Control => "control",
            ChannelKind::Data => "data",
        }
    }
}

/// Why a channel operation failed.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ChannelError {
    /// The guest is gone; the channel will never carry anything again.
    Closed,
    /// The message is larger than [`MAX_MESSAGE`].
    TooLarge,
}

impl fmt::Display for ChannelError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ChannelError::Closed => f.write_str("channel closed"),
            ChannelError::TooLarge => f.write_str("message larger than MAX_MESSAGE"),
        }
    }
}

impl std::error::Error for ChannelError {}

#[derive(Debug, Default)]
pub(crate) struct ChannelState {
    pub(crate) outgoing: VecDeque<Vec<u8>>,
    pub(crate) outgoing_bytes: usize,
    pub(crate) incoming: VecDeque<Vec<u8>>,
    /// str0m's buffered amount for this channel at the last host poll.
    pub(crate) sctp_buffered: usize,
    pub(crate) closed: bool,
}

pub(crate) type Shared = Arc<Mutex<ChannelState>>;

pub(crate) fn lock(s: &Shared) -> MutexGuard<'_, ChannelState> {
    s.lock().expect("guest channel lock poisoned")
}

/// One end of a guest's data channel, owned by the application.
///
/// Sending only queues: the host writes queued messages into the connection on
/// its next [`Host::poll`](crate::Host::poll), and received messages appear here
/// after a poll. Messages sent before the channel opens wait in the queue. Clones
/// share the same queues. Once the guest is gone, every call fails with
/// [`ChannelError::Closed`] (queued incoming messages are still delivered first).
#[derive(Clone, Debug)]
pub struct GuestChannel {
    guest: GuestId,
    kind: ChannelKind,
    state: Shared,
}

impl GuestChannel {
    pub(crate) fn new(guest: GuestId, kind: ChannelKind, state: Shared) -> Self {
        Self { guest, kind, state }
    }

    /// The guest this channel belongs to.
    pub fn guest(&self) -> GuestId {
        self.guest
    }

    /// Control or data.
    pub fn kind(&self) -> ChannelKind {
        self.kind
    }

    /// Bytes sent on this end that have not yet left the host: still queued here
    /// plus held in the connection's send buffer as of the last poll. The
    /// equivalent of RTCDataChannel.bufferedAmount, for backpressure.
    pub fn buffered(&self) -> usize {
        let s = lock(&self.state);
        s.outgoing_bytes + s.sctp_buffered
    }

    /// Whether the guest is gone.
    pub fn is_closed(&self) -> bool {
        lock(&self.state).closed
    }
}

impl MessageChannel for GuestChannel {
    type Error = ChannelError;

    fn send(&mut self, message: Vec<u8>) -> Result<(), ChannelError> {
        if message.len() > MAX_MESSAGE {
            return Err(ChannelError::TooLarge);
        }
        let mut s = lock(&self.state);
        if s.closed {
            return Err(ChannelError::Closed);
        }
        s.outgoing_bytes += message.len();
        s.outgoing.push_back(message);
        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<Vec<u8>>, ChannelError> {
        let mut s = lock(&self.state);
        match s.incoming.pop_front() {
            Some(m) => Ok(Some(m)),
            None if s.closed => Err(ChannelError::Closed),
            None => Ok(None),
        }
    }
}
