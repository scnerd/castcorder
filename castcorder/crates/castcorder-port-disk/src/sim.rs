//! The simulated disk: in memory, with power loss.

use std::collections::BTreeMap;
use std::io;
use std::sync::{Arc, Mutex, MutexGuard};

use crate::{Storage, StorageFile, validate_name};

#[derive(Debug, Default)]
struct FileState {
    /// What reads see now.
    data: Vec<u8>,
    /// What survives a crash; `None` if never synced.
    durable: Option<Vec<u8>>,
    /// The most recent operation since the last sync, if it was a write.
    last_write: Option<(u64, Vec<u8>)>,
    /// Whether the directory entry survives a crash (set by `sync_dir`).
    entry_durable: bool,
}

#[derive(Debug)]
struct DiskState {
    /// Every file on the disk, keyed by its full path (`dir/sub/name`).
    files: BTreeMap<String, FileState>,
    /// Removed files whose removal is not yet durable: the durable contents a
    /// crash brings back, keyed by full path.
    ghosts: BTreeMap<String, Vec<u8>>,
    /// Bumped by every crash; handles from an older generation are dead.
    generation: u64,
    rng: u64,
}

impl DiskState {
    /// splitmix64: same seed, same tears, every platform.
    fn next_random(&mut self) -> u64 {
        self.rng = self.rng.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.rng;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }
}

/// An in-memory disk. Cloning shares the same disk (and the same directory).
#[derive(Clone, Debug)]
pub struct SimDisk {
    state: Arc<Mutex<DiskState>>,
    /// The directory this handle looks at: empty for the top, else `a/b/`.
    prefix: String,
}

impl SimDisk {
    /// An empty disk. `seed` decides how torn writes tear on [`crash`](Self::crash).
    pub fn new(seed: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(DiskState {
                files: BTreeMap::new(),
                ghosts: BTreeMap::new(),
                generation: 0,
                rng: seed,
            })),
            prefix: String::new(),
        }
    }

    fn lock(&self) -> MutexGuard<'_, DiskState> {
        self.state.lock().expect("sim disk lock poisoned")
    }

    fn key(&self, name: &str) -> io::Result<String> {
        validate_name(name)?;
        Ok(format!("{}{name}", self.prefix))
    }

    /// Whether `key` names a file directly in this handle's directory.
    fn is_mine(&self, key: &str) -> bool {
        key.strip_prefix(&self.prefix)
            .is_some_and(|rest| !rest.contains('/'))
    }

    /// Simulate power loss on the whole disk (every directory): every file reverts
    /// to its durable contents (see the crate docs), possibly plus a torn prefix of
    /// its last unsynced write. Files whose directory entry was never made durable
    /// with [`Storage::sync_dir`] vanish; files removed since the last `sync_dir`
    /// come back. Every open handle becomes unusable; open the files again.
    pub fn crash(&self) {
        let mut s = self.lock();
        s.generation += 1;
        let files = std::mem::take(&mut s.files);
        for (name, file) in files {
            if !file.entry_durable {
                continue;
            }
            let mut data = file.durable.unwrap_or_default();
            if let Some((offset, bytes)) = file.last_write {
                let keep = (s.next_random() % (bytes.len() as u64 + 1)) as usize;
                if keep > 0 {
                    write_into(&mut data, offset, &bytes[..keep]);
                }
            }
            s.files.insert(name, durable_file(data));
        }
        let ghosts = std::mem::take(&mut s.ghosts);
        for (name, data) in ghosts {
            s.files.entry(name).or_insert_with(|| durable_file(data));
        }
    }
}

fn durable_file(data: Vec<u8>) -> FileState {
    FileState {
        durable: Some(data.clone()),
        data,
        last_write: None,
        entry_durable: true,
    }
}

fn write_into(data: &mut Vec<u8>, offset: u64, bytes: &[u8]) {
    let start = usize::try_from(offset).expect("offset fits in memory");
    let end = start + bytes.len();
    if data.len() < end {
        data.resize(end, 0);
    }
    data[start..end].copy_from_slice(bytes);
}

impl Storage for SimDisk {
    type File = SimFile;

    fn open(&self, name: &str) -> io::Result<SimFile> {
        let key = self.key(name)?;
        let mut s = self.lock();
        s.files.entry(key.clone()).or_default();
        Ok(SimFile {
            disk: self.clone(),
            key,
            generation: s.generation,
        })
    }

    fn exists(&self, name: &str) -> io::Result<bool> {
        let key = self.key(name)?;
        Ok(self.lock().files.contains_key(&key))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        let key = self.key(name)?;
        let mut s = self.lock();
        match s.files.remove(&key) {
            Some(f) => {
                if f.entry_durable {
                    s.ghosts.insert(key, f.durable.unwrap_or_default());
                }
                Ok(())
            }
            None => Err(io::ErrorKind::NotFound.into()),
        }
    }

    fn list(&self) -> io::Result<Vec<String>> {
        // BTreeMap keys are already sorted, and a common prefix keeps that order.
        Ok(self
            .lock()
            .files
            .keys()
            .filter(|k| self.is_mine(k))
            .map(|k| k[self.prefix.len()..].to_owned())
            .collect())
    }

    fn subdir(&self, name: &str) -> io::Result<SimDisk> {
        let key = self.key(name)?;
        Ok(SimDisk {
            state: self.state.clone(),
            prefix: format!("{key}/"),
        })
    }

    fn sync_dir(&self) -> io::Result<()> {
        let mut s = self.lock();
        let s = &mut *s;
        for (k, f) in s.files.iter_mut() {
            if self.is_mine(k) {
                f.entry_durable = true;
            }
        }
        s.ghosts.retain(|k, _| !self.is_mine(k));
        Ok(())
    }
}

/// An open file on a [`SimDisk`].
#[derive(Debug)]
pub struct SimFile {
    disk: SimDisk,
    key: String,
    generation: u64,
}

impl SimFile {
    fn with<T>(&self, f: impl FnOnce(&mut FileState) -> T) -> io::Result<T> {
        let mut s = self.disk.lock();
        if s.generation != self.generation {
            return Err(io::Error::other(
                "sim disk crashed since this file was opened",
            ));
        }
        let file = s.files.get_mut(&self.key).ok_or(io::ErrorKind::NotFound)?;
        Ok(f(file))
    }
}

impl StorageFile for SimFile {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.with(|f| {
            write_into(&mut f.data, offset, data);
            f.last_write = Some((offset, data.to_vec()));
        })
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.with(|f| {
            let start = usize::try_from(offset)
                .unwrap_or(usize::MAX)
                .min(f.data.len());
            let n = buf.len().min(f.data.len() - start);
            buf[..n].copy_from_slice(&f.data[start..start + n]);
            n
        })
    }

    fn len(&self) -> io::Result<u64> {
        self.with(|f| f.data.len() as u64)
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.with(|f| {
            f.data
                .resize(usize::try_from(len).expect("length fits in memory"), 0);
            f.last_write = None;
        })
    }

    fn sync(&mut self) -> io::Result<()> {
        self.with(|f| {
            f.durable = Some(f.data.clone());
            f.last_write = None;
        })
    }
}
