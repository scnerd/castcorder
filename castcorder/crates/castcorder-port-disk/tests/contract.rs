//! Runs the disk contract against real std fs (in a temp directory) and the
//! simulated disk.

use std::path::PathBuf;
use std::sync::atomic::{AtomicU32, Ordering};

use castcorder_port_disk::contract::{self, DiskHarness};
use castcorder_port_disk::{FsStorage, SimDisk};

/// A unique directory under the system temp dir, removed on drop.
struct Real {
    root: PathBuf,
}

impl Real {
    fn new() -> Self {
        static NEXT: AtomicU32 = AtomicU32::new(0);
        let root = std::env::temp_dir().join(format!(
            "castcorder-port-disk-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::SeqCst)
        ));
        let _ = std::fs::remove_dir_all(&root);
        Self { root }
    }
}

impl Drop for Real {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

impl DiskHarness for Real {
    type Storage = FsStorage;
    fn storage(&self) -> FsStorage {
        FsStorage::new(&self.root).unwrap()
    }
}

struct Sim(SimDisk);

impl DiskHarness for Sim {
    type Storage = SimDisk;
    fn storage(&self) -> SimDisk {
        self.0.clone()
    }
}

macro_rules! contract_tests {
    ($module:ident, $harness:expr) => {
        mod $module {
            use super::*;
            #[test]
            fn open_creates_empty_file() {
                contract::open_creates_empty_file(&$harness);
            }
            #[test]
            fn positioned_writes_read_back() {
                contract::positioned_writes_read_back(&$harness);
            }
            #[test]
            fn reads_stop_at_end() {
                contract::reads_stop_at_end(&$harness);
            }
            #[test]
            fn set_len_truncates_and_extends() {
                contract::set_len_truncates_and_extends(&$harness);
            }
            #[test]
            fn data_is_shared_across_handles() {
                contract::data_is_shared_across_handles(&$harness);
            }
            #[test]
            fn list_and_remove() {
                contract::list_and_remove(&$harness);
            }
            #[test]
            fn invalid_names_are_refused() {
                contract::invalid_names_are_refused(&$harness);
            }
        }
    };
}

contract_tests!(real, Real::new());
contract_tests!(sim, Sim(SimDisk::new(1)));
