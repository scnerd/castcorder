//! The app's update check (cast-qci): "Check for updates" in the tray menu.
//!
//! The updater plugin is registered only when the build has a signing key
//! and endpoint (`plugins.updater` in the config): release builds get them
//! from the release workflow's overlay config once the project's
//! `TAURI_UPDATER_PUBKEY` exists (docs/release.md); development builds and
//! forks without the key have no update item at all.
//!
//! One click checks. If a newer signed release exists the item becomes
//! "Install Castcorder X (restarts)"; the next click downloads it, verifies
//! its signature against the pubkey, installs it and restarts the app.

use std::sync::Mutex;

use tauri::menu::MenuItem;
use tauri::{AppHandle, Manager, Wry};
use tauri_plugin_updater::{Update, UpdaterExt};

/// The menu item's id.
pub const ID: &str = "update";

/// The tray item and the update it offers, if any.
pub struct Updates {
    item: MenuItem<Wry>,
    pending: Mutex<Option<Update>>,
}

/// Whether this build's config enables the updater.
pub fn configured(app: &AppHandle) -> bool {
    app.config().plugins.0.contains_key("updater")
}

/// Register the plugin (only when [`configured`]).
pub fn register(app: &AppHandle) {
    if !configured(app) {
        return;
    }
    if let Err(e) = app.plugin(tauri_plugin_updater::Builder::new().build()) {
        tracing::warn!(error = %e, "the updater is not available");
    }
}

/// The tray's update item, if the updater is configured; also checks once now.
pub fn item(app: &AppHandle, smoke: bool) -> Option<MenuItem<Wry>> {
    if !configured(app) {
        return None;
    }
    let item = MenuItem::with_id(app, ID, "Check for updates", !smoke, None::<&str>).ok()?;
    app.manage(Updates {
        item: item.clone(),
        pending: Mutex::new(None),
    });
    if !smoke {
        check(app, false);
    }
    Some(item)
}

/// The item was clicked: install the update found, else look for one.
pub fn clicked(app: &AppHandle) {
    let waiting = app
        .try_state::<Updates>()
        .is_some_and(|u| u.pending.lock().is_ok_and(|p| p.is_some()));
    if waiting {
        install(app);
    } else {
        check(app, true);
    }
}

fn set_text(app: &AppHandle, text: &str) {
    if let Some(u) = app.try_state::<Updates>() {
        let _ = u.item.set_text(text);
    }
}

/// Look for a newer release in the background; `say` also reports "up to date".
fn check(app: &AppHandle, say: bool) {
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let found = match app.updater() {
            Ok(u) => u.check().await,
            Err(e) => Err(e),
        };
        match found {
            Ok(Some(update)) => {
                tracing::info!(version = %update.version, "an update is available");
                set_text(
                    &app,
                    &format!(
                        "Install Castcorder {} (stops hosting, restarts)",
                        update.version
                    ),
                );
                if let Some(u) = app.try_state::<Updates>()
                    && let Ok(mut p) = u.pending.lock()
                {
                    *p = Some(update);
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
    });
}

/// Download, verify and install the pending update, then restart.
fn install(app: &AppHandle) {
    let Some(update) = app
        .try_state::<Updates>()
        .and_then(|u| u.pending.lock().ok().and_then(|mut p| p.take()))
    else {
        return;
    };
    set_text(app, "Installing the update...");
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        match update.download_and_install(|_, _| {}, || {}).await {
            Ok(()) => {
                tracing::info!(version = %update.version, "update installed; restarting");
                // Stop hosting first, as Quit does: open files get finished.
                // This runs on the async runtime, not the event loop.
                app.state::<crate::Shell>().host.stop();
                app.restart();
            }
            Err(e) => {
                tracing::warn!(error = %e, "update not installed");
                set_text(&app, "Update failed (check again)");
            }
        }
    });
}
