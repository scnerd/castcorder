//! The disk contract: behaviour every [`Storage`] implementation must show (with no
//! crash involved; power loss is simulation-only).

use std::io::ErrorKind;

use crate::{Storage, StorageFile};

/// Gives handles onto one fresh, empty backing store per harness.
pub trait DiskHarness {
    /// The implementation under test.
    type Storage: Storage;
    /// A new handle onto this harness's backing store (as if the process restarted
    /// cleanly and opened the same directory).
    fn storage(&self) -> Self::Storage;
}

fn read_all<F: StorageFile>(f: &mut F) -> Vec<u8> {
    let mut buf = vec![0u8; f.len().unwrap() as usize];
    assert_eq!(f.read_at(0, &mut buf).unwrap(), buf.len());
    buf
}

/// Opening a missing file creates it empty; opening again keeps it.
pub fn open_creates_empty_file<H: DiskHarness>(h: &H) {
    let s = h.storage();
    assert!(!s.exists("a.bin").unwrap());
    let f = s.open("a.bin").unwrap();
    assert!(f.is_empty().unwrap());
    assert!(s.exists("a.bin").unwrap());
    assert_eq!(s.list().unwrap(), vec!["a.bin".to_string()]);
}

/// Positioned writes read back, overwrite in place, and zero-fill gaps.
pub fn positioned_writes_read_back<H: DiskHarness>(h: &H) {
    let s = h.storage();
    let mut f = s.open("track.wav").unwrap();
    f.write_at(0, b"hello world").unwrap();
    f.write_at(6, b"WORLD").unwrap();
    assert_eq!(read_all(&mut f), b"hello WORLD");
    f.write_at(14, b"!").unwrap();
    assert_eq!(f.len().unwrap(), 15);
    assert_eq!(read_all(&mut f), b"hello WORLD\0\0\0!");
    let mut mid = [0u8; 3];
    assert_eq!(f.read_at(2, &mut mid).unwrap(), 3);
    assert_eq!(&mid, b"llo");
}

/// Reads stop at the end of the file.
pub fn reads_stop_at_end<H: DiskHarness>(h: &H) {
    let s = h.storage();
    let mut f = s.open("short").unwrap();
    f.write_at(0, b"abc").unwrap();
    let mut buf = [0xaau8; 8];
    assert_eq!(f.read_at(1, &mut buf).unwrap(), 2);
    assert_eq!(&buf[..2], b"bc");
    assert_eq!(f.read_at(3, &mut buf).unwrap(), 0);
    assert_eq!(f.read_at(100, &mut buf).unwrap(), 0);
}

/// `set_len` truncates and extends with zeros.
pub fn set_len_truncates_and_extends<H: DiskHarness>(h: &H) {
    let s = h.storage();
    let mut f = s.open("f").unwrap();
    f.write_at(0, b"abcdef").unwrap();
    f.set_len(2).unwrap();
    assert_eq!(read_all(&mut f), b"ab");
    f.set_len(4).unwrap();
    assert_eq!(read_all(&mut f), b"ab\0\0");
}

/// Data (synced or not) is visible through a second handle and a new storage handle.
pub fn data_is_shared_across_handles<H: DiskHarness>(h: &H) {
    let s = h.storage();
    let mut f = s.open("shared").unwrap();
    f.write_at(0, b"unsynced").unwrap();
    let mut g = s.open("shared").unwrap();
    assert_eq!(read_all(&mut g), b"unsynced");
    f.write_at(0, b"UN").unwrap();
    f.sync().unwrap();
    drop((f, g));
    let mut again = h.storage().open("shared").unwrap();
    assert_eq!(read_all(&mut again), b"UNsynced");
}

/// `list` is sorted; `remove` deletes, and fails with `NotFound` when missing.
pub fn list_and_remove<H: DiskHarness>(h: &H) {
    let s = h.storage();
    for name in ["c-3.flac", "a_1.wav", "b.2"] {
        s.open(name).unwrap();
    }
    assert_eq!(s.list().unwrap(), vec!["a_1.wav", "b.2", "c-3.flac"]);
    s.remove("b.2").unwrap();
    assert_eq!(s.list().unwrap(), vec!["a_1.wav", "c-3.flac"]);
    assert!(!s.exists("b.2").unwrap());
    assert_eq!(s.remove("b.2").unwrap_err().kind(), ErrorKind::NotFound);
}

