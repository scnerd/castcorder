use serde::{Deserialize, Serialize};

/// One message on the pipe. A data channel message is one `Frame`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Frame {
    /// Client -> host: a server function call (what would be an HTTP request).
    Req {
        id: u64,
        method: String,
        path: String,
        content_type: String,
        accepts: String,
        body: Vec<u8>,
    },
    /// Host -> client: the reply to `Req { id }` (what would be an HTTP response).
    Res {
        id: u64,
        status: u16,
        content_type: String,
        body: Vec<u8>,
    },
    /// Host -> client: unsolicited push (level meters, connection state, ...).
    Event { topic: String, body: Vec<u8> },
}

impl Frame {
    pub fn encode(&self) -> Vec<u8> {
        postcard::to_allocvec(self).expect("frame encode")
    }
    pub fn decode(bytes: &[u8]) -> Result<Frame, postcard::Error> {
        postcard::from_bytes(bytes)
    }
}
