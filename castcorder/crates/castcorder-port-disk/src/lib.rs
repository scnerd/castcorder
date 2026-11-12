//! The disk port: the only way the core stores files (ADR 0003).
//!
//! - [`Storage`] is a directory of named files, with named subdirectories
//!   ([`Storage::subdir`]); [`StorageFile`] is one open file with positioned reads
//!   and writes, length, truncation, and `sync`.
//! - [`FsStorage`] is the real implementation: std fs in one directory.
//! - [`SimDisk`] is the simulated one: in memory, with [`SimDisk::crash`] to model
//!   power loss.
//! - [`contract`] is the behaviour both must show; it runs against each in
//!   `tests/contract.rs`.
//!
//! Durability rule (what [`SimDisk::crash`] keeps): a file's contents as of its last
//! `sync` survive; anything written after that is lost, except that the last write
//! may survive torn (a prefix of it applied).
//!
//! Directory entries are separate, as on a real filesystem (Linux ext4, for
//! example): a newly created file survives a crash only once [`Storage::sync_dir`]
//! has run on its directory (then with whatever contents were synced, or empty),
//! and a removed file comes back unless `sync_dir` ran after the remove. Code that
//! relies on a create or a remove surviving a crash (receiving into a final name,
//! deleting after confirmed receipt) calls `sync_dir` afterwards. Creating a
//! directory with [`Storage::subdir`] is durable at once.
//!
//! Names are flat and portable: 1 to 128 characters from `A-Z a-z 0-9 . _ -`, not
//! starting with `.`, and not a Windows reserved device name (`con`, `nul`, `com1`,
//! ...), so a name valid here is valid on every platform. Removing a file that still
//! has an open handle is unspecified (Windows refuses it).

use std::io;

pub mod contract;
mod fs;
mod sim;

pub use fs::{FsFile, FsStorage};
pub use sim::{SimDisk, SimFile};

/// A directory of named files. Subdirectories are storages of their own.
pub trait Storage: Send + Sync {
    /// An open file in this storage.
    type File: StorageFile;
    /// Open a file for reading and writing, creating it empty if it does not exist.
    fn open(&self, name: &str) -> io::Result<Self::File>;
    /// Whether a file with this name exists.
    fn exists(&self, name: &str) -> io::Result<bool>;
    /// Delete a file. Fails with `NotFound` if it does not exist.
    fn remove(&self, name: &str) -> io::Result<()>;
    /// The names of every file directly in this directory (not in subdirectories),
    /// sorted.
    fn list(&self) -> io::Result<Vec<String>>;
    /// The subdirectory `name` as a storage of its own, created (durably) if it
    /// does not exist. Subdirectory names follow the file-name rule.
    fn subdir(&self, name: &str) -> io::Result<Self>
    where
        Self: Sized;
    /// Make every create and remove of a file in this directory so far durable:
    /// the directory-entry counterpart of [`StorageFile::sync`].
    fn sync_dir(&self) -> io::Result<()>;
}

/// One open file.
pub trait StorageFile: Send {
    /// Write all of `data` at `offset`. Writing past the end extends the file, and
    /// any gap reads back as zeros.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    /// Read into `buf` from `offset`, returning how many bytes were read. Fills
    /// `buf` unless the end of the file is reached; returns 0 at or past the end.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// The file's current length in bytes, including unsynced writes.
    fn len(&self) -> io::Result<u64>;
    /// Whether the file is empty.
    fn is_empty(&self) -> io::Result<bool> {
        Ok(self.len()? == 0)
    }
    /// Truncate or extend (with zeros) to `len` bytes.
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    /// Make everything written so far durable.
    fn sync(&mut self) -> io::Result<()>;
}

const RESERVED: &[&str] = &[
    "con", "prn", "aux", "nul", "com1", "com2", "com3", "com4", "com5", "com6", "com7", "com8",
    "com9", "lpt1", "lpt2", "lpt3", "lpt4", "lpt5", "lpt6", "lpt7", "lpt8", "lpt9",
];

/// Check a file name against the portable naming rule in the crate docs.
pub fn validate_name(name: &str) -> io::Result<()> {
    let stem = name.split('.').next().unwrap_or("").to_ascii_lowercase();
    let ok = (1..=128).contains(&name.len())
        && !name.starts_with('.')
        && name
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'_' || b == b'-')
        && !RESERVED.contains(&stem.as_str());
    if ok {
        Ok(())
    } else {
        Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            format!("invalid storage file name: {name:?}"),
        ))
    }
}
