//! The browser guest's log files, in the storage worker (cast-zoa).
//!
//! Lines buffered by [`crate::logging`] (the worker's own and the page's) are
//! appended to OPFS files `logs/log-<start>-<seq>.jsonl`, one current file at a
//! time. A file is closed (rotated) when it passes [`ROTATE_BYTES`], when a take
//! stops and when the guest leaves; only closed files are offered to the host
//! through the outbox (`SourceKind::Log`), because a file must not change after
//! it was offered. `<start>` is this worker's start time in ms since the epoch
//! (13 digits), so names never collide across sessions; the host stores them as
//! `<device>-<name>.jsonl`. At most [`MAX_FILES`] files are kept (about
//! 20 MB), oldest closed ones that the outbox does not hold go first. A file
//! left behind by a crashed session is simply a closed file in the next one.

use std::cell::{Cell, RefCell};

use js_sys::Promise;
use wasm_bindgen::prelude::*;
use wasm_bindgen_futures::JsFuture;
use web_sys::FileSystemSyncAccessHandle;

use crate::js::call;
use crate::worker::{at, entry_names, named_dir, open_sync};

/// OPFS directory of the log files.
pub const LOGS_DIR: &str = "logs";
/// A file is rotated once it holds this many bytes.
const ROTATE_BYTES: u64 = 1024 * 1024;
/// Files kept; with [`ROTATE_BYTES`] this bounds the total near 20 MB.
const MAX_FILES: usize = 20;

struct Current {
    name: String,
    handle: FileSystemSyncAccessHandle,
    len: u64,
}

thread_local! {
    static CURRENT: RefCell<Option<Current>> = const { RefCell::new(None) };
    static SEQ: Cell<u32> = const { Cell::new(0) };
    static START_MS: Cell<u64> = const { Cell::new(0) };
    static BUSY: Cell<bool> = const { Cell::new(false) };
}

async fn tick() {
    let p = Promise::new(&mut |resolve, _| {
        let r: JsValue = resolve.into();
        let _ = call(&js_sys::global(), "setTimeout", &[&r, &JsValue::from(5)]);
    });
    let _ = JsFuture::from(p).await;
}

/// Hold the log files (one task at a time touches them).
pub(crate) async fn lock() {
    while BUSY.replace(true) {
        tick().await;
    }
}

pub(crate) fn unlock() {
    BUSY.set(false);
}

/// The file being written, if any (never offer it).
pub(crate) fn current_name() -> Option<String> {
    CURRENT.with(|c| c.borrow().as_ref().map(|c| c.name.clone()))
}

/// Write the buffered lines; with `rotate`, close the current file afterwards
/// (it then counts as closed and may be offered).
pub async fn flush(rotate: bool) -> Result<(), JsValue> {
    lock().await;
    let r = flush_locked(rotate).await;
    unlock();
    r
}

async fn flush_locked(rotate: bool) -> Result<(), JsValue> {
    if START_MS.get() == 0 {
        START_MS.set(js_sys::Date::now() as u64);
    }
    let text = crate::logging::take_text();
    if !text.is_empty() {
        if CURRENT.with(|c| c.borrow().is_none()) {
            let dir = named_dir(LOGS_DIR).await?;
            let name = format!("log-{:013}-{:03}.jsonl", START_MS.get(), SEQ.get());
            let handle = open_sync(&dir, &name, true).await?;
            let len = handle.get_size()? as u64;
            CURRENT.with(|c| *c.borrow_mut() = Some(Current { name, handle, len }));
        }
        let r = CURRENT.with(|c| {
            let mut c = c.borrow_mut();
            let cur = c.as_mut().expect("opened above");
            let n = cur
                .handle
                .write_with_u8_array_and_options(text.as_bytes(), &at(cur.len as f64))?;
            cur.len += n as u64;
            cur.handle.flush()
        });
        r?;
    }
    let full = CURRENT.with(|c| c.borrow().as_ref().is_some_and(|c| c.len >= ROTATE_BYTES));
    if rotate || full {
        close_current();
        prune().await?;
    }
    Ok(())
}

fn close_current() {
    if let Some(c) = CURRENT.with(|c| c.borrow_mut().take()) {
        c.handle.close();
        SEQ.set(SEQ.get() + 1);
    }
}

/// Remove the oldest closed files the outbox does not hold, beyond [`MAX_FILES`].
async fn prune() -> Result<(), JsValue> {
    let dir = named_dir(LOGS_DIR).await?;
    let mut names: Vec<String> = entry_names(&dir)
        .await?
        .into_iter()
        .filter(|n| n.ends_with(".jsonl"))
        .collect();
    let current = current_name();
    let mut excess = names.len().saturating_sub(MAX_FILES);
    names.retain(|n| Some(n) != current.as_ref() && crate::outbox::held(n).is_none());
    for n in names {
        if excess == 0 {
            break;
        }
        JsFuture::from(dir.remove_entry(&n)).await?;
        excess -= 1;
    }
    Ok(())
}
