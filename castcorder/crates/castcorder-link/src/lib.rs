//! Reaching a host by its call link, shared by the host, the Browser Guest and
//! the rendezvous (which is AGPL and may depend on this MIT crate, never
//! the other way round).
//!
//! - [`link`]: the call link `https://<rendezvous>/v/<version>/#<host-id>.<call-secret>`.
//!   The host id is the host's Ed25519 public key; the fragment never reaches
//!   any server.
//! - [`seal`]: end-to-end sealing. A guest seals its offer to the host key with
//!   an ephemeral X25519 key; the host's answer is sealed back under a key only
//!   that ephemeral exchange knows. The rendezvous sees only routing metadata.
//! - [`wire`]: the JSON text frames on the rendezvous WebSocket, and the
//!   sealed plaintexts inside them.
//! - [`identity`]: a device's Ed25519 key, its device id (a fingerprint of
//!   the key) and the signature an offer carries to prove it.
//! - [`stun`]: just enough RFC 5389 for a Binding request and its
//!   XOR-MAPPED-ADDRESS answer (the host learns its server-reflexive address
//!   from the rendezvous).
//!
//! Nothing here draws randomness: callers pass random bytes in, so the crate
//! is deterministic under test and needs no wasm randomness backend.

pub mod identity;
pub mod link;
pub mod seal;
pub mod stun;
pub mod wire;

pub use identity::{DeviceProof, ProofError};
pub use link::{CallLink, CallSecret, HostId, LinkError};
pub use seal::{GuestSeal, HostKey, ReplySeal, SealError};