/// Names outside the portable rule are refused with `InvalidInput`, everywhere.
pub fn invalid_names_are_refused<H: DiskHarness>(h: &H) {
    let s = h.storage();
    let long = "x".repeat(129);
    for bad in [
        "", ".hidden", "../up", "a/b", "a\\b", "c:x", "sp ace", "nul.wav", "CON", "com1.txt", &long,
    ] {
        let kind = s.open(bad).err().map(|e| e.kind());
        assert_eq!(kind, Some(ErrorKind::InvalidInput), "{bad:?}");
        assert_eq!(s.exists(bad).unwrap_err().kind(), ErrorKind::InvalidInput);
    }
    assert!(s.list().unwrap().is_empty());
}

/// A subdirectory is a storage of its own: its files are not listed in the
/// parent, the same name can exist in both, and it is there for a new handle.
pub fn subdirs_are_separate<H: DiskHarness>(h: &H) {
    let s = h.storage();
    s.open("top.wav").unwrap().write_at(0, b"top").unwrap();
    let call = s.subdir("call1").unwrap();
    let take = call.subdir("take01").unwrap();
    let mut f = take.open("top.wav").unwrap();
    f.write_at(0, b"nested").unwrap();
    f.sync().unwrap();
    drop(f);
    assert_eq!(s.list().unwrap(), vec!["top.wav"]);
    assert!(call.list().unwrap().is_empty());
    assert_eq!(take.list().unwrap(), vec!["top.wav"]);
    let again = h
        .storage()
        .subdir("call1")
        .unwrap()
        .subdir("take01")
        .unwrap();
    assert_eq!(read_all(&mut again.open("top.wav").unwrap()), b"nested");
    assert_eq!(read_all(&mut s.open("top.wav").unwrap()), b"top");
    take.remove("top.wav").unwrap();
    assert!(s.exists("top.wav").unwrap());
    let kind = s.subdir("../escape").err().map(|e| e.kind());
    assert_eq!(kind, Some(ErrorKind::InvalidInput));
}

/// `sync_dir` succeeds after creates and removes, in the top directory and in a
/// subdirectory, and changes nothing that reads see.
pub fn sync_dir_keeps_what_reads_see<H: DiskHarness>(h: &H) {
    let s = h.storage();
    let sub = s.subdir("d").unwrap();
    for st in [&s, &sub] {
        st.open("keep").unwrap().write_at(0, b"k").unwrap();
        st.open("gone").unwrap();
        st.remove("gone").unwrap();
        st.sync_dir().unwrap();
        assert_eq!(st.list().unwrap(), vec!["keep"]);
        assert_eq!(read_all(&mut st.open("keep").unwrap()), b"k");
    }
}

/// `rename` moves contents to the new name, replaces an existing target, and
/// fails with `NotFound` when the source is missing.
pub fn rename_moves_and_replaces<H: DiskHarness>(h: &H) {
    let s = h.storage();
    s.open("a.part").unwrap().write_at(0, b"new").unwrap();
    s.open("a").unwrap().write_at(0, b"old data").unwrap();
    s.rename("a.part", "a").unwrap();
    assert_eq!(s.list().unwrap(), vec!["a"]);
    assert_eq!(read_all(&mut s.open("a").unwrap()), b"new");
    let missing = s.rename("a.part", "b").unwrap_err();
    assert_eq!(missing.kind(), ErrorKind::NotFound);
    s.rename("a", "b").unwrap();
    assert_eq!(s.list().unwrap(), vec!["b"]);
    let kind = s.rename("b", "../x").err().map(|e| e.kind());
    assert_eq!(kind, Some(ErrorKind::InvalidInput));
    s.sync_dir().unwrap();
}

/// `free_space` reports a volume with room (more than a byte) where the
/// platform can say; where it cannot, it says `Unsupported`.
pub fn free_space_is_reported<H: DiskHarness>(h: &H) {
    match h.storage().free_space() {
        Ok(bytes) => assert!(bytes > 0, "no free space reported"),
        Err(e) => assert_eq!(e.kind(), ErrorKind::Unsupported),
    }
}
