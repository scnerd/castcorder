//! `castcorder-app`: the desktop shell (Tauri 2). It starts the host in this
//! process ([`castcorder_app::host_control`]) and shows its producer page in
//! the window, where the person at the host joins the call explicitly.
//! Closing the window hides it to the tray; the host keeps serving the call
//! until Quit in the tray menu, which finishes the open files and stops it.
//! "Start at login" in the tray menu is off until turned on there. The tray
//! lists every room with its own "Copy call link"; replacing a link is in the
//! room's menu on the home, which asks first (cast-7jv).
//! `castcorder://` links (at startup, or handed over by a second launch) open
//! the https call link in a call window. Closing that window leaves the call
//! and hides it, so its page goes on delivering the recordings over a
//! sync-only connection; the last link opened is remembered and reopened
//! hidden at the next start (at login too) to finish that sync.
//!
//! Usage: `castcorder-app [host flags] [--hidden] [--smoke] [--no-register]
//! [castcorder://LINK]`; the host flags are `castcorder-host`'s. `--smoke`
//! starts on a throwaway library and OS-picked loopback ports, waits for the
//! hidden window to load the producer page, prints `APP_READY`, then stops
//! the host and exits (exit code 1 if that takes over a minute).
//! `castcorder-app --smoke-gst` only checks GStreamer (see `smoke_gst`); an
//! installer's bundled GStreamer is found beside the executable.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::sync::Arc;
use std::time::Duration;

mod updates;

use castcorder_app::AppArgs;
use castcorder_app::host_control::{
    Badge, HostControl, TrayRoom, bridge_allows, tray_badge, tray_room_label, tray_room_of,
    tray_text,
};
use castcorder_app::{deeplink, remembered};
use tauri::ipc::CapabilityBuilder;
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem, Submenu};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::PageLoadEvent;
use tauri::{
    AppHandle, Manager, RunEvent, State, Webview, WebviewUrl, WebviewWindowBuilder, WindowEvent,
};
use tauri_plugin_autostart::{MacosLauncher, ManagerExt};
use tauri_plugin_clipboard_manager::ClipboardExt;
use tauri_plugin_deep_link::DeepLinkExt;

/// The window with the host's producer page.
const MAIN: &str = "main";
/// The window with a call joined through a call link.
const CALL: &str = "call";

/// What the shell keeps in Tauri's state.
struct Shell {
    host: Arc<HostControl>,
    smoke: bool,
}

