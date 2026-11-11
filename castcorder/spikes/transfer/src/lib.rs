//! Castcorder transfer spike: verified, resumable blob transfer over an
//! ordered reliable message channel, built on bao-tree (BLAKE3 verified
//! streaming). Same code runs natively and in browser WASM.

pub mod channel;
pub mod geometry;
pub mod incremental;
pub mod journal;
pub mod receiver;
pub mod sender;
pub mod storage;
pub mod wire;

#[cfg(target_arch = "wasm32")]
pub mod wasm;

pub use bao_tree;
pub use blake3;

use bao_tree::io::outboard::PostOrderMemOutboard;
use bao_tree::BlockSize;

/// Deterministic pseudo-random bytes (splitmix64), identical on every
/// platform, so native and browser tests use the same data.
pub fn fill_pseudo_random(seed: u64, offset: u64, buf: &mut [u8]) {
    assert!(offset % 8 == 0);
    let mut i = offset / 8;
    for c in buf.chunks_mut(8) {
        let v = splitmix64(seed ^ i.wrapping_mul(0x9E37_79B9_7F4A_7C15)).to_le_bytes();
        c.copy_from_slice(&v[..c.len()]);
        i += 1;
    }
}

pub fn splitmix64(mut z: u64) -> u64 {
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

/// Tiny PRNG for test decisions (cut points, bit positions).
pub struct Rng(pub u64);

impl Rng {
    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(1);
        splitmix64(self.0)
    }
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n
    }
}

/// Build the sender-side outboard for an in-memory blob.
pub fn outboard_for(data: &[u8], block_log: u8) -> PostOrderMemOutboard {
    PostOrderMemOutboard::create(data, BlockSize::from_chunk_log(block_log))
}
