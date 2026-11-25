//! The desktop app for hosts and app guests: a user-level process that runs
//! the host core in-process, shows the client in its window, closes to the
//! tray (and keeps hosting), optionally starts at login, and opens
//! `castcorder://` call links.
//!
//! This library is the part without a webview, so it is tested in
//! `task check`: [`host_control`] (start and stop the in-process host),
//! [`deeplink`] (map `castcorder://` to the https call link), [`remembered`]
//! (the last call link, reopened hidden at start so its page syncs) and the
//! app's flags ([`AppArgs`]). The Tauri shell is the `castcorder-app` binary
//! (feature `desktop`).

pub mod deeplink;
pub mod host_control;
pub mod remembered;

use std::path::{Path, PathBuf};

use castcorder_host::Config;

/// The app's own flags, beside the host flags ([`castcorder_host::USAGE`]).
pub const USAGE: &str = "[--hidden] [--smoke] [--no-register] [castcorder://LINK]";

/// The app's command line.
#[derive(Clone, Debug)]
pub struct AppArgs {
    /// The host's config.
    pub config: Config,
    /// `--hidden`: start in the tray without showing the window (start at login).
    pub hidden: bool,
    /// `--smoke`: start on a throwaway library with OS-picked loopback ports,
    /// wait for the window to load the producer page, print `APP_READY`, stop
    /// the host and exit. Registers nothing with the OS.
    pub smoke: bool,
    /// `--no-register`: do not register the `castcorder://` scheme for this
    /// executable (registered by default; per user, no admin rights).
    pub no_register: bool,
    /// A `castcorder://` link the app was started with.
    pub link: Option<String>,
}

/// Where the app finds the client build: `CASTCORDER_DIST`, else the first of
/// the places an installer puts the bundled `client` resource (beside the
/// executable on Windows, `../lib/Castcorder/` for Linux packages and
/// AppImage, `../Resources/` in a macOS bundle) that holds an `index.html`.
pub fn default_dist() -> PathBuf {
    if let Some(d) = std::env::var_os("CASTCORDER_DIST") {
        return PathBuf::from(d);
    }
    let Some(dir) = std::env::current_exe()
        .ok()
        .and_then(|e| e.parent().map(PathBuf::from))
    else {
        return PathBuf::from("client");
    };
    dist_candidates(&dir)
        .into_iter()
        .find(|c| c.join("index.html").exists())
        .unwrap_or_else(|| dir.join("client"))
}

/// Where the app finds its bundled transcription models (cast-7hm.4):
/// `CASTCORDER_MODELS`, else the first `models` resource beside the client
/// (the installers' layouts, as for [`default_dist`]) holding a `.bin`.
pub fn default_models() -> Option<PathBuf> {
    if let Some(d) = std::env::var_os("CASTCORDER_MODELS") {
        return Some(PathBuf::from(d));
    }
    let dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    models_candidates(&dir).into_iter().find(|c| has_model(c))
}

fn has_model(dir: &Path) -> bool {
    std::fs::read_dir(dir).is_ok_and(|mut d| {
        d.any(|e| e.is_ok_and(|e| e.path().extension().is_some_and(|x| x == "bin")))
    })
}

/// The model locations to try for an executable in `exe_dir`.
fn models_candidates(exe_dir: &Path) -> Vec<PathBuf> {
    dist_candidates(exe_dir)
        .into_iter()
        .map(|c| c.with_file_name("models"))
        .collect()
}

/// The client locations to try for an executable in `exe_dir`.
fn dist_candidates(exe_dir: &Path) -> Vec<PathBuf> {
    vec![
        exe_dir.join("client"),
        exe_dir.join("../lib/Castcorder/client"),
        exe_dir.join("../Resources/client"),
    ]
}

impl AppArgs {
    /// Parse the app's arguments (without the program name).
    pub fn parse(args: impl IntoIterator<Item = String>) -> Result<Self, String> {
        let mut config = Config::new(default_dist());
        config.log = true;
        // The live transcript is on wherever this build can run it and the
        // installer bundled its models; `--transcribe off` turns it off.
        if cfg!(feature = "transcribe") {
            config.transcribe = default_models().map(castcorder_host::transcript::Engine::Whisper);
        }
        config.rendezvous = castcorder_host::DEFAULT_RENDEZVOUS.map(String::from);
        let rest = config.apply_args(args)?;
        let mut a = AppArgs {
            config,
            hidden: false,
            smoke: false,
            no_register: false,
            link: None,
        };
        for arg in rest {
            match arg.as_str() {
                "--hidden" => a.hidden = true,
                "--smoke" => a.smoke = true,
                "--no-register" => a.no_register = true,
                _ if arg
                    .get(..deeplink::SCHEME.len() + 1)
                    .is_some_and(|s| s.eq_ignore_ascii_case("castcorder:")) =>
                {
                    a.link = Some(arg)
                }
                other => return Err(format!("unknown argument {other}")),
            }
        }
        if a.smoke {
            a.no_register = true;
            a.hidden = true;
            a.config.library =
                std::env::temp_dir().join(format!("castcorder-app-smoke-{}", std::process::id()));
            a.config.bind = std::net::Ipv4Addr::LOCALHOST.into();
            a.config.https_port = 0;
            a.config.http_port = 0;
            a.config.udp_port = 0;
            a.config.rendezvous = None;
            a.config.transcribe = None;
        }
        Ok(a)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(s: &str) -> Vec<String> {
        s.split_whitespace().map(String::from).collect()
    }

    #[test]
    fn host_flags_pass_through_and_app_flags_are_kept() {
        let a = AppArgs::parse(args("--call show --hidden castcorder://x/#y --port 9")).unwrap();
        assert_eq!(a.config.room, "show");
        assert_eq!(a.config.https_port, 9);
        assert!(a.hidden && !a.smoke && !a.no_register);
        assert_eq!(a.link.as_deref(), Some("castcorder://x/#y"));
        assert!(AppArgs::parse(args("--bogus")).is_err());
    }

    #[test]
    fn smoke_uses_loopback_os_ports_and_registers_nothing() {
        let a = AppArgs::parse(args("--smoke --port 27800")).unwrap();
        assert!(a.no_register && a.hidden);
        assert_eq!(
            (a.config.https_port, a.config.http_port, a.config.udp_port),
            (0, 0, 0)
        );
        assert!(a.config.bind.is_loopback());
    }

    #[test]
    fn dist_candidates_cover_each_installer_layout() {
        let c = dist_candidates(Path::new("bin"));
        assert_eq!(c[0], Path::new("bin/client"));
        assert!(c.iter().any(|p| p.ends_with("lib/Castcorder/client")));
        assert!(c.iter().any(|p| p.ends_with("Resources/client")));
    }

    #[test]
    fn models_sit_beside_the_client_in_each_installer_layout() {
        let c = models_candidates(Path::new("bin"));
        assert_eq!(c[0], Path::new("bin/models"));
        assert!(c.iter().any(|p| p.ends_with("lib/Castcorder/models")));
        assert!(c.iter().any(|p| p.ends_with("Resources/models")));
        let dir = std::env::temp_dir().join(format!("castcorder-models-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        assert!(!has_model(&dir));
        std::fs::write(dir.join("ggml-base.en-q8_0.bin"), b"x").unwrap();
        assert!(has_model(&dir));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