fn main() {
    if std::env::args().nth(1).as_deref() == Some("--smoke-gst") {
        std::process::exit(smoke_gst());
    }
    let mut args = AppArgs::parse(std::env::args().skip(1)).unwrap_or_else(|e| {
        eprintln!("castcorder-app: {e}");
        eprintln!(
            "usage: castcorder-app {} {}",
            castcorder_host::USAGE,
            castcorder_app::USAGE
        );
        std::process::exit(2);
    });
    if args.smoke {
        smoke_dist(&mut args);
        // The smoke run never hangs a CI job.
        std::thread::spawn(|| {
            std::thread::sleep(Duration::from_secs(60));
            eprintln!("castcorder-app: smoke run timed out");
            std::process::exit(1);
        });
    }
    let host = Arc::new(HostControl::new(args.config.clone()));
    // A smoke run's throwaway library goes when the app exits.
    let scratch = args.smoke.then(|| args.config.library.clone());
    let mut builder = tauri::Builder::default();
    if !args.smoke {
        // First, so a second launch (a clicked castcorder:// link) is handed to
        // this process instead of starting a second host on the same ports.
        builder = builder.plugin(tauri_plugin_single_instance::init(|app, _argv, _cwd| {
            show(app, MAIN);
        }));
    }
    let app = builder
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        .plugin(tauri_plugin_autostart::init(
            MacosLauncher::LaunchAgent,
            Some(vec!["--hidden"]),
        ))
        .manage(Shell {
            host: host.clone(),
            smoke: args.smoke,
        })
        .invoke_handler(tauri::generate_handler![
            contact_code,
            contact_add,
            joined_rooms_save,
            show_home
        ])
        .setup(move |app| {
            if !args.smoke {
                updates::register(app.handle());
            }
            setup(app.handle(), &args);
            Ok(())
        })
        .on_window_event(|window, event| {
            // Closing hides to the tray; the host keeps serving the call.
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == MAIN
            {
                api.prevent_close();
                let _ = window.hide();
            }
            // Closing a call window leaves the call (as its Leave button
            // does) and hides it: its page goes on delivering the recordings
            // over a sync-only connection from the tray.
            if let WindowEvent::CloseRequested { api, .. } = event
                && window.label() == CALL
                && let Some(w) = window.app_handle().get_webview_window(CALL)
            {
                api.prevent_close();
                let _ = w.eval("document.getElementById('leave')?.click()");
                let _ = w.hide();
            }
        })
        .on_page_load(|webview, payload| {
            if webview.label() == MAIN
                && payload.event() == PageLoadEvent::Finished
                && webview.state::<Shell>().smoke
            {
                // Ready only once the window shows the running host's page.
                let host = webview.state::<Shell>().host.status();
                let url = payload.url();
                let ours = host.is_some_and(|h| {
                    url.scheme() == "http" && url.port() == Some(h.http_addr.port())
                });
                if !ours {
                    eprintln!("castcorder-app: smoke run loaded {url}, not the host's page");
                    std::process::exit(1);
                }
                // The tray's status snapshot answers: nobody is in the call.
                match webview.state::<Shell>().host.live_status() {
                    Some(live) if live.guests == 0 && live.recording.is_none() => {
                        println!("APP_STATUS {}", tray_text(&live));
                    }
                    other => {
                        eprintln!("castcorder-app: smoke run got status {other:?}");
                        std::process::exit(1);
                    }
                }
                println!("APP_READY url={url}");
                quit(webview.app_handle());
            }
        })
        .build(tauri::generate_context!())
        .unwrap_or_else(|e| {
            eprintln!("castcorder-app: {e}");
            std::process::exit(1);
        });
    app.run(move |_app, event| {
        if let RunEvent::Exit = event {
            host.stop();
            if let Some(dir) = &scratch {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    });
}

/// `--smoke-gst`: GStreamer initialises and every element native capture
/// needs is found. Prints `GST_ELEMENT <name> <plugin file>` or
/// `GST_MISSING <name>` per element, then `GST_OK`; exit code 1 if a required
/// element is missing or the build has no native capture.
fn smoke_gst() -> i32 {
    let elements = match castcorder_host::native_capture_check() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("castcorder-app: GStreamer: {e}");
            return 1;
        }
    };
    let mut ok = true;
    for e in &elements {
        match &e.plugin {
            Some(p) => println!("GST_ELEMENT {} {}", e.name, p.display()),
            None => {
                let optional = if e.required { "" } else { " (optional)" };
                println!("GST_MISSING {}{optional}", e.name);
                ok &= !e.required;
            }
        }
    }
    if ok {
        println!("GST_OK");
        0
    } else {
        1
    }
}

/// A smoke run without a client build serves a stand-in page from its
/// throwaway library.
fn smoke_dist(args: &mut AppArgs) {
    if args.config.dist.join("index.html").exists() {
        return;
    }
    let dist = args.config.library.join("dist");
    if std::fs::create_dir_all(&dist)
        .and_then(|()| {
            std::fs::write(
                dist.join("index.html"),
                "<!doctype html><title>Castcorder smoke</title>",
            )
        })
        .is_ok()
    {
        args.config.dist = dist;
    }
}

fn setup(app: &AppHandle, args: &AppArgs) {
    let shell = app.state::<Shell>();
    let url = match shell.host.start() {
        Ok(status) => {
            tracing::info!(room = %status.room, producer = %status.producer_url, "app host running");
            println!(
                "HOST_READY http={}",
                status.producer_url.trim_end_matches('/')
            );
            bridge(app, &shell.host);
            WebviewUrl::External(status.producer_url.parse().expect("producer url"))
        }
        Err(e) => {
            eprintln!("castcorder-app: the host could not start: {e}");
            if args.smoke {
                std::process::exit(1);
            }
            WebviewUrl::App(format!("index.html?error={}", query_escape(&e)).into())
        }
    };
    let nav = app.clone();
    let window = WebviewWindowBuilder::new(app, MAIN, url)
        // The home's castcorder:// links (Rooms you joined) open here, not in the OS.
        .on_navigation(move |u| {
            if u.scheme().eq_ignore_ascii_case(deeplink::SCHEME) {
                open_link(&nav, u.as_str());
                return false;
            }
            true
        })
        .title("Castcorder")
        .inner_size(1100.0, 760.0)
        .visible(!args.hidden)
        .build();
    if let Err(e) = window {
        eprintln!("castcorder-app: no window: {e}");
    }
    if let Err(e) = tray(app, args.smoke) {
        tracing::warn!(error = %e, "no tray icon: close the window to quit");
    }
    if !args.no_register {
        // Installers will register the scheme; until then the app registers
        // itself (per user) for castcorder:// links.
        #[cfg(any(windows, target_os = "linux"))]
        if let Err(e) = app.deep_link().register_all() {
            tracing::warn!(error = %e, "castcorder:// links not registered");
        }
    }
    let handle = app.clone();
    app.deep_link().on_open_url(move |event| {
        for url in event.urls() {
            open_link(&handle, url.as_str());
        }
    });
    if let Some(link) = &args.link {
        open_link(app, link);
    } else if !args.smoke
        && let Some(url) = data_dir(app).and_then(|d| remembered::recall(&d))
    {
        // The last call's page, hidden: it delivers what its outbox still
        // holds (at login too), without joining.
        tracing::info!("reopening the last call link hidden, to finish syncing");
        call_window(app, &url, false);
    }
}

