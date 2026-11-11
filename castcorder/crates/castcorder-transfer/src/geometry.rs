//! Shared geometry: how a blob is split into bao chunk groups and transfer units.

use std::ops::Range;

use bao_tree::{BaoTree, BlockSize, ChunkNum, ChunkRanges};

/// Bao chunk group: 16 KiB (chunk_log 4). Same default as iroh-blobs.
pub const DEFAULT_BLOCK_LOG: u8 = 4;
/// Transfer unit: 1 MiB. The unit of request, verification and progress.
pub const DEFAULT_UNIT_LOG: u8 = 20;
/// Data channel message payload: 64 KiB (+49 bytes header).
pub const DEFAULT_FRAME_PAYLOAD: usize = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Geometry {
    pub size: u64,
    pub block_log: u8,
    /// log2 of the unit size in bytes. Must be >= block_log + 10.
    pub unit_log: u8,
}

impl Geometry {
    pub fn new(size: u64, block_log: u8, unit_log: u8) -> Self {
        assert!(
            unit_log >= block_log + 10,
            "unit must be a whole number of chunk groups"
        );
        assert!(unit_log <= 30, "unit too large for u32 framing");
        Self {
            size,
            block_log,
            unit_log,
        }
    }

    pub fn with_defaults(size: u64) -> Self {
        Self::new(size, DEFAULT_BLOCK_LOG, DEFAULT_UNIT_LOG)
    }

    pub fn block_size(&self) -> BlockSize {
        BlockSize::from_chunk_log(self.block_log)
    }

    pub fn tree(&self) -> BaoTree {
        BaoTree::new(self.size, self.block_size())
    }

    pub fn unit_bytes(&self) -> u64 {
        1u64 << self.unit_log
    }

    pub fn n_units(&self) -> u64 {
        self.size.div_ceil(self.unit_bytes())
    }

    pub fn unit_byte_range(&self, unit: u64) -> Range<u64> {
        let s = unit * self.unit_bytes();
        s..(s + self.unit_bytes()).min(self.size)
    }

    /// Chunk ranges for one unit. Always chunk-group aligned except possibly
    /// at the very end of the blob, where it ends at the last chunk.
    pub fn unit_chunks(&self, unit: u64) -> ChunkRanges {
        let per = 1u64 << (self.unit_log - 10);
        let total = self.tree().chunks().0;
        let s = unit * per;
        let e = (s + per).min(total);
        ChunkRanges::from(ChunkNum(s)..ChunkNum(e))
    }

    /// Upper bound on the encoded size of one unit (data + parent pairs on
    /// the path from the root). Used by the receiver to reject absurd totals.
    pub fn max_unit_encoding(&self) -> u64 {
        // pairs inside the unit (one per chunk group, upper bound) plus at
        // most 64 pairs on the path from the root; each pair is 64 bytes
        let groups = self.unit_bytes() >> (self.block_log as u64 + 10);
        self.unit_bytes() + 64 * groups + 64 * 64
    }
}
