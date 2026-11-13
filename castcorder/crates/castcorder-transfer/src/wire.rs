//! Message framing. One `Msg` is exactly one data channel message.
//!
//! All integers are little-endian. Every message starts with a 1-byte type
//! and the 32-byte BLAKE3 root of the blob it concerns, so several blobs can
//! share one channel.
//!
//! ```text
//! REQUEST  0x01 | root[32] | n:u32 | n x (start_unit:u64, end_unit:u64)
//! DATA     0x02 | root[32] | unit:u64 | offset:u32 | total:u32 | payload
//! DONE     0x03 | root[32] | item (rest of the message, may be empty)
//! ERROR    0x04 | root[32] | code:u8
//! ```
//!
//! REQUEST goes receiver -> sender and names unit ranges (half-open) the
//! receiver still needs. DATA carries a slice `[offset, offset+len)` of the
//! bao-tree encoding of one unit; the unit encoding is `total` bytes long.
//! DONE tells the sender the whole blob is verified on the receiver's disk.
//! `item` names which of the sender's blobs that is (the guest's track name),
//! because several blobs can share one root: byte-identical items. An empty
//! item is the original frame and confirms every blob with that root.

use std::ops::Range;

pub const T_REQUEST: u8 = 0x01;
pub const T_DATA: u8 = 0x02;
pub const T_DONE: u8 = 0x03;
pub const T_ERROR: u8 = 0x04;

/// Bytes of DATA header before the payload.
pub const DATA_HEADER: usize = 1 + 32 + 8 + 4 + 4;

pub const ERR_UNKNOWN_BLOB: u8 = 1;
pub const ERR_SOURCE_CORRUPT: u8 = 2;
pub const ERR_BAD_REQUEST: u8 = 3;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Msg {
    Request {
        root: [u8; 32],
        units: Vec<Range<u64>>,
    },
    Data {
        root: [u8; 32],
        unit: u64,
        offset: u32,
        total: u32,
        payload: Vec<u8>,
    },
    Done {
        root: [u8; 32],
        item: Vec<u8>,
    },
    Error {
        root: [u8; 32],
        code: u8,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WireError(pub &'static str);

impl std::fmt::Display for WireError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "wire error: {}", self.0)
    }
}

impl std::error::Error for WireError {}

/// Max ranges per REQUEST so a request stays far below any message limit.
pub const MAX_REQUEST_RANGES: usize = 4096;

impl Msg {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = Vec::new();
        match self {
            Msg::Request { root, units } => {
                out.reserve(1 + 32 + 4 + 16 * units.len());
                out.push(T_REQUEST);
                out.extend_from_slice(root);
                out.extend_from_slice(&(units.len() as u32).to_le_bytes());
                for r in units {
                    out.extend_from_slice(&r.start.to_le_bytes());
                    out.extend_from_slice(&r.end.to_le_bytes());
                }
            }
            Msg::Data {
                root,
                unit,
                offset,
                total,
                payload,
            } => {
                out.reserve(DATA_HEADER + payload.len());
                out.push(T_DATA);
                out.extend_from_slice(root);
                out.extend_from_slice(&unit.to_le_bytes());
                out.extend_from_slice(&offset.to_le_bytes());
                out.extend_from_slice(&total.to_le_bytes());
                out.extend_from_slice(payload);
            }
            Msg::Done { root, item } => {
                out.push(T_DONE);
                out.extend_from_slice(root);
                out.extend_from_slice(item);
            }
            Msg::Error { root, code } => {
                out.push(T_ERROR);
                out.extend_from_slice(root);
                out.push(*code);
            }
        }
        out
    }

    pub fn decode(b: &[u8]) -> Result<Msg, WireError> {
        let mut r = Reader { b, pos: 0 };
        let t = r.u8()?;
        let root = r.arr32()?;
        let msg = match t {
            T_REQUEST => {
                let n = r.u32()? as usize;
                if n > MAX_REQUEST_RANGES {
                    return Err(WireError("too many ranges"));
                }
                let mut units = Vec::with_capacity(n);
                for _ in 0..n {
                    let s = r.u64()?;
                    let e = r.u64()?;
                    if e <= s {
                        return Err(WireError("empty or inverted range"));
                    }
                    units.push(s..e);
                }
                Msg::Request { root, units }
            }
            T_DATA => {
                let unit = r.u64()?;
                let offset = r.u32()?;
                let total = r.u32()?;
                let payload = r.rest().to_vec();
                Msg::Data {
                    root,
                    unit,
                    offset,
                    total,
                    payload,
                }
            }
            T_DONE => Msg::Done {
                root,
                item: r.rest().to_vec(),
            },
            T_ERROR => Msg::Error {
                root,
                code: r.u8()?,
            },
            _ => return Err(WireError("unknown message type")),
        };
        if r.pos != b.len() {
            return Err(WireError("trailing bytes"));
        }
        Ok(msg)
    }
}

struct Reader<'a> {
    b: &'a [u8],
    pos: usize,
}

impl<'a> Reader<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.b.len() - self.pos < n {
            return Err(WireError("truncated message"));
        }
        let s = &self.b[self.pos..self.pos + n];
        self.pos += n;
        Ok(s)
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.take(1)?[0])
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        Ok(u32::from_le_bytes(self.take(4)?.try_into().unwrap()))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        Ok(u64::from_le_bytes(self.take(8)?.try_into().unwrap()))
    }
    fn arr32(&mut self) -> Result<[u8; 32], WireError> {
        Ok(self.take(32)?.try_into().unwrap())
    }
    fn rest(&mut self) -> &'a [u8] {
        let s = &self.b[self.pos..];
        self.pos = self.b.len();
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip() {
        let root = [7u8; 32];
        for m in [
            Msg::Request {
                root,
                units: vec![0..3, 10..11],
            },
            Msg::Data {
                root,
                unit: 5,
                offset: 100,
                total: 2000,
                payload: vec![1, 2, 3],
            },
            Msg::Done {
                root,
                item: Vec::new(),
            },
            Msg::Done {
                root,
                item: b"take1-mic".to_vec(),
            },
            Msg::Error { root, code: 2 },
        ] {
            assert_eq!(Msg::decode(&m.encode()).unwrap(), m);
        }
        assert!(Msg::decode(&[0x01, 1, 2]).is_err());
        assert!(Msg::decode(&[0x09; 40]).is_err());
    }
}
