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
}

#[derive(Debug)]
struct DiskState {
    files: BTreeMap<String, FileState>,
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

/// An in-memory disk. Cloning shares the same disk.
#[derive(Clone, Debug)]
pub struct SimDisk {
    state: Arc<Mutex<DiskState>>,
}

impl SimDisk {
    /// An empty disk. `seed` decides how torn writes tear on [`crash`](Self::crash).
    pub fn new(seed: u64) -> Self {
        Self {
            state: Arc::new(Mutex::new(DiskState {
                files: BTreeMap::new(),
                generation: 0,
                rng: seed,
            })),
        }
    }

    fn lock(&self) -> MutexGuard<'_, DiskState> {
        self.state.lock().expect("sim disk lock poisoned")
    }

    /// Simulate power loss: every file reverts to its durable contents (see the crate
    /// docs), possibly plus a torn prefix of its last unsynced write. Files never
    /// synced vanish. Every open handle becomes unusable; open the files again.
    pub fn crash(&self) {
        let mut s = self.lock();
        s.generation += 1;
        let names: Vec<String> = s.files.keys().cloned().collect();
        for name in names {
            let file = s.files.remove(&name).expect("name came from the map");
            let Some(mut data) = file.durable else {
                continue;
            };
            if let Some((offset, bytes)) = file.last_write {
                let keep = (s.next_random() % (bytes.len() as u64 + 1)) as usize;
                if keep > 0 {
                    write_into(&mut data, offset, &bytes[..keep]);
                }
            }
            s.files.insert(
                name,
                FileState {
                    durable: Some(data.clone()),
                    data,
                    last_write: None,
                },
            );
        }
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
        validate_name(name)?;
        let mut s = self.lock();
        s.files.entry(name.to_owned()).or_default();
        Ok(SimFile {
            disk: self.clone(),
            name: name.to_owned(),
            generation: s.generation,
        })
    }

    fn exists(&self, name: &str) -> io::Result<bool> {
        validate_name(name)?;
        Ok(self.lock().files.contains_key(name))
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        validate_name(name)?;
        match self.lock().files.remove(name) {
            Some(_) => Ok(()),
            None => Err(io::ErrorKind::NotFound.into()),
        }
    }

    fn list(&self) -> io::Result<Vec<String>> {
        // BTreeMap keys are already sorted.
        Ok(self.lock().files.keys().cloned().collect())
    }
}

/// An open file on a [`SimDisk`].
#[derive(Debug)]
pub struct SimFile {
    disk: SimDisk,
    name: String,
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
        let file = s.files.get_mut(&self.name).ok_or(io::ErrorKind::NotFound)?;
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