/// The app's data folder (the last call link is kept there).
fn data_dir(app: &AppHandle) -> Option<std::path::PathBuf> {
    app.path().app_data_dir().ok()
}

fn tray(app: &AppHandle, smoke: bool) -> tauri::Result<()> {
    let open = MenuItem::with_id(app, "open", "Open Castcorder", true, None::<&str>)?;
    // Never read or written in a smoke run: it is per-user OS state.
    let at_login = !smoke && app.autolaunch().is_enabled().unwrap_or(false);
    let login = CheckMenuItem::with_id(
        app,
        "login",
        "Start at login",
        !smoke,
        at_login,
        None::<&str>,
    )?;
    let quit_item = MenuItem::with_id(app, "quit", "Quit (stop hosting)", true, None::<&str>)?;
    let status_item = MenuItem::with_id(app, "status", "Starting", false, None::<&str>)?;
    // One "Copy call link" per room (cast-7jv); filled by the poll below.
    let rooms_menu = Submenu::with_id(app, "rooms", "Rooms", true)?;
    let folder = MenuItem::with_id(
        app,
        "open_folder",
        "Open recordings folder",
        true,
        None::<&str>,
    )?;
    let update_item = updates::item(app, smoke);
    let menu = Menu::with_items(
        app,
        &[
            &status_item,
            &PredefinedMenuItem::separator(app)?,
            &open,
            &folder,
            &rooms_menu,
            &login,
        ],
    )?;
    if let Some(u) = &update_item {
        menu.append(u)?;
    }
    menu.append(&PredefinedMenuItem::separator(app)?)?;
    menu.append(&quit_item)?;
    let mut tray = TrayIconBuilder::with_id("castcorder")
        .tooltip("Castcorder: hosting")
        .menu(&menu)
        .show_menu_on_left_click(false)
        .on_menu_event(move |app, event| match event.id.as_ref() {
            "open" => show(app, MAIN),
            "login" => {
                let autostart = app.autolaunch();
                let on = autostart.is_enabled().unwrap_or(false);
                let r = if on {
                    autostart.disable()
                } else {
                    autostart.enable()
                };
                if let Err(e) = r {
                    tracing::warn!(error = %e, "start at login not changed");
                }
                let _ = login.set_checked(autostart.is_enabled().unwrap_or(false));
            }
            "open_folder" => {
                if let Err(e) = app.state::<Shell>().host.open_recordings_folder() {
                    tracing::warn!(error = %e, "recordings folder not opened");
                }
            }
            updates::ID => updates::clicked(app),
            "quit" => quit(app),
            id => {
                if let Some(room) = tray_room_of(id) {
                    copy_room_link(app, room);
                }
            }
        })
        .on_tray_icon_event(|tray, event| {
            if let TrayIconEvent::Click {
                button: MouseButton::Left,
                button_state: MouseButtonState::Up,
                ..
            } = event
            {
                show(tray.app_handle(), MAIN);
            }
        });
    if let Some(icon) = app.default_window_icon() {
        tray = tray.icon(icon.clone());
    }
    let tray = tray.build(app)?;
    // One icon per badge, painted once.
    let badge_icon = |badge: Badge| {
        app.default_window_icon()
            .map(|i| badge_variant(i, badge.rgb()))
    };
    let recording_icon = badge_icon(Badge::Recording);
    let attention_icon = badge_icon(Badge::NeedsAttention);
    let syncing_icon = badge_icon(Badge::Syncing);
    let idle_icon = app
        .default_window_icon()
        .map(|i| tauri::image::Image::new_owned(i.rgba().to_vec(), i.width(), i.height()));
    let app = app.clone();
    // Poll the host once a second; touch the OS only when something changed.
    std::thread::spawn(move || {
        let mut last = None;
        let mut invites = (0, None);
        let mut listed: Vec<TrayRoom> = Vec::new();
        let mut tick = 0u32;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            // The rooms menu follows the host's rooms (every few seconds: the
            // host answers on its own thread).
            tick = tick.wrapping_add(1);
            if tick % 4 == 1 {
                let rooms = app.state::<Shell>().host.tray_rooms();
                if rooms != listed {
                    if let Err(e) = fill_rooms_menu(&app, &rooms_menu, &rooms) {
                        tracing::warn!(error = %e, "the tray's rooms menu was not updated");
                    }
                    listed = rooms;
                }
            }
            // An invite from a contact brings the home (which shows it with
            // Join and Dismiss) to the front, and the tray says so.
            let now = app.state::<Shell>().host.invites().unwrap_or_default();
            let changed = now != invites;
            if now.0 > invites.0 {
                show(&app, MAIN);
            }
            invites = now;
            let Some(live) = app.state::<Shell>().host.live_status() else {
                continue;
            };
            if last == Some(live) && !changed {
                continue;
            }
            last = Some(live);
            let text = match &invites.1 {
                Some(invite) => format!("{}; {invite}", tray_text(&live)),
                None => tray_text(&live),
            };
            let _ = status_item.set_text(&text);
            let _ = tray.set_tooltip(Some(format!("Castcorder: {text}")));
            let _ = tray.set_title(live.recording.map(|t| format!("REC {t}")));
            let icon = match tray_badge(&live) {
                Some(Badge::Recording) => recording_icon.clone(),
                Some(Badge::NeedsAttention) => attention_icon.clone(),
                Some(Badge::Syncing) => syncing_icon.clone(),
                None => idle_icon.clone(),
            };
            if icon.is_some() {
                let _ = tray.set_icon(icon);
            }
        }
    });
    Ok(())
}

