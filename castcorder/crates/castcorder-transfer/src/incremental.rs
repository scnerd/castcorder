//! Incremental BLAKE3 root + bao post-order outboard for a file that is
//! being appended to (a recording in progress).
//!
//! Every byte is hashed exactly once, as it is appended. Memory is
//! O(log n) chaining values + one chunk group + a patchable prefix. When
//! the take ends, `finish` completes the right spine of the tree (O(log n)
//! work) and nothing has to be re-read.
//!
//! Containers usually patch their header after the fact (WAV RIFF/data
//! sizes, FLAC STREAMINFO, MP4 mdat size, Matroska segment size and
//! duration). `patch` allows overwriting bytes inside the first
//! `prefix_bytes` of the file at any time before `finish`; the prefix is kept
//! in memory and only it plus its left-spine ancestors are re-hashed.
//!
//! The output is byte-identical to `bao_tree::io::sync::outboard_post_order`
//! over the final file (tested).

use blake3::hazmat::{
    ChainingValue, HasherExt, Mode, merge_subtrees_non_root, merge_subtrees_root,
};

fn hash_group(start_chunk: u64, data: &[u8], is_root: bool) -> blake3::Hash {
    if is_root {
        blake3::hash(data)
    } else {
        let mut h = blake3::Hasher::new();
        h.set_input_offset(start_chunk * 1024);
        h.update(data);
        let cv: ChainingValue = h.finalize_non_root();
        blake3::Hash::from(cv)
    }
}

fn parent(l: &blake3::Hash, r: &blake3::Hash, is_root: bool) -> blake3::Hash {
    let l: ChainingValue = *l.as_bytes();
    let r: ChainingValue = *r.as_bytes();
    if is_root {
        merge_subtrees_root(&l, &r, Mode::Hash)
    } else {
        blake3::Hash::from(merge_subtrees_non_root(&l, &r, Mode::Hash))
    }
}

struct Entry {
    cv: blake3::Hash,
    /// number of chunk groups covered (a power of two)
    groups: u64,
}

pub struct IncrementalOutboard {
    group_bytes: usize,
    block_log: u8,
    /// groups covered by the patchable prefix (power of two, >= 1)
    prefix_groups: u64,
    buf: Vec<u8>,
    stack: Vec<Entry>,
    groups_done: u64,
    size: u64,
    /// post-order hash pairs, sequentially appended
    outboard: Vec<u8>,
    prefix: Vec<u8>,
    patched: bool,
    /// outboard offsets of left-spine pairs whose left child covers at
    /// least the prefix subtree, in emission (bottom-up) order
    spine: Vec<usize>,
}

pub struct Finished {
    pub root: blake3::Hash,
    pub size: u64,
    /// post-order outboard without size suffix, as bao-tree expects
    pub outboard: Vec<u8>,
}

impl IncrementalOutboard {
    /// `prefix_bytes` is rounded up to a power-of-two number of chunk groups.
    pub fn new(block_log: u8, prefix_bytes: usize) -> Self {
        let group_bytes = 1024usize << block_log;
        let prefix_groups = prefix_bytes
            .div_ceil(group_bytes)
            .max(1)
            .next_power_of_two() as u64;
        Self {
            group_bytes,
            block_log,
            prefix_groups,
            buf: Vec::with_capacity(group_bytes),
            stack: Vec::new(),
            groups_done: 0,
            size: 0,
            outboard: Vec::new(),
            prefix: Vec::new(),
            patched: false,
            spine: Vec::new(),
        }
    }

    pub fn block_log(&self) -> u8 {
        self.block_log
    }

    pub fn size(&self) -> u64 {
        self.size
    }

    pub fn prefix_capacity(&self) -> usize {
        (self.prefix_groups as usize) * self.group_bytes
    }

    pub fn append(&mut self, mut data: &[u8]) {
        self.size += data.len() as u64;
        let pc = self.prefix_capacity();
        if self.prefix.len() < pc {
            let n = (pc - self.prefix.len()).min(data.len());
            self.prefix.extend_from_slice(&data[..n]);
        }
        while !data.is_empty() {
            // a full group is only hashed once we know more data follows,
            // because the last group of the file may be the root
            if self.buf.len() == self.group_bytes {
                self.flush_group();
            }
            let n = (self.group_bytes - self.buf.len()).min(data.len());
            self.buf.extend_from_slice(&data[..n]);
            data = &data[n..];
        }
    }

    fn flush_group(&mut self) {
        let start_chunk = self.groups_done << self.block_log;
        let cv = hash_group(start_chunk, &self.buf, false);
        self.buf.clear();
        self.groups_done += 1;
        self.push(cv, 1);
    }

