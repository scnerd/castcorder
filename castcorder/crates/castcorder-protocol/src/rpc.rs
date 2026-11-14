//! Request/response correlation and event queuing over any ordered message channel.
//!
//! The helper owns no thread and no timer, so it works the same natively, in the
//! browser, and in a deterministic simulation: the caller decides when to [`poll`].
//!
//! - [`RpcClient`] assigns request ids, sends requests, and on [`poll`] sorts what
//!   arrives into responses (fetched by id) and events (fetched in order).
//! - [`RpcServer`] yields decoded requests and sends responses and events.
//! - [`MessageChannel`] is the transport seam: anything that moves whole messages in
//!   order (a data channel, a WebSocket, an in-process queue).
//! - [`memory_pair`] gives two connected in-process channels.
//!
//! [`poll`]: RpcClient::poll

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex};

use crate::{ClientMessage, CoreMessage, DecodeError, Event, Request, RequestId, Response};

/// An ordered, reliable channel of whole messages.
pub trait MessageChannel {
    /// The transport's error type.
    type Error;
    /// Queue one message for delivery to the other end.
    fn send(&mut self, message: Vec<u8>) -> Result<(), Self::Error>;
    /// Take the next delivered message, if one has arrived. Never blocks.
    fn try_recv(&mut self) -> Result<Option<Vec<u8>>, Self::Error>;
}

/// Something went wrong in the RPC layer.
#[derive(Debug, PartialEq, Eq)]
pub enum RpcError<E> {
    /// The underlying channel failed.
    Channel(E),
    /// A message could not be decoded.
    Decode(DecodeError),
    /// A response arrived for a request that is not pending.
    UnknownResponse(RequestId),
}

/// The client end: sends requests and collects responses and events.
#[derive(Debug)]
pub struct RpcClient<C> {
    channel: C,
    next_id: u64,
    pending: BTreeSet<RequestId>,
    responses: BTreeMap<RequestId, Response>,
    events: VecDeque<Event>,
}

impl<C: MessageChannel> RpcClient<C> {
    /// Wrap a channel connected to a core.
    pub fn new(channel: C) -> Self {
        Self {
            channel,
            next_id: 1,
            pending: BTreeSet::new(),
            responses: BTreeMap::new(),
            events: VecDeque::new(),
        }
    }

    /// Send a request and return the id its response will carry.
    pub fn send(&mut self, request: Request) -> Result<RequestId, RpcError<C::Error>> {
        let id = RequestId(self.next_id);
        self.next_id += 1;
        let bytes = crate::encode(&ClientMessage::Request { id, request });
        self.channel.send(bytes).map_err(RpcError::Channel)?;
        self.pending.insert(id);
        Ok(id)
    }

    /// Drain every message that has arrived, sorting responses and events.
    pub fn poll(&mut self) -> Result<(), RpcError<C::Error>> {
        while let Some(bytes) = self.channel.try_recv().map_err(RpcError::Channel)? {
            match crate::decode::<CoreMessage>(&bytes).map_err(RpcError::Decode)? {
                CoreMessage::Response { id, response } => {
                    if !self.pending.remove(&id) {
                        return Err(RpcError::UnknownResponse(id));
                    }
                    self.responses.insert(id, response);
                }
                CoreMessage::Event(event) => self.events.push_back(event),
            }
        }
        Ok(())
    }

    /// Take the response to `id` if it has arrived (call [`poll`](Self::poll) first).
    pub fn take_response(&mut self, id: RequestId) -> Option<Response> {
        self.responses.remove(&id)
    }

    /// Take the oldest event not yet taken.
    pub fn next_event(&mut self) -> Option<Event> {
        self.events.pop_front()
    }

    /// How many requests are still waiting for a response.
    pub fn pending_count(&self) -> usize {
        self.pending.len()
    }

    /// The wrapped channel.
    pub fn channel_mut(&mut self) -> &mut C {
        &mut self.channel
    }
}

/// The core end: receives requests and sends responses and events.
#[derive(Debug)]
pub struct RpcServer<C> {
    channel: C,
}

impl<C: MessageChannel> RpcServer<C> {
    /// Wrap a channel connected to a client.
    pub fn new(channel: C) -> Self {
        Self { channel }
    }

    /// Take the next request, if one has arrived.
    pub fn next_request(&mut self) -> Result<Option<(RequestId, Request)>, RpcError<C::Error>> {
        match self.channel.try_recv().map_err(RpcError::Channel)? {
            None => Ok(None),
            Some(bytes) => {
                match crate::decode::<ClientMessage>(&bytes).map_err(RpcError::Decode)? {
                    ClientMessage::Request { id, request } => Ok(Some((id, request))),
                }
            }
        }
    }

    /// Answer the request with this id.
    pub fn respond(&mut self, id: RequestId, response: Response) -> Result<(), RpcError<C::Error>> {
        let bytes = crate::encode(&CoreMessage::Response { id, response });
        self.channel.send(bytes).map_err(RpcError::Channel)
    }

    /// Send an unsolicited event.
    pub fn emit(&mut self, event: Event) -> Result<(), RpcError<C::Error>> {
        let bytes = crate::encode(&CoreMessage::Event(event));
        self.channel.send(bytes).map_err(RpcError::Channel)
    }

