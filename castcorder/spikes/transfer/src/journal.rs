//! Append-only progress journal.
//!
//! ```text
//! header (64 bytes):
//!   magic "CCXFER01" | root[32] | size:u64 | block_log:u8 | unit_log:u8 | 0[6] | check:u64
//!   check = first 8 bytes of BLAKE3(the 56 bytes before it)
//! record (16 bytes, repeated):
//!   unit:u64 | check:u64
//!   check = first 8 bytes of BLAKE3("rec" | root | unit)
//! ```
//!
//! Loading stops at the first short or invalid record; everything after it
//! is discarded (a torn append from a crash). Because a record is appended
//! only after the unit's data is synced, every valid record is trustworthy
//! (modulo later disk corruption, which the final whole-file hash catches).

use std::io;

use crate::storage::Storage;

pub const MAGIC: &[u8; 8] = b"CCXFER01";
pub const HEADER_LEN: usize = 64;
pub const RECORD_LEN: usize = 16;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JournalHeader {
    pub root: [u8; 32],
    pub size: u64,
    pub block_log: u8,
    pub unit_log: u8,
}

impl JournalHeader {
    pub fn encode(&self) -> [u8; HEADER_LEN] {
        let mut b = [0u8; HEADER_LEN];
        b[0..8].copy_from_slice(MAGIC);
        b[8..40].copy_from_slice(&self.root);
        b[40..48].copy_from_slice(&self.size.to_le_bytes());
        b[48] = self.block_log;
        b[49] = self.unit_log;
        let c = blake3::hash(&b[..56]);
        b[56..64].copy_from_slice(&c.as_bytes()[..8]);
        b
    }
}

pub fn record(root: &[u8; 32], unit: u64) -> [u8; RECORD_LEN] {
    let mut b = [0u8; RECORD_LEN];
    b[..8].copy_from_slice(&unit.to_le_bytes());
    b[8..].copy_from_slice(&record_check(root, unit));
    b
}

fn record_check(root: &[u8; 32], unit: u64) -> [u8; 8] {
    let mut h = blake3::Hasher::new();
    h.update(b"rec");
    h.update(root);
    h.update(&unit.to_le_bytes());
    h.finalize().as_bytes()[..8].try_into().unwrap()
}

pub struct Loaded {
    pub have: Vec<bool>,
    /// Length of the valid prefix of the journal.
    pub valid_len: u64,
}

/// Returns None if there is no valid header for exactly this blob.
pub fn load<S: Storage + ?Sized>(
    j: &S,
    expect: &JournalHeader,
    n_units: u64,
) -> io::Result<Option<Loaded>> {
    let len = j.len()?;
    if len < HEADER_LEN as u64 {
        return Ok(None);
    }
    let mut buf = vec![0u8; len as usize];
    j.read_exact_at(0, &mut buf)?;
    if buf[..HEADER_LEN] != expect.encode() {
        return Ok(None);
    }
    let mut have = vec![false; n_units as usize];
    let mut pos = HEADER_LEN;
    while pos + RECORD_LEN <= buf.len() {
        let unit = u64::from_le_bytes(buf[pos..pos + 8].try_into().unwrap());
        if unit >= n_units || buf[pos + 8..pos + 16] != record_check(&expect.root, unit) {
            break;
        }
        have[unit as usize] = true;
        pos += RECORD_LEN;
    }
    Ok(Some(Loaded {
        have,
        valid_len: pos as u64,
    }))
}

/// Truncate the journal and write a fresh header. Returns the new length.
pub fn write_header<S: Storage + ?Sized>(j: &mut S, h: &JournalHeader) -> io::Result<u64> {
    j.set_len(0)?;
    j.write_at(0, &h.encode())?;
    j.sync()?;
    Ok(HEADER_LEN as u64)
}