    fn push(&mut self, mut cv: blake3::Hash, mut groups: u64) {
        // merge equal-sized neighbours; safe as non-root because more data follows
        while let Some(top) = self.stack.last() {
            if top.groups != groups {
                break;
            }
            let left = self.stack.pop().unwrap();
            self.emit(&left, &cv, groups);
            cv = parent(&left.cv, &cv, false);
            groups *= 2;
        }
        self.stack.push(Entry { cv, groups });
    }

    fn emit(&mut self, left: &Entry, right: &blake3::Hash, left_groups: u64) {
        let off = self.outboard.len();
        self.outboard.extend_from_slice(left.cv.as_bytes());
        self.outboard.extend_from_slice(right.as_bytes());
        // the leftmost entry is the bottom of the stack
        let left_starts_at_zero = self.stack.is_empty();
        if left_starts_at_zero && left_groups >= self.prefix_groups {
            self.spine.push(off);
        }
    }

    /// Overwrite bytes at `offset`. Only allowed inside the prefix.
    pub fn patch(&mut self, offset: u64, bytes: &[u8]) -> Result<(), &'static str> {
        let end = offset as usize + bytes.len();
        if end > self.prefix_capacity() {
            return Err("patch outside the patchable prefix");
        }
        if end as u64 > self.size {
            return Err("patch beyond end of data");
        }
        self.prefix[offset as usize..end].copy_from_slice(bytes);
        // also patch the not-yet-hashed tail group, if the patch reaches it
        let tail_start = self.groups_done as usize * self.group_bytes;
        if end > tail_start {
            let s = (offset as usize).max(tail_start);
            self.buf[s - tail_start..end - tail_start]
                .copy_from_slice(&bytes[s - offset as usize..]);
        }
        self.patched = true;
        Ok(())
    }

    pub fn finish(mut self) -> Finished {
        let total_groups = self.groups_done + 1;
        if self.size == 0 {
            return Finished {
                root: blake3::hash(&[]),
                size: 0,
                outboard: Vec::new(),
            };
        }
        if self.patched && total_groups <= self.prefix_groups {
            // the whole file is in memory: just redo it
            let data = std::mem::take(&mut self.prefix);
            let mut fresh = IncrementalOutboard::new(self.block_log, 0);
            fresh.append(&data);
            return fresh.finish();
        }
        // last group, then the right spine bottom-up
        let start_chunk = self.groups_done << self.block_log;
        let is_root = self.stack.is_empty();
        let mut cv = hash_group(start_chunk, &self.buf, is_root);
        while let Some(left) = self.stack.pop() {
            let root = self.stack.is_empty();
            let off = self.outboard.len();
            self.outboard.extend_from_slice(left.cv.as_bytes());
            self.outboard.extend_from_slice(cv.as_bytes());
            if root && left.groups >= self.prefix_groups {
                self.spine.push(off);
            }
            cv = parent(&left.cv, &cv, root);
        }
        if self.patched {
            cv = self.rehash_prefix();
        }
        Finished {
            root: cv,
            size: self.size,
            outboard: self.outboard,
        }
    }

    /// Recompute the prefix subtree (its pairs are the first
    /// prefix_groups-1 pairs in post order) and then the left spine.
    fn rehash_prefix(&mut self) -> blake3::Hash {
        let mut sub = SubtreeBuilder::default();
        for (i, g) in self.prefix.chunks(self.group_bytes).enumerate() {
            let cv = hash_group((i as u64) << self.block_log, g, false);
            sub.push(cv, 1);
        }
        assert_eq!(sub.stack.len(), 1, "prefix must be a complete subtree");
        let n = sub.pairs.len();
        self.outboard[..n].copy_from_slice(&sub.pairs);
        let mut cv = sub.stack[0].cv;
        let last = self.spine.len();
        for (i, off) in self.spine.clone().into_iter().enumerate() {
            self.outboard[off..off + 32].copy_from_slice(cv.as_bytes());
            let right = blake3::Hash::from(
                <[u8; 32]>::try_from(&self.outboard[off + 32..off + 64]).unwrap(),
            );
            cv = parent(&cv, &right, i + 1 == last);
        }
        cv
    }
}

#[derive(Default)]
struct SubtreeBuilder {
    stack: Vec<Entry>,
    pairs: Vec<u8>,
}

impl SubtreeBuilder {
    fn push(&mut self, mut cv: blake3::Hash, mut groups: u64) {
        while let Some(top) = self.stack.last() {
            if top.groups != groups {
                break;
            }
            let left = self.stack.pop().unwrap();
            self.pairs.extend_from_slice(left.cv.as_bytes());
            self.pairs.extend_from_slice(cv.as_bytes());
            cv = parent(&left.cv, &cv, false);
            groups *= 2;
        }
        self.stack.push(Entry { cv, groups });
    }
}
