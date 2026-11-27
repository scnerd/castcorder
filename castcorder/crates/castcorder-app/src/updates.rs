//! The app's updates (cast-qci, cast-b3k).
//!
//! The updater plugin is registered only when the build has a signing key
//! and endpoint (`plugins.updater` in the config): release builds get them
//! from the release workflow's overlay config once the project's
//! `TAURI_UPDATER_PUBKEY` exists (docs/release.md); development builds and
//! forks without the key have no updates at all.
//!
//! The app looks for a newer signed release at start and every
//! [`EVERY`], and downloads one it finds in the background (its signature is
//! checked on download and again on install). Once downloaded, the tray item
//! reads "Restart to update to Castcorder X" and the home says the update is
//! ready ([`ready`], through the host's `/settings`); either installs it and
//! restarts, refused while a call is on. Quit installs a downloaded update
//! too, so nobody stays on an old version for long. With nothing downloaded,
//! the tray item checks now.

use std::sync::Mutex;
use std::time::Duration;

use tauri::menu::MenuItem;
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_updater::{Update, UpdaterExt};

/// The menu item's id.
pub const ID: &str = "update";

/// How often the app looks for a newer release.
pub const EVERY: Duration = Duration::from_secs(6 * 60 * 60);

/// An update downloaded and verified, waiting to be installed.
struct Downloaded {
    update: Update,
    bytes: Vec<u8>,
}

/// The tray item and the update state.
#[derive(Default)]
pub struct Updates {
    item: Mutex<Option<MenuItem<Wry>>>,
    /// A check or download is running.
    busy: Mutex<bool>,
    downloaded: Mutex<Option<Downloaded>>,
}

/// Whether this build's config enables the updater.
pub fn configured(app: &AppHandle) -> bool {
    app.config().plugins.0.contains_key("updater")
}

/// Register the plugin and the update state (only when [`configured`]).
pub fn register(app: &AppHandle) {
    if !configured(app) {
        return;
    }
    if let Err(e) = app.plugin(tauri_plugin_updater::Builder::new().build()) {
        tracing::warn!(error = %e, "the updater is not available");
        return;
    }
    app.manage(Updates::default());
}

/// The tray's update item, if the updater is configured; also starts the
/// checks (now and every [`EVERY`]).
pub fn item(app: &AppHandle, smoke: bool) -> Option<MenuItem<Wry>> {
    let updates = app.try_state::<Updates>()?;
    let item = MenuItem::with_id(app, ID, "Check for updates", !smoke, None::<&str>).ok()?;
    if let Ok(mut i) = updates.item.lock() {
        *i = Some(item.clone());
    }
    if !smoke {
        let app = app.clone();
        std::thread::spawn(move || {
            loop {
                check(&app, false);
                std::thread::sleep(EVERY);
            }
        });
    }
    Some(item)
}

/// The version downloaded and ready to install, if any.
pub fn ready(app: &AppHandle) -> Option<String> {
    let updates = app.try_state::<Updates>()?;
    let d = updates.downloaded.lock().ok()?;
    d.as_ref().map(|d| d.update.version.clone())
}

/// The item was clicked: install the update downloaded, else look for one.
pub fn clicked(app: &AppHandle) {
    if ready(app).is_some() {
        let host = &app.state::<crate::Shell>().host;
        match host.may_update() {
            Ok(()) => {
                let _ = restart_into_update(app);
            }
            Err(e) => set_text(app, &format!("Update waits: {e}")),
        }
    } else {
        check(app, true);
    }
}

fn set_text(app: &AppHandle, text: &str) {
    if let Some(u) = app.try_state::<Updates>()
        && let Ok(i) = u.item.lock()
        && let Some(i) = i.as_ref()
    {
        let _ = i.set_text(text);
    }
}

fn ready_text(version: &str) -> String {
    format!("Restart to update to Castcorder {version}")
}

/// Look for a newer release and download it, in the background; `say` also
/// reports "up to date" and failures on the tray item. Nothing happens while
/// a check runs or an update waits.
fn check(app: &AppHandle, say: bool) {
    let Some(updates) = app.try_state::<Updates>() else {
        return;
    };
    if ready(app).is_some() {
        return;
    }
    match updates.busy.lock() {
        Ok(mut b) if !*b => *b = true,
        _ => return,
    }
    if say {
        set_text(app, "Checking for updates...");
    }
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let found = match app.updater() {
            Ok(u) => u.check().await,
            Err(e) => Err(e),
        };
        match found {
            Ok(Some(update)) => {
                tracing::info!(version = %update.version, "an update is available; downloading it");
                set_text(
                    &app,
                    &format!("Downloading Castcorder {}...", update.version),
                );
                match update.download(|_, _| {}, || {}).await {
                    Ok(bytes) => {
                        tracing::info!(version = %update.version, "update downloaded");
                        set_text(&app, &ready_text(&update.version));
                        if let Some(u) = app.try_state::<Updates>()
                            && let Ok(mut d) = u.downloaded.lock()
                        {
                            *d = Some(Downloaded { update, bytes });
                        }
                    }
                    Err(e) => {
                        tracing::warn!(error = %e, "update not downloaded");
                        set_text(&app, "Update download failed (try again)");
                    }
                }
            }
            Ok(None) => {
                tracing::info!("Castcorder is up to date");
                if say {
                    set_text(&app, "Castcorder is up to date (check again)");
                }
            }
            Err(e) => {
                tracing::warn!(error = %e, "update check failed");
                if say {
                    set_text(&app, "Update check failed (try again)");
                }
            }
        }
        if let Some(u) = app.try_state::<Updates>()
            && let Ok(mut b) = u.busy.lock()
        {
            *b = false;
        }
    });
}

/// Take the downloaded update.
fn take(app: &AppHandle) -> Option<Downloaded> {
    app.try_state::<Updates>()?.downloaded.lock().ok()?.take()
}

/// Install the downloaded update and restart into it, on a thread of its
/// own (stopping the host waits for open files). Err when none is ready.
pub fn restart_into_update(app: &AppHandle) -> Result<(), String> {
    let d = take(app).ok_or("no update is ready")?;
    set_text(app, "Installing the update...");
    let app = app.clone();
    std::thread::spawn(move || {
        // Stop hosting first, as Quit does: open files get finished.
        app.state::<crate::Shell>().host.stop();
        match d.update.install(&d.bytes) {
            Ok(()) => {
                tracing::info!(version = %d.update.version, "update installed; restarting");
                app.restart();
            }
            Err(e) => {
                tracing::error!(error = %e, "update not installed; starting again on this version");
                app.restart();
            }
        }
    });
    Ok(())
}

/// On Quit, after the host stopped: install a downloaded update, so the next
/// start is on it.
pub fn install_on_quit(app: &AppHandle) {
    if let Some(d) = take(app) {
        match d.update.install(&d.bytes) {
            Ok(()) => tracing::info!(version = %d.update.version, "update installed on quit"),
            Err(e) => tracing::warn!(error = %e, "update not installed on quit"),
        }
    }
}