    /// The wrapped channel.
    pub fn channel_mut(&mut self) -> &mut C {
        &mut self.channel
    }
}

type Queue = Arc<Mutex<VecDeque<Vec<u8>>>>;

/// One end of an in-process ordered channel made by [`memory_pair`].
#[derive(Debug)]
pub struct MemoryChannel {
    outgoing: Queue,
    incoming: Queue,
}

/// The other end of a [`MemoryChannel`] was dropped.
#[derive(Debug, PartialEq, Eq)]
pub struct Disconnected;

impl MessageChannel for MemoryChannel {
    type Error = Disconnected;

    fn send(&mut self, message: Vec<u8>) -> Result<(), Disconnected> {
        if Arc::strong_count(&self.outgoing) < 2 {
            return Err(Disconnected);
        }
        self.outgoing
            .lock()
            .expect("memory channel lock poisoned")
            .push_back(message);
        Ok(())
    }

    fn try_recv(&mut self) -> Result<Option<Vec<u8>>, Disconnected> {
        let message = self
            .incoming
            .lock()
            .expect("memory channel lock poisoned")
            .pop_front();
        match message {
            Some(m) => Ok(Some(m)),
            None if Arc::strong_count(&self.incoming) < 2 => Err(Disconnected),
            None => Ok(None),
        }
    }
}

/// Two connected in-process channels: what one sends, the other receives, in order.
pub fn memory_pair() -> (MemoryChannel, MemoryChannel) {
    let a: Queue = Arc::default();
    let b: Queue = Arc::default();
    (
        MemoryChannel {
            outgoing: a.clone(),
            incoming: b.clone(),
        },
        MemoryChannel {
            outgoing: b,
            incoming: a,
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ParticipantId, TakeId, TakeStatus};

    fn pair() -> (RpcClient<MemoryChannel>, RpcServer<MemoryChannel>) {
        let (a, b) = memory_pair();
        (RpcClient::new(a), RpcServer::new(b))
    }

    #[test]
    fn responses_are_matched_to_requests_even_out_of_order() {
        let (mut client, mut server) = pair();
        let join = client
            .send(Request::Join {
                display_name: "Bob".into(),
                protocol_version: crate::PROTOCOL_VERSION,
            })
            .unwrap();
        let status = client.send(Request::GetTakeStatus).unwrap();
        assert_ne!(join, status);

        let (id1, _) = server.next_request().unwrap().unwrap();
        let (id2, req2) = server.next_request().unwrap().unwrap();
        assert_eq!(req2, Request::GetTakeStatus);
        assert!(server.next_request().unwrap().is_none());
        // Answer the second request first.
        server
            .respond(id2, Response::TakeStatus(TakeStatus::Idle))
            .unwrap();
        server
            .respond(
                id1,
                Response::Joined {
                    participant: ParticipantId(3),
                },
            )
            .unwrap();

        assert_eq!(client.pending_count(), 2);
        client.poll().unwrap();
        assert_eq!(client.pending_count(), 0);
        assert_eq!(
            client.take_response(join),
            Some(Response::Joined {
                participant: ParticipantId(3)
            })
        );
        assert_eq!(
            client.take_response(status),
            Some(Response::TakeStatus(TakeStatus::Idle))
        );
        assert_eq!(client.take_response(status), None);
    }

    #[test]
    fn events_interleave_with_responses_and_keep_their_order() {
        let (mut client, mut server) = pair();
        let id = client.send(Request::StartTake).unwrap();
        let (sid, _) = server.next_request().unwrap().unwrap();
        server.emit(Event::Meters(vec![])).unwrap();
        server
            .respond(sid, Response::TakeStarted { take: TakeId(1) })
            .unwrap();
        server
            .emit(Event::TakeStatusChanged(TakeStatus::Recording {
                take: TakeId(1),
                video: false,
            }))
            .unwrap();
        client.poll().unwrap();
        assert_eq!(client.next_event(), Some(Event::Meters(vec![])));
        assert_eq!(
            client.next_event(),
            Some(Event::TakeStatusChanged(TakeStatus::Recording {
                take: TakeId(1),
                video: false
            }))
        );
        assert_eq!(client.next_event(), None);
        assert_eq!(
            client.take_response(id),
            Some(Response::TakeStarted { take: TakeId(1) })
        );
    }

    #[test]
    fn unknown_response_ids_and_garbage_are_errors() {
        let (mut client, mut server) = pair();
        server
            .respond(RequestId(99), Response::TakeStatus(TakeStatus::Idle))
            .unwrap();
        assert_eq!(client.poll(), Err(RpcError::UnknownResponse(RequestId(99))));
        client.channel_mut().send(vec![0xff, 0xff]).unwrap();
        assert!(matches!(server.next_request(), Err(RpcError::Decode(_))));
    }

    #[test]
    fn dropping_one_end_disconnects_the_other() {
        let (mut client, server) = pair();
        drop(server);
        assert_eq!(
            client.send(Request::ListSources),
            Err(RpcError::Channel(Disconnected))
        );
        assert_eq!(client.poll(), Err(RpcError::Channel(Disconnected)));
    }
}
