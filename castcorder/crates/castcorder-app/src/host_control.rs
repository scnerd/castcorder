//! The host the app runs in its own process: started with the app, kept
//! running while the window is hidden in the tray, stopped (its open files
//! finished) when the app quits. The person at the host joins and leaves the
//! call with a microphone captured natively at runtime ([`HostControl::join_native`];
//! the producer page does the same through the host's `/native` endpoints).

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Mutex;

use castcorder_host::{Config, LiveStatus, NativeInput, NativeState, Running};

/// What the window and the tray show about the running host.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct HostStatus {
    /// The default room's name.
    pub room: String,
    /// The producer page the window shows (`http://localhost:<port>/`).
    pub producer_url: String,
    /// The producer page's socket address.
    pub http_addr: SocketAddr,
    /// The LAN join links.
    pub lan_links: Vec<String>,
    /// The call's UDP address.
    pub udp_addr: SocketAddr,
    /// The call link for guests anywhere, once registered with a rendezvous.
    pub call_link: Option<String>,
}

/// Starts and stops the in-process host for one [`Config`].
pub struct HostControl {
    config: Config,
    running: Mutex<Option<Running>>,
}

impl HostControl {
    /// A control for `config`'s host; nothing runs until [`HostControl::start`].
    pub fn new(config: Config) -> Self {
        HostControl {
            config,
            running: Mutex::new(None),
        }
    }

    /// The config the host starts with.
    pub fn config(&self) -> &Config {
        &self.config
    }

    /// Start the host, or report the one already running.
    pub fn start(&self) -> Result<HostStatus, String> {
        let mut running = self.running.lock().map_err(|_| "host lock poisoned")?;
        if running.is_none() {
            let room = self.config.room.clone();
            let r = castcorder_host::start(self.config.clone(), move |link| {
                tracing::info!(room = %room, link = %link, "call link ready");
            })?;
            *running = Some(r);
        }
        Ok(status(running.as_ref().expect("just started")))
    }

    /// The running host, if any.
    pub fn status(&self) -> Option<HostStatus> {
        self.running.lock().ok()?.as_ref().map(status)
    }

    /// Join the call as the person at the host with `input`, captured
    /// natively (a build with `native-capture`); a take recording now is
    /// recorded from here on.
    pub fn join_native(&self, input: NativeInput) -> Result<(), String> {
        let running = self.running.lock().map_err(|_| "host lock poisoned")?;
        running
            .as_ref()
            .ok_or("the host is not running")?
            .join_native(input)
    }

    /// Leave the call with the native input: the take recording now gets its
    /// microphone files finished, and the input is closed when this returns.
    /// False if it was not in the call.
    pub fn leave_native(&self) -> Result<bool, String> {
        let running = self.running.lock().map_err(|_| "host lock poisoned")?;
        match running.as_ref() {
            Some(r) => r.leave_native(),
            None => Ok(false),
        }
    }

    /// Mute (true) or unmute the native microphone in the call (call-only:
    /// its track keeps recording). False if it is not in the call.
    pub fn mute_native(&self, muted: bool) -> bool {
        self.running
            .lock()
            .ok()
            .and_then(|r| r.as_ref().map(|r| r.mute_native(muted)))
            .unwrap_or(false)
    }

    /// The inputs the host can join with and whether it is joined, if running.
    pub fn native_state(&self) -> Option<NativeState> {
        self.running
            .lock()
            .ok()?
            .as_ref()
            .map(Running::native_state)
    }

    /// Where the open call's tracks are saved: `<library>/<room>/<call>`,
    /// absolute (else the default room's folder, else the library).
    pub fn recordings_folder(&self) -> PathBuf {
        let dir = self
            .running
            .lock()
            .ok()
            .and_then(|r| r.as_ref().map(|r| r.call_dir()))
            .unwrap_or_else(|| self.config.library.join(&self.config.room));
        std::path::absolute(&dir).unwrap_or(dir)
    }

    /// Open the call's folder in the file manager.
    pub fn open_recordings_folder(&self) -> Result<(), String> {
        castcorder_host::open_folder(&self.recordings_folder())
    }

    /// The call as it is now (guests, recording take, syncing outboxes), if
    /// the host is running.
    pub fn live_status(&self) -> Option<LiveStatus> {
        self.running.lock().ok()?.as_ref()?.live_status()
    }

    /// The newest invite to a call from a contact ("Ann invites you to
    /// show"), and how many are shown, if the host is running.
    pub fn invites(&self) -> Option<(usize, Option<String>)> {
        let running = self.running.lock().ok()?;
        let v = running.as_ref()?.contacts();
        let newest = v
            .invites
            .iter()
            .max_by_key(|i| i.at_ms)
            .map(|i| format!("{} invites you to {}", i.from, i.room));
        Some((v.invites.len(), newest))
    }

    /// The script the call window runs on every page (cast-40e.7): on the
    /// project's own pages (the rendezvous origin) it hands the page a link
    /// code approved in advance, so the window links itself to this
    /// install's person. Empty when the host is not running or not yet
    /// registered with a rendezvous.
    pub fn call_window_script(&self) -> String {
        let link = self
            .running
            .lock()
            .ok()
            .and_then(|r| r.as_ref()?.call_window_link());
        link.map(|(origin, fragment)| autolink_script(&origin, &fragment))
            .unwrap_or_default()
    }

    /// Rotate the call secret (the old call link stops admitting anyone) and
    /// return the new call link, if the host is reachable through a rendezvous.
    pub fn regenerate_link(&self) -> Result<Option<String>, String> {
        let running = self.running.lock().map_err(|_| "host lock poisoned")?;
        running
            .as_ref()
            .ok_or("the host is not running")?
            .regenerate_link()
    }

