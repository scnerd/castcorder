//! The bug-report link (cast-0wr): the GitHub new-issue page with the issue form's
//! fields filled in. The home's "Report a bug" link and the tray item use it.

/// The repository's new-issue page, for the bug-report form.
const NEW_ISSUE: &str = "https://github.com/scnerd/castcorder/issues/new?template=bug_report.yml";

/// The page that starts a bug report, with the form's version and system
/// fields filled in (`.github/ISSUE_TEMPLATE/bug_report.yml`).
pub fn url(version: &str, os: &str) -> String {
    let mut url = String::from(NEW_ISSUE);
    if !version.is_empty() {
        url.push_str(&format!("&version={}", encode(version)));
    }
    if !os.is_empty() {
        url.push_str(&format!("&system={}", encode(os)));
    }
    url
}

/// Whether `u` is the bug-report page: the only address the app opens in the
/// system browser on a page's request.
pub fn is_bug_report(u: &str) -> bool {
    u.starts_with(NEW_ISSUE)
}

fn encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'.' | b'_' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::{is_bug_report, url};

    #[test]
    fn the_bug_report_link_prefills_version_and_system() {
        assert_eq!(
            url("1.2.3", "windows"),
            "https://github.com/scnerd/castcorder/issues/new?template=bug_report.yml&version=1.2.3&system=windows"
        );
        assert_eq!(url("", ""), NEW_ISSUE_ONLY);
        assert!(url("1 2&3", "").ends_with("&version=1%202%263"));
    }

    const NEW_ISSUE_ONLY: &str =
        "https://github.com/scnerd/castcorder/issues/new?template=bug_report.yml";

    #[test]
    fn only_the_bug_report_page_is_opened_outside_the_app() {
        assert!(is_bug_report(&url("1", "linux")));
        assert!(!is_bug_report("https://example.com/"));
        assert!(!is_bug_report(
            "https://github.com/scnerd/castcorder/issues/new?template=x"
        ));
        assert!(!is_bug_report(
            "https://github.com.evil.example/scnerd/castcorder/issues/new?template=bug_report.yml"
        ));
    }
}
