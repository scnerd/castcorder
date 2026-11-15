//! `castcorder-app`: the desktop shell (Tauri 2). It starts the host in this
//! process ([`castcorder_app::host_control`]) and shows its producer page in
//! the window, where the person at the host joins the call explicitly.
//! Closing the window hides it to the tray; the host keeps serving the call
//! until Quit in the tray menu, which finishes the open files and stops it.
//! "Start at login" in the tray menu is off until turned on there.
//! `castcorder://` links (at startup, or handed over by a second launch) open
//! the https call link in a call window.
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

use castcorder_app::AppArgs;
use castcorder_app::deeplink;
use castcorder_app::host_control::{HostControl, tray_text};
use tauri::menu::{CheckMenuItem, Menu, MenuItem, PredefinedMenuItem};
use tauri::tray::{MouseButton, MouseButtonState, TrayIconBuilder, TrayIconEvent};
use tauri::webview::PageLoadEvent;
use tauri::{AppHandle, Manager, RunEvent, WebviewUrl, WebviewWindowBuilder, WindowEvent};
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
        .setup(move |app| {
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
            tracing::info!(call = %status.call, producer = %status.producer_url, "app host running");
            println!(
                "HOST_READY http={}",
                status.producer_url.trim_end_matches('/')
            );
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
    let window = WebviewWindowBuilder::new(app, MAIN, url)
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
    }
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
    let copy = MenuItem::with_id(app, "copy_link", "Copy call link", true, None::<&str>)?;
    let regen = MenuItem::with_id(
        app,
        "regen_link",
        "Regenerate call link",
        true,
        None::<&str>,
    )?;
    let menu = Menu::with_items(
        app,
        &[
            &status_item,
            &PredefinedMenuItem::separator(app)?,
            &open,
            &copy,
            &regen,
            &login,
            &PredefinedMenuItem::separator(app)?,
            &quit_item,
        ],
    )?;
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
            "copy_link" => copy_link(app),
            "regen_link" => {
                let app = app.clone();
                // The host answers on its own thread; not on the event loop.
                std::thread::spawn(move || match app.state::<Shell>().host.regenerate_link() {
                    Ok(Some(link)) => write_clipboard(&app, &link),
                    Ok(None) => tracing::warn!("no call link: the host has no rendezvous"),
                    Err(e) => tracing::warn!(error = %e, "call link not regenerated"),
                });
            }
            "quit" => quit(app),
            _ => {}
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
    let recording_icon = app.default_window_icon().map(recording_variant);
    let idle_icon = app
        .default_window_icon()
        .map(|i| tauri::image::Image::new_owned(i.rgba().to_vec(), i.width(), i.height()));
    let app = app.clone();
    // Poll the host once a second; touch the OS only when something changed.
    std::thread::spawn(move || {
        let mut last = None;
        loop {
            std::thread::sleep(Duration::from_secs(1));
            let Some(live) = app.state::<Shell>().host.live_status() else {
                continue;
            };
            if last == Some(live) {
                continue;
            }
            last = Some(live);
            let text = tray_text(&live);
            let _ = status_item.set_text(&text);
            let _ = tray.set_tooltip(Some(format!("Castcorder: {text}")));
            let _ = tray.set_title(live.recording.map(|t| format!("REC {t}")));
            let icon = if live.recording.is_some() {
                recording_icon.clone()
            } else {
                idle_icon.clone()
            };
            if icon.is_some() {
                let _ = tray.set_icon(icon);
            }
        }
    });
    Ok(())
}

/// `icon` with a red dot in its lower right corner: the tray while recording.
fn recording_variant(icon: &tauri::image::Image<'_>) -> tauri::image::Image<'static> {
    let (w, h) = (icon.width() as i64, icon.height() as i64);
    let mut rgba = icon.rgba().to_vec();
    let r = (w.min(h) / 3).max(2);
    let (cx, cy) = (w - r - 1, h - r - 1);
    for y in 0..h {
        for x in 0..w {
            if (x - cx).pow(2) + (y - cy).pow(2) <= r * r {
                let i = ((y * w + x) * 4) as usize;
                rgba[i..i + 4].copy_from_slice(&[220, 30, 30, 255]);
            }
        }
    }
    tauri::image::Image::new_owned(rgba, w as u32, h as u32)
}

/// Put the running host's call link on the clipboard.
fn copy_link(app: &AppHandle) {
    match app.state::<Shell>().host.status().and_then(|s| s.call_link) {
        Some(link) => write_clipboard(app, &link),
        None => tracing::warn!("no call link yet: the host is not registered with a rendezvous"),
    }
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
    let url = match deeplink::call_link(link) {
        Ok(url) => url,
        Err(e) => {
            tracing::warn!(error = %e, "not opening a castcorder:// link");
            return;
        }
    };
    let Ok(parsed) = url.parse() else { return };
    tracing::info!("opening a call link");
    if let Some(w) = app.get_webview_window(CALL) {
        let _ = w.navigate(parsed);
        show(app, CALL);
        return;
    }
    if let Err(e) = WebviewWindowBuilder::new(app, CALL, WebviewUrl::External(parsed))
        .title("Castcorder call")
        .inner_size(1100.0, 760.0)
        .build()
    {
        tracing::warn!(error = %e, "no call window");
    }
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
