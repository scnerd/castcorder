//! Random-access storage abstraction.
//!
//! The same trait is implemented by native files, an in-memory simulated disk
//! (with power-loss simulation) and, in the browser, an OPFS
//! FileSystemSyncAccessHandle (see `wasm.rs`).

use std::cell::RefCell;
use std::io;
use std::rc::Rc;

use bao_tree::io::sync::{ReadAt, Size, WriteAt};

/// A random-access byte store. All writes are "write all".
pub trait Storage {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize>;
    fn write_at(&mut self, pos: u64, buf: &[u8]) -> io::Result<()>;
    fn len(&self) -> io::Result<u64>;
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    /// Make all previous writes durable.
    fn sync(&mut self) -> io::Result<()>;

    fn read_exact_at(&self, mut pos: u64, mut buf: &mut [u8]) -> io::Result<()> {
        while !buf.is_empty() {
            let n = self.read_at(pos, buf)?;
            if n == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "short read"));
            }
            pos += n as u64;
            buf = &mut buf[n..];
        }
        Ok(())
    }
}

impl<S: Storage + ?Sized> Storage for Box<S> {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        (**self).read_at(pos, buf)
    }
    fn write_at(&mut self, pos: u64, buf: &[u8]) -> io::Result<()> {
        (**self).write_at(pos, buf)
    }
    fn len(&self) -> io::Result<u64> {
        (**self).len()
    }
    fn set_len(&mut self, len: u64) -> io::Result<()> {
        (**self).set_len(len)
    }
    fn sync(&mut self) -> io::Result<()> {
        (**self).sync()
    }
}

/// Read-only adaptor to the positioned-io traits used by bao-tree.
pub struct At<'a, S: ?Sized>(pub &'a S);

impl<S: Storage + ?Sized> ReadAt for At<'_, S> {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_at(pos, buf)
    }
}

impl<S: Storage + ?Sized> Size for At<'_, S> {
    fn size(&self) -> io::Result<Option<u64>> {
        Ok(Some(self.0.len()?))
    }
}

/// Read-write adaptor to the positioned-io traits used by bao-tree.
pub struct AtMut<'a, S: ?Sized>(pub &'a mut S);

impl<S: Storage + ?Sized> ReadAt for AtMut<'_, S> {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.0.read_at(pos, buf)
    }
}