    /// Finish the host's open files and stop it; nothing happens if it is not
    /// running. Its ports are free when this returns.
    pub fn stop(&self) {
        let running = self.running.lock().ok().and_then(|mut r| r.take());
        if let Some(r) = running {
            r.stop();
        }
    }
}

impl Drop for HostControl {
    fn drop(&mut self) {
        self.stop();
    }
}

fn status(r: &Running) -> HostStatus {
    HostStatus {
        room: r.room().to_string(),
        producer_url: r.producer_url(),
        http_addr: r.http_addr(),
        lan_links: r.lan_links().to_vec(),
        udp_addr: r.udp_addr(),
        call_link: r.call_link(),
    }
}

/// The corner badge on the tray icon.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Badge {
    /// A take is recording: red (the design's only red).
    Recording,
    /// Something needs a look: a device is short of disk or cannot meet the
    /// recording profile. Amber.
    NeedsAttention,
    /// Guests are still syncing their tracks to the library. Blue.
    Syncing,
}

impl Badge {
    /// The badge's colour: the client's `--rec`, `--warn` and `--accent` tokens.
    pub fn rgb(self) -> [u8; 3] {
        match self {
            Badge::Recording => [220, 38, 38],
            Badge::NeedsAttention => [251, 191, 36],
            Badge::Syncing => [138, 180, 248],
        }
    }
}

/// Which badge the tray icon wears for `live`, if any. When several apply the
/// most urgent wins: red recording, then amber needs a look, then blue syncing.
pub fn tray_badge(live: &LiveStatus) -> Option<Badge> {
    if live.recording.is_some() {
        Some(Badge::Recording)
    } else if live.attention > 0 {
        Some(Badge::NeedsAttention)
    } else if live.syncing > 0 {
        Some(Badge::Syncing)
    } else {
        None
    }
}

/// The one line the tray shows for `live`: guests, the recording take (or
/// idle) and outboxes still syncing.
pub fn tray_text(live: &LiveStatus) -> String {
    let guests = match live.guests {
        1 => "1 guest".to_string(),
        n => format!("{n} guests"),
    };
    let take = match live.recording {
        Some(t) => format!("recording take {t}"),
        None => "not recording".to_string(),
    };
    let mut text = format!("{guests}, {take}");
    if live.syncing > 0 {
        text.push_str(&format!(", syncing {}", live.syncing));
    }
    if live.attention > 0 {
        text.push_str(&format!(", {} to check", live.attention));
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tray_text_says_guests_take_and_sync() {
        let mut live = LiveStatus::default();
        assert_eq!(tray_text(&live), "0 guests, not recording");
        live.guests = 1;
        live.recording = Some(3);
        assert_eq!(tray_text(&live), "1 guest, recording take 3");
        live.guests = 2;
        live.recording = None;
        live.syncing = 1;
        assert_eq!(tray_text(&live), "2 guests, not recording, syncing 1");
    }

    #[test]
    fn the_badge_follows_the_most_urgent_state() {
        let mut live = LiveStatus::default();
        assert_eq!(tray_badge(&live), None);
        live.guests = 2;
        assert_eq!(tray_badge(&live), None);
        live.syncing = 1;
        assert_eq!(tray_badge(&live), Some(Badge::Syncing));
        live.attention = 1;
        assert_eq!(tray_badge(&live), Some(Badge::NeedsAttention));
        assert_eq!(
            tray_text(&live),
            "2 guests, not recording, syncing 1, 1 to check"
        );
        live.recording = Some(4);
        assert_eq!(tray_badge(&live), Some(Badge::Recording));
        live.syncing = 0;
        live.attention = 0;
        assert_eq!(tray_badge(&live), Some(Badge::Recording));
        live.recording = None;
        assert_eq!(tray_badge(&live), None);
    }

    #[test]
    fn badge_colours_are_distinct() {
        let all = [Badge::Recording, Badge::NeedsAttention, Badge::Syncing];
        for (i, a) in all.iter().enumerate() {
            for b in &all[i + 1..] {
                assert_ne!(a.rgb(), b.rgb());
            }
        }
    }
}

/// The call window's script for `origin` and the code's `fragment`: it sets
/// `window.__castcorderAutoLink` only on a page of that origin, so another
/// host's LAN page loaded in the window never sees the code.
pub fn autolink_script(origin: &str, fragment: &str) -> String {
    // A JS string literal; anything but plain URL characters escaped.
    let q = |s: &str| {
        let body: String = s
            .chars()
            .map(|c| {
                if c.is_ascii_alphanumeric() || matches!(c, '.' | ':' | '/' | '_' | '-') {
                    c.to_string()
                } else {
                    format!("\\u{:04x}", c as u32)
                }
            })
            .collect();
        format!("\"{body}\"")
    };
    format!(
        "if (window.location.origin === {}) {{ window.__castcorderAutoLink = {}; }}",
        q(origin),
        q(fragment)
    )
}

#[cfg(test)]
mod autolink_tests {
    #[test]
    fn the_script_names_the_origin_and_the_code() {
        let s = super::autolink_script("https://cast.example", "link.h.s");
        assert_eq!(
            s,
            r#"if (window.location.origin === "https://cast.example") { window.__castcorderAutoLink = "link.h.s"; }"#
        );
        assert!(super::autolink_script("x\"y", "a</b").contains(r#""x\u0022y""#));
    }
}
