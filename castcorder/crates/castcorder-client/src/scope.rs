//! Which host and which call a recording belongs to (the `call` a track's
//! sidecar and a log file's lines carry), so the outbox offers every track
//! of a host whichever of its pages is open.
//!
//! - A host's scope is `link-<host>` for a page opened from a call link, else
//!   the LAN signaling URL without its query ([`lan_host_scope`]): `?room=`
//!   only picks a room of the same host, so every room's page of one host
//!   shares one scope. A track recorded in a call is kept under
//!   `<host scope>/<room>/<call>`; one from before rooms under the host scope.
//! - Sidecars written before this carry the query (`<url>?room=X/<room>/<call>`);
//!   every comparison goes through [`normalize`], so those still sync from any
//!   room's page of their host (cast-9hs).

/// The host scope of a LAN or host machine page's signaling URL: the URL
/// without its query.
pub fn lan_host_scope(signal_url: &str) -> String {
    match signal_url.split_once('?') {
        Some((base, _)) => base.to_string(),
        None => signal_url.to_string(),
    }
}

/// `scope` with the query of its URL part removed: up to the next `/` (the
/// query's values are URI-encoded, so they hold none), else to the end.
pub fn normalize(scope: &str) -> String {
    let Some((base, query)) = scope.split_once('?') else {
        return scope.to_string();
    };
    match query.find('/') {
        Some(i) => format!("{base}{}", &query[i..]),
        None => base.to_string(),
    }
}

/// Whether a recording's `call` belongs to host scope `legacy`: one of its
/// calls (`<legacy>/<room>/<call>`), or from before rooms (`<legacy>`).
pub fn in_scope(meta_call: &str, legacy: &str) -> bool {
    let (meta_call, legacy) = (normalize(meta_call), normalize(legacy));
    !legacy.is_empty()
        && (meta_call == legacy
            || meta_call
                .strip_prefix(legacy.as_str())
                .is_some_and(|rest| rest.starts_with('/')))
}

/// Whether `call` names the host scope `legacy` itself (no room, no call).
pub fn is_host(call: &str, legacy: &str) -> bool {
    normalize(call) == normalize(legacy)
}

/// The room and call a recording kept under `scope` is offered for, by a
/// page whose current call scope is `call` and host scope `legacy`: empty
/// for the current call and for recordings from before rooms (`OfferTrack`),
/// else the call of that host it was recorded in (`OfferCallTrack`).
pub fn offer_call(scope: &str, call: &str, legacy: &str) -> (String, String) {
    let (scope, call, legacy) = (normalize(scope), normalize(call), normalize(legacy));
    if scope == call || legacy.is_empty() {
        return Default::default();
    }
    let Some(rest) = scope
        .strip_prefix(legacy.as_str())
        .and_then(|r| r.strip_prefix('/'))
    else {
        return Default::default();
    };
    match rest.split_once('/') {
        Some((room, call)) if !room.is_empty() && !call.is_empty() && !call.contains('/') => {
            (room.to_string(), call.to_string())
        }
        _ => Default::default(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LATE: &str = "http://h:1/offer?room=Late-show";
    const MAIN: &str = "http://h:1/offer?room=main-show";
    const BARE: &str = "http://h:1/offer";

    /// A take recorded in a Late-show call: under the scope this build
    /// writes, and under the one earlier builds wrote (with `?room=`).
    fn takes() -> [String; 2] {
        [
            format!("{}/Late-show/2026-11-17-3", lan_host_scope(LATE)),
            format!("{LATE}/Late-show/2026-11-17-3"),
        ]
    }

    #[test]
    fn every_room_page_of_a_host_shares_one_scope() {
        assert_eq!(lan_host_scope(LATE), BARE);
        assert_eq!(lan_host_scope(MAIN), BARE);
        assert_eq!(lan_host_scope(BARE), BARE);
        assert_eq!(lan_host_scope("link-abc"), "link-abc");
    }

    /// cast-9hs: a producer left a Late-show call before its take synced and
    /// opened main-show's page; that page never offered the take.
    #[test]
    fn a_take_of_one_room_syncs_from_another_rooms_page() {
        for page in [MAIN, BARE, LATE] {
            let legacy = lan_host_scope(page);
            let in_main = format!("{legacy}/main-show/2026-11-17");
            for take in takes() {
                assert!(in_scope(&take, &legacy), "{take} from {page}");
                assert_eq!(
                    offer_call(&take, &in_main, &legacy),
                    ("Late-show".into(), "2026-11-17-3".into()),
                    "{take} from {page}"
                );
                // A sync-only connection (in no call: the host scope).
                assert_eq!(
                    offer_call(&take, &legacy, &legacy),
                    ("Late-show".into(), "2026-11-17-3".into())
                );
            }
        }
    }

    #[test]
    fn the_current_call_and_tracks_from_before_rooms_go_as_offer_track() {
        let legacy = lan_host_scope(LATE);
        let current = format!("{legacy}/Late-show/2026-11-17-3");
        for take in takes() {
            assert_eq!(offer_call(&take, &current, &legacy), Default::default());
        }
        for old in [LATE, BARE] {
            assert!(in_scope(old, &legacy));
            assert!(is_host(old, &legacy));
            assert_eq!(offer_call(old, &current, &legacy), Default::default());
        }
    }

    #[test]
    fn other_hosts_and_link_scopes_stay_apart() {
        let legacy = lan_host_scope(MAIN);
        assert!(!in_scope("http://h:2/offer/Late-show/c", &legacy));
        assert!(!in_scope("http://h:1/offerx/Late-show/c", &legacy));
        assert!(!in_scope("link-abc/Late-show/c", &legacy));
        assert!(!in_scope(BARE, ""));
        assert!(in_scope("link-abc/Late-show/c", "link-abc"));
        assert_eq!(
            offer_call("link-abc/Late-show/c", "link-abc", "link-abc"),
            ("Late-show".into(), "c".into())
        );
        assert!(!is_host("link-abc/Late-show/c", "link-abc"));
    }
}