/// `icon` with a dot of colour `rgb` in its lower right corner: the tray's
/// state badge (red recording, amber needs a look, blue syncing).
fn badge_variant(icon: &tauri::image::Image<'_>, rgb: [u8; 3]) -> tauri::image::Image<'static> {
    let (w, h) = (icon.width() as i64, icon.height() as i64);
    let mut rgba = icon.rgba().to_vec();
    let r = (w.min(h) / 3).max(2);
    let (cx, cy) = (w - r - 1, h - r - 1);
    for y in 0..h {
        for x in 0..w {
            if (x - cx).pow(2) + (y - cy).pow(2) <= r * r {
                let i = ((y * w + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[rgb[0], rgb[1], rgb[2], 255]);
            }
        }
    }
    tauri::image::Image::new_owned(rgba, w as u32, h as u32)
}

/// Put the running host's call link on the clipboard.
/// Copy `room`'s call link (the tray's Rooms menu).
fn copy_room_link(app: &AppHandle, room: &str) {
    match app.state::<Shell>().host.room_link(room) {
        Some(link) => write_clipboard(app, &link),
        None => tracing::warn!(
            room,
            "no call link yet: the host is not registered with a rendezvous"
        ),
    }
}

/// Replace the Rooms submenu's items with one per room.
fn fill_rooms_menu(
    app: &AppHandle,
    menu: &Submenu<tauri::Wry>,
    rooms: &[TrayRoom],
) -> tauri::Result<()> {
    for item in menu.items()? {
        menu.remove(&item)?;
    }
    if rooms.is_empty() {
        menu.append(&MenuItem::with_id(
            app,
            "no_rooms",
            "No rooms yet",
            false,
            None::<&str>,
        )?)?;
    }
    for r in rooms {
        let id = format!("room_copy:{}", r.name);
        menu.append(&MenuItem::with_id(
            app,
            id,
            tray_room_label(r),
            r.linked,
            None::<&str>,
        )?)?;
    }
    Ok(())
}

fn write_clipboard(app: &AppHandle, text: &str) {
    if let Err(e) = app.clipboard().write_text(text) {
        tracing::warn!(error = %e, "call link not copied");
    }
}

fn show(app: &AppHandle, label: &str) {
    if let Some(w) = app.get_webview_window(label) {
        let _ = w.unminimize();
        let _ = w.show();
        let _ = w.set_focus();
    }
}

/// Finish the host's open files and stop it, then exit.
fn quit(app: &AppHandle) {
    let app = app.clone();
    // Stopping waits for the take to finish; not on the event loop.
    std::thread::spawn(move || {
        app.state::<Shell>().host.stop();
        app.exit(0);
    });
}

/// Open a `castcorder://` link's call in the call window.
fn open_link(app: &AppHandle, link: &str) {
    if deeplink::is_rooms_link(link) {
        // "Rooms you joined": the call window's lobby lists them (its browser
        // profile holds the list and the device key).
        if app.get_webview_window(CALL).is_some() {
            show(app, CALL);
        } else if let Some(url) = data_dir(app).and_then(|d| remembered::recall(&d)) {
            call_window(app, &url, true);
        } else {
            tracing::info!("no rooms to list yet: this app has not joined a call");
        }
        return;
    }
    let url = match deeplink::call_link(link) {
        Ok(url) => url,
        Err(e) => {
            tracing::warn!(error = %e, "not opening a castcorder:// link");
            return;
        }
    };
    tracing::info!("opening a call link");
    if let Some(dir) = data_dir(app)
        && let Err(e) = remembered::remember(&dir, link)
    {
        tracing::warn!(error = %e, "call link not remembered; it will not sync after a restart");
    }
    call_window(app, &url, true);
}

/// Show `url` in the call window (made if needed), or load it hidden.
fn call_window(app: &AppHandle, url: &str, visible: bool) {
    let Ok(parsed) = url.parse() else { return };
    if let Some(w) = app.get_webview_window(CALL) {
        let _ = w.navigate(parsed);
        if visible {
            show(app, CALL);
        }
        return;
    }
    // Links the window to this install's person on the project's pages
    // (cast-40e.7), so the app joins its person's rooms as a producer.
    let script = app.state::<Shell>().host.call_window_script();
    if let Err(e) = WebviewWindowBuilder::new(app, CALL, WebviewUrl::External(parsed))
        .initialization_script(&script)
        .title("Castcorder call")
        .inner_size(1100.0, 760.0)
        .visible(visible)
        .build()
    {
        tracing::warn!(error = %e, "no call window");
    }
}

/// Let the call window reach this install's contacts from the project's own
/// pages (cast-40e.10): "Add as contact" and accepting an offer in a call.
/// Remote pages reach no command unless a capability names their origin.
fn bridge(app: &AppHandle, host: &HostControl) {
    let Some(origin) = host.project_origin() else {
        return;
    };
    let capability = CapabilityBuilder::new("call-window-contacts")
        .local(false)
        .window(CALL)
        .remote(format!("{origin}/*"))
        .permission("allow-contact-code")
        .permission("allow-contact-add")
        .permission("allow-joined-rooms-save")
        .permission("allow-show-home");
    match app.add_capability(capability) {
        Ok(()) => tracing::info!(%origin, "the call window may reach the contacts"),
        Err(e) => tracing::warn!(error = %e, "the call window cannot reach the contacts"),
    }
}

/// The host, if `webview` may use the bridge ([`bridge_allows`]).
fn bridged<'a>(webview: &Webview, shell: &'a State<'_, Shell>) -> Result<&'a HostControl, String> {
    let url = webview.url().map_err(|e| e.to_string())?;
    let origin = shell.host.project_origin();
    if bridge_allows(webview.label(), url.as_str(), origin.as_deref()) {
        Ok(&shell.host)
    } else {
        Err("only the call window on the project's pages may do that".into())
    }
}

/// The call window's "Add as contact": a code for `person` alone.
#[tauri::command]
fn contact_code(
    webview: Webview,
    shell: State<'_, Shell>,
    person: String,
) -> Result<String, String> {
    bridged(&webview, &shell)?.contact_code_for(&person)
}

/// The call window's Accept on an offer: use its contact code.
#[tauri::command]
fn contact_add(webview: Webview, shell: State<'_, Shell>, code: String) -> Result<(), String> {
    bridged(&webview, &shell)?.add_contact(&code)
}

/// The call window's list of rooms it joined on other hosts, for the home to
/// show (cast-n56.9).
#[tauri::command]
fn joined_rooms_save(
    webview: Webview,
    shell: State<'_, Shell>,
    rooms: String,
) -> Result<(), String> {
    bridged(&webview, &shell)?.save_joined_rooms(&rooms)
}

/// The call window's "Back to your rooms" once its person left the call
/// (cast-n56.12 for a call joined from a link): hide the call window, which
/// goes on delivering the recordings as closing it does, and bring the home
/// to the front.
#[tauri::command]
fn show_home(webview: Webview, shell: State<'_, Shell>) -> Result<(), String> {
    bridged(&webview, &shell)?;
    let app = webview.app_handle();
    if let Some(w) = app.get_webview_window(CALL) {
        let _ = w.hide();
    }
    show(app, MAIN);
    Ok(())
}

/// Percent-encode everything but unreserved characters.
fn query_escape(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}