impl<S: Storage + ?Sized> WriteAt for AtMut<'_, S> {
    fn write_at(&mut self, pos: u64, buf: &[u8]) -> io::Result<usize> {
        self.0.write_at(pos, buf)?;
        Ok(buf.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Batching adaptors. Some backends (OPFS sync access handles in Chrome) cost
// ~150-300 us per call regardless of size, so the sender reads a whole unit
// with one call and the receiver writes a whole unit with one call (plus a
// few coalesced outboard writes) instead of one call per 16 KiB group or
// 64-byte hash pair.
// ---------------------------------------------------------------------------

/// A read-only window `[base, base + data.len())` of a larger file.
pub struct Window<'a> {
    pub base: u64,
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

/// Collects positioned writes in memory; `flush_to` coalesces adjacent
/// writes and applies them to a store.
#[derive(Default)]
pub struct WriteBatch {
    writes: Vec<(u64, Vec<u8>)>,
}

impl WriteBatch {
    pub fn clear(&mut self) {
        self.writes.clear();
    }

    pub fn flush_to<S: Storage + ?Sized>(&mut self, s: &mut S) -> io::Result<usize> {
        self.writes.sort_by_key(|w| w.0);
        let mut calls = 0;
        let mut cur: Option<(u64, Vec<u8>)> = None;
        for (pos, data) in self.writes.drain(..) {
            match &mut cur {
                Some((p, d)) if *p + d.len() as u64 == pos => d.extend_from_slice(&data),
                _ => {
                    if let Some((p, d)) = cur.take() {
                        s.write_at(p, &d)?;
                        calls += 1;
                    }
                    cur = Some((pos, data));
                }
            }
        }
        if let Some((p, d)) = cur {
            s.write_at(p, &d)?;
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

// ---------------------------------------------------------------------------
// Simulated disk
// ---------------------------------------------------------------------------

#[derive(Clone, Debug)]
enum Op {
    Write(u64, Vec<u8>),
    SetLen(u64),
}

#[derive(Default, Debug)]
struct SimInner {
    /// What a reader currently sees (durable + pending applied).
    current: Vec<u8>,
    /// What survives a power loss.
    durable: Vec<u8>,
    /// Operations since the last sync, in order.
    pending: Vec<Op>,
    /// If set, every operation fails (the "machine" is down).
    dead: bool,
    /// Writes that went through, for statistics.
    bytes_written: u64,
}

fn apply(buf: &mut Vec<u8>, op: &Op) {
    match op {
        Op::Write(pos, data) => {
            let end = *pos as usize + data.len();
            if buf.len() < end {
                buf.resize(end, 0);
            }
            buf[*pos as usize..end].copy_from_slice(data);
        }
        Op::SetLen(len) => buf.resize(*len as usize, 0),
    }
}

/// An in-memory disk whose handle can be cloned so that the contents survive
/// dropping whoever was using it. `power_loss` models a crash where only
/// synced data is guaranteed and an arbitrary prefix of unsynced operations
/// made it to disk, the last of which may be torn.
#[derive(Clone, Default, Debug)]
pub struct SimDisk(Rc<RefCell<SimInner>>);

impl SimDisk {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn from_vec(v: Vec<u8>) -> Self {
        let d = Self::default();
        {
            let mut i = d.0.borrow_mut();
            i.current = v.clone();
            i.durable = v;
        }
        d
    }

    pub fn contents(&self) -> Vec<u8> {
        self.0.borrow().current.clone()
    }

    pub fn bytes_written(&self) -> u64 {
        self.0.borrow().bytes_written
    }

    /// Simulate a power loss. `keep_ops` unsynced operations survive in full,
    /// then `torn_bytes` of the next write (if it is a write) survive.
    pub fn power_loss(&self, keep_ops: usize, torn_bytes: usize) {
        let mut i = self.0.borrow_mut();
        let mut disk = i.durable.clone();
        let pending = std::mem::take(&mut i.pending);
        for op in pending.iter().take(keep_ops) {
            apply(&mut disk, op);
        }
        if let Some(Op::Write(pos, data)) = pending.get(keep_ops) {
            let n = torn_bytes.min(data.len());
            apply(&mut disk, &Op::Write(*pos, data[..n].to_vec()));
        }
        i.current = disk.clone();
        i.durable = disk;
        i.dead = false;
    }

    pub fn pending_ops(&self) -> usize {
        self.0.borrow().pending.len()
    }

    pub fn set_dead(&self, dead: bool) {
        self.0.borrow_mut().dead = dead;
    }

    /// Flip one bit directly in the stored bytes (bit rot / bad sector).
    pub fn flip_bit(&self, pos: u64, bit: u8) {
        let mut i = self.0.borrow_mut();
        i.current[pos as usize] ^= 1 << bit;
        i.durable[pos as usize] ^= 1 << bit;
    }
}

fn dead_err() -> io::Error {
    io::Error::other("simulated disk is down")
}

impl Storage for SimDisk {
    fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
        let i = self.0.borrow();
        if i.dead {
            return Err(dead_err());
        }
        let len = i.current.len() as u64;
        if pos >= len {
            return Ok(0);
        }
        let n = ((len - pos) as usize).min(buf.len());
        buf[..n].copy_from_slice(&i.current[pos as usize..pos as usize + n]);
        Ok(n)
    }

    fn write_at(&mut self, pos: u64, buf: &[u8]) -> io::Result<()> {
        let mut i = self.0.borrow_mut();
        if i.dead {
            return Err(dead_err());
        }
        let op = Op::Write(pos, buf.to_vec());
        apply(&mut i.current, &op);
        i.pending.push(op);
        i.bytes_written += buf.len() as u64;
        Ok(())
    }

    fn len(&self) -> io::Result<u64> {
        let i = self.0.borrow();
        if i.dead {
            return Err(dead_err());
        }
        Ok(i.current.len() as u64)
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        let mut i = self.0.borrow_mut();
        if i.dead {
            return Err(dead_err());
        }
        let op = Op::SetLen(len);
        apply(&mut i.current, &op);
        i.pending.push(op);
        Ok(())
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut i = self.0.borrow_mut();
        if i.dead {
            return Err(dead_err());
        }
        i.durable = i.current.clone();
        i.pending.clear();
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// Native files
// ---------------------------------------------------------------------------

#[cfg(not(target_arch = "wasm32"))]
pub use native::FileStorage;

#[cfg(not(target_arch = "wasm32"))]
mod native {
    use super::Storage;
    use std::fs::{File, OpenOptions};
    use std::io;
    use std::path::Path;

    /// A native file. `sync` is `File::sync_data` (FlushFileBuffers on Windows).
    pub struct FileStorage {
        file: File,
    }

    impl FileStorage {
        pub fn open(path: impl AsRef<Path>) -> io::Result<Self> {
            let file = OpenOptions::new()
                .read(true)
                .write(true)
                .create(true)
                .truncate(false)
                .open(path)?;
            Ok(Self { file })
        }

        pub fn open_read(path: impl AsRef<Path>) -> io::Result<Self> {
            Ok(Self {
                file: File::open(path)?,
            })
        }
    }

    impl Storage for FileStorage {
        fn read_at(&self, pos: u64, buf: &mut [u8]) -> io::Result<usize> {
            #[cfg(windows)]
            {
                std::os::windows::fs::FileExt::seek_read(&self.file, buf, pos)
            }
            #[cfg(unix)]
            {
                std::os::unix::fs::FileExt::read_at(&self.file, buf, pos)
            }
        }

        fn write_at(&mut self, mut pos: u64, mut buf: &[u8]) -> io::Result<()> {
            while !buf.is_empty() {
                #[cfg(windows)]
                let n = std::os::windows::fs::FileExt::seek_write(&self.file, buf, pos)?;
                #[cfg(unix)]
                let n = std::os::unix::fs::FileExt::write_at(&self.file, buf, pos)?;
                if n == 0 {
                    return Err(io::Error::new(io::ErrorKind::WriteZero, "write zero"));
                }
                pos += n as u64;
                buf = &buf[n..];
            }
            Ok(())
        }

        fn len(&self) -> io::Result<u64> {
            Ok(self.file.metadata()?.len())
        }

        fn set_len(&mut self, len: u64) -> io::Result<()> {
            self.file.set_len(len)
        }

        fn sync(&mut self) -> io::Result<()> {
            self.file.sync_data()
        }
    }
}
