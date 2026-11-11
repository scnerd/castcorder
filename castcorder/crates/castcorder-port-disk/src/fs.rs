//! The real implementation: std fs in one directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use crate::{Storage, StorageFile, validate_name};

/// Files in one directory on the real filesystem.
#[derive(Clone, Debug)]
pub struct FsStorage {
    root: PathBuf,
}

impl FsStorage {
    /// Use `root` as the storage directory, creating it if needed.
    pub fn new(root: impl Into<PathBuf>) -> io::Result<Self> {
        let root = root.into();
        fs::create_dir_all(&root)?;
        Ok(Self { root })
    }

    /// The storage directory.
    pub fn root(&self) -> &Path {
        &self.root
    }

    fn path(&self, name: &str) -> io::Result<PathBuf> {
        validate_name(name)?;
        Ok(self.root.join(name))
    }
}

impl Storage for FsStorage {
    type File = FsFile;

    fn open(&self, name: &str) -> io::Result<FsFile> {
        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(self.path(name)?)?;
        Ok(FsFile { file })
    }

    fn exists(&self, name: &str) -> io::Result<bool> {
        self.path(name)?.try_exists()
    }

    fn remove(&self, name: &str) -> io::Result<()> {
        fs::remove_file(self.path(name)?)
    }

    fn list(&self) -> io::Result<Vec<String>> {
        let mut names = Vec::new();
        for entry in fs::read_dir(&self.root)? {
            let entry = entry?;
            if !entry.file_type()?.is_file() {
                continue;
            }
            if let Some(name) = entry.file_name().to_str()
                && validate_name(name).is_ok()
            {
                names.push(name.to_owned());
            }
        }
        names.sort();
        Ok(names)
    }
}

/// An open file on the real filesystem.
#[derive(Debug)]
pub struct FsFile {
    file: File,
}

impl StorageFile for FsFile {
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(offset))?;
        self.file.write_all(data)
    }

    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.file.seek(SeekFrom::Start(offset))?;
        let mut total = 0;
        while total < buf.len() {
            match self.file.read(&mut buf[total..]) {
                Ok(0) => break,
                Ok(n) => total += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(total)
    }

    fn len(&self) -> io::Result<u64> {
        Ok(self.file.metadata()?.len())
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.file.set_len(len)
    }

    fn sync(&mut self) -> io::Result<()> {
        self.file.sync_all()
    }
}
