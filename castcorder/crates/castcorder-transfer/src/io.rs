//! Adaptors between the disk port ([`StorageFile`]) and bao-tree's positioned-io traits.
//!
//! Some backends (OPFS sync access handles in Chrome) cost 150-700 us per call
//! regardless of size, so the sender reads a whole unit with one call and the
//! receiver writes a whole unit with one call (plus a few coalesced outboard
//! writes) instead of one call per 16 KiB group or 64-byte hash pair.

use std::cell::RefCell;
use std::io;

use bao_tree::io::sync::{ReadAt, Size, WriteAt};
use castcorder_port_disk::StorageFile;

/// Fill `buf` from `pos`, failing with `UnexpectedEof` if the file is too short.
pub fn read_exact_at<F: StorageFile + ?Sized>(
    f: &mut F,
    mut pos: u64,
    mut buf: &mut [u8],
) -> io::Result<()> {
    while !buf.is_empty() {
        let n = f.read_at(pos, buf)?;
        if n == 0 {
            return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
        }
        pos += n as u64;
        buf = &mut buf[n..];
    }
    Ok(())
}

/// Read-only adaptor from a [`StorageFile`] to bao-tree's `ReadAt` and `Size`.
///
/// bao-tree reads through `&self` while the port reads through `&mut self`, so
/// the borrow sits in a `RefCell`. Single-threaded by construction.
pub struct At<'a, F: ?Sized>(RefCell<&'a mut F>);

impl<'a, F: ?Sized> At<'a, F> {
    /// Wrap a file for reading.
    pub fn new(f: &'a mut F) -> Self {
        Self(RefCell::new(f))
    }
}

impl<F: StorageFile + ?Sized> ReadAt for At<'_, F> {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.0.borrow_mut().read_at(pos, buf)
    }
}

impl<F: StorageFile + ?Sized> Size for At<'_, F> {
    fn size(&self) -> io::Result<Option<u64>> {
        Ok(Some(self.0.borrow().len()?))
    }
}

/// A read-only window `[base, base + data.len())` of a larger file.
pub struct Window<'a> {
    /// File offset of `data[0]`.
    pub base: u64,
    /// The bytes of the window.
    pub data: &'a [u8],
}

impl ReadAt for Window<'_> {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        if pos < self.base {
            return Err(io::Error::other("read before window"));
        }
        let off = (pos - self.base) as usize;
        if off >= self.data.len() {
            return Ok(0);
        }
        let n = (self.data.len() - off).min(buf.len());
        buf[..n].copy_from_slice(&self.data[off..off + n]);
        Ok(n)
    }
}

/// Collects positioned writes in memory; `flush_to` coalesces adjacent writes
/// and applies them to a file.
#[derive(Default)]
pub struct WriteBatch {
    writes: Vec<(u64, Vec<u8>)>,
}

impl WriteBatch {
    /// Apply the collected writes, sorted and coalesced; returns the number of
    /// `write_at` calls made.
    pub fn flush_to<F: StorageFile + ?Sized>(&mut self, f: &mut F) -> io::Result<usize> {
        self.writes.sort_by_key(|w| w.0);
        let mut calls = 0;
        let mut cur: Option<(u64, Vec<u8>)> = None;
        for (pos, data) in self.writes.drain(..) {
            match &mut cur {
                Some((p, d)) if *p + d.len() as u64 == pos => d.extend_from_slice(&data),
                _ => {
                    if let Some((p, d)) = cur.take() {
                        f.write_at(p, &d)?;
                        calls += 1;
                    }
                    cur = Some((pos, data));
                }
            }
        }
        if let Some((p, d)) = cur {
            f.write_at(p, &d)?;
            calls += 1;
        }
        Ok(calls)
    }
}

impl ReadAt for WriteBatch {
    fn read_at(&self, _pos: u64, _buf: &mut [u8]) -> io::Result<usize> {
        Err(io::Error::other("WriteBatch is write-only"))
    }
}

impl WriteAt for WriteBatch {
    fn write_at(&mut self, pos: u64, buf: &[u8]) -> io::Result<usize> {
        self.writes.push((pos, buf.to_vec()));
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}
