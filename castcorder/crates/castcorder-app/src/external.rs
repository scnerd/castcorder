//! Opening a web page in the system browser (cast-0wr): the home's "Report a
//! bug" link and the tray item. Only the bug-report page is ever opened.

use std::process::Command;

/// Open `url` in the system browser; false if it is not the bug-report page
/// or no opener could be started.
pub fn open_bug_report(url: &str) -> bool {
    if !castcorder_core::bugreport::is_bug_report(url) {
        return false;
    }
    #[cfg(windows)]
    let mut cmd = {
        // rundll32 takes the address as one argument; `start` would need `&` escaped.
        let mut c = Command::new("rundll32");
        c.args(["url.dll,FileProtocolHandler", url]);
        c
    };
    #[cfg(target_os = "macos")]
    let mut cmd = {
        let mut c = Command::new("open");
        c.arg(url);
        c
    };
    #[cfg(all(unix, not(target_os = "macos")))]
    let mut cmd = {
        let mut c = Command::new("xdg-open");
        c.arg(url);
        c
    };
    cmd.spawn().is_ok()
}

/// The bug-report page for this app: its version and operating system filled in.
pub fn bug_report_url() -> String {
    castcorder_core::bugreport::url(env!("CARGO_PKG_VERSION"), std::env::consts::OS)
}
