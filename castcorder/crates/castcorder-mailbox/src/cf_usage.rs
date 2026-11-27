//! Relay metering for Cloudflare Realtime TURN (cast-lxt.10): the I/O-free
//! half. The Worker tags every credential it mints with the host id
//! (`customIdentifier`), asks Cloudflare's GraphQL analytics
//! (`callsTurnUsageAdaptiveGroups`) for that identifier's egress over the
//! last [`WINDOW_DAYS`] days, caches the answer per host key for
//! [`REFRESH_MS`], and refuses new credentials once it reaches the budget,
//! as castcorder-rendezvous does from coturn's exporter. Egress (relay to
//! client) is what Cloudflare bills.
//!
//! One identifier per query: Cloudflare's docs warn that adaptive sampling
//! can skew a query spanning many identifiers.

use castcorder_link::HostId;
use castcorder_link::wire::{IceServer, code};
use serde::{Deserialize, Serialize};

/// The rolling window a budget covers, as castcorder-rendezvous's ledger.
pub const WINDOW_DAYS: u64 = 30;

/// How long a host key's usage figure is trusted before it is asked again.
/// Analytics lag by minutes anyway; one query per active host key per quarter
/// hour stays far inside the GraphQL API's limits.
pub const REFRESH_MS: u64 = 15 * 60 * 1000;

/// Cloudflare's GraphQL endpoint.
pub const GRAPHQL_URL: &str = "https://api.cloudflare.com/client/v4/graphql";

/// The `customIdentifier` a host's credentials carry: its host id.
pub fn identifier(host: &HostId) -> String {
    host.encode()
}

/// The body of a credential request to the Realtime TURN API
/// (`credentials/generate`): lifetime and the host's identifier.
pub fn generate_body(host: &HostId, ttl: u64) -> String {
    serde_json::json!({ "ttl": ttl, "customIdentifier": identifier(host) }).to_string()
}

/// The ICE servers in a Realtime TURN credential answer. `generate-ice-servers`
/// answers `{"iceServers": [..]}`; `credentials/generate`, whose answer the
/// docs do not show, has answered `{"iceServers": {..}}` (one entry), so
/// both are read.
pub fn generated_servers(body: &str) -> Result<Vec<IceServer>, String> {
    #[derive(Deserialize)]
    #[serde(untagged)]
    enum Servers {
        Many(Vec<IceServer>),
        One(IceServer),
    }
    #[derive(Deserialize)]
    struct Generated {
        #[serde(rename = "iceServers")]
        ice_servers: Servers,
    }
    let g: Generated = serde_json::from_str(body).map_err(|e| format!("unreadable answer: {e}"))?;
    Ok(match g.ice_servers {
        Servers::Many(v) => v,
        Servers::One(s) => vec![s],
    })
}

/// A `credentials/generate` entry with the URLs `generate-ice-servers` would
/// have given: that endpoint answers only the standard ports (3478, TLS on
/// 5349) plus a `stun:` URL, while Cloudflare's TURN also listens on TCP 80
/// and TLS 443, which strict firewalls leave open. So beside each
/// `turn:HOST:3478?transport=tcp` this adds `turn:HOST:80?transport=tcp`,
/// beside each `turns:HOST:5349?transport=tcp` it adds
/// `turns:HOST:443?transport=tcp`, and it drops `stun:` URLs (every hello
/// carries STUN on its own).
pub fn with_firewall_ports(mut server: IceServer) -> IceServer {
    let mut urls = Vec::with_capacity(server.urls.len() + 2);
    for u in server.urls.drain(..) {
        if u.starts_with("stun:") {
            continue;
        }
        let extra = [
            ("turn:", ":3478?transport=tcp", ":80?transport=tcp"),
            ("turns:", ":5349?transport=tcp", ":443?transport=tcp"),
        ]
        .iter()
        .find(|(scheme, port, _)| u.starts_with(scheme) && u.ends_with(port))
        .map(|(_, port, alt)| format!("{}{alt}", &u[..u.len() - port.len()]));
        urls.push(u);
        urls.extend(extra);
    }
    urls.dedup();
    server.urls = urls;
    server
}

/// A host key's last known relay usage, as stored in its Durable Object.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Usage {
    /// Egress bytes in the window when last asked.
    pub bytes: u64,
    /// When that was (unix ms); 0 for never.
    pub checked_ms: u64,
}

impl Usage {
    /// Whether the figure is recent enough to use without asking again.
    pub fn fresh(&self, now_ms: u64) -> bool {
        self.checked_ms > 0 && now_ms.saturating_sub(self.checked_ms) < REFRESH_MS
    }

    /// Why the host gets no relay credentials, if it is at or over `budget`.
    pub fn refusal(&self, budget: u64) -> Option<String> {
        (self.bytes >= budget).then(|| budget_refusal(self.bytes, budget))
    }
}

/// The `relay_error` for a host key at or over its relay budget, worded as
/// castcorder-rendezvous words it.
pub fn budget_refusal(used: u64, budget: u64) -> String {
    format!(
        "{}: this host has used its relay budget ({used} of {budget} bytes in {WINDOW_DAYS} days); direct connections still work",
        code::RELAY_QUOTA
    )
}

/// The UTC date (`YYYY-MM-DD`) of `ms` since the unix epoch.
pub fn utc_date(ms: u64) -> String {
    // Howard Hinnant's civil_from_days.
    let z = (ms / 86_400_000) as i64 + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02}")
}

/// The GraphQL request (JSON) for `host`'s egress on `account` over the
/// [`WINDOW_DAYS`] days up to `now_ms`, today included.
pub fn query(account: &str, host: &HostId, now_ms: u64) -> String {
    let from = utc_date(now_ms.saturating_sub((WINDOW_DAYS - 1) * 86_400_000));
    let to = utc_date(now_ms);
    let q = "query($account: String!, $id: String!, $from: Date!, $to: Date!) { viewer { accounts(filter: { accountTag: $account }) { callsTurnUsageAdaptiveGroups(filter: { date_geq: $from, date_leq: $to, customIdentifier: $id }, limit: 1) { sum { egressBytes } } } } }";
    serde_json::json!({
        "query": q,
        "variables": { "account": account, "id": identifier(host), "from": from, "to": to },
    })
    .to_string()
}

#[derive(Deserialize)]
struct Answer {
    data: Option<Data>,
    #[serde(default)]
    errors: Option<Vec<serde_json::Value>>,
}

#[derive(Deserialize)]
struct Data {
    viewer: Viewer,
}

#[derive(Deserialize)]
struct Viewer {
    accounts: Vec<Account>,
}

#[derive(Deserialize)]
struct Account {
    #[serde(rename = "callsTurnUsageAdaptiveGroups")]
    groups: Vec<Group>,
}

#[derive(Deserialize)]
struct Group {
    sum: Sum,
}

#[derive(Deserialize)]
struct Sum {
    #[serde(rename = "egressBytes")]
    egress_bytes: u64,
}

/// The egress bytes in an answer to [`query`]: the sum over its groups (none
/// when the identifier relayed nothing). `Err` for GraphQL errors or an
/// answer of another shape.
pub fn egress_bytes(body: &str) -> Result<u64, String> {
    let a: Answer = serde_json::from_str(body).map_err(|e| format!("unreadable answer: {e}"))?;
    if let Some(errs) = a.errors.filter(|e| !e.is_empty()) {
        let first = errs[0]
            .get("message")
            .and_then(|m| m.as_str())
            .unwrap_or("unknown error")
            .chars()
            .take(200)
            .collect::<String>();
        return Err(format!("analytics error: {first}"));
    }
    let data = a.data.ok_or("analytics answer without data")?;
    Ok(data
        .viewer
        .accounts
        .iter()
        .flat_map(|acc| &acc.groups)
        .map(|g| g.sum.egress_bytes)
        .sum())
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_link::HostKey;

    #[test]
    fn dates_are_utc_calendar_days() {
        assert_eq!(utc_date(0), "1970-01-01");
        assert_eq!(utc_date(951_782_400_000), "2000-02-29");
        assert_eq!(utc_date(1_795_747_024_000), "2026-11-27");
        assert_eq!(utc_date(1_798_761_599_999), "2026-12-31");
        assert_eq!(utc_date(1_798_761_600_000), "2027-01-01");
    }

    #[test]
    fn the_query_names_one_host_over_thirty_days() {
        let id = HostKey::from_secret([4; 32]).id();
        let q: serde_json::Value =
            serde_json::from_str(&query("acct", &id, 1_795_747_024_000)).unwrap();
        let v = &q["variables"];
        assert_eq!(v["account"], "acct");
        assert_eq!(v["id"], id.encode());
        assert_eq!(v["from"], "2026-10-29");
        assert_eq!(v["to"], "2026-11-27");
        let text = q["query"].as_str().unwrap();
        assert!(text.contains("callsTurnUsageAdaptiveGroups"));
        assert!(text.contains("customIdentifier: $id"));
        assert!(text.contains("egressBytes"));
    }

    #[test]
    fn the_credential_request_carries_the_host_id() {
        let id = HostKey::from_secret([4; 32]).id();
        let b: serde_json::Value = serde_json::from_str(&generate_body(&id, 600)).unwrap();
        assert_eq!(b["ttl"], 600);
        assert_eq!(b["customIdentifier"], id.encode());
    }

    #[test]
    fn both_credential_answers_are_read() {
        let many = r#"{"iceServers":[{"urls":["stun:stun.cloudflare.com:3478"]},{"urls":["turn:turn.cloudflare.com:3478?transport=udp"],"username":"u","credential":"c"}]}"#;
        let v = generated_servers(many).unwrap();
        assert_eq!(v.len(), 2);
        assert_eq!(v[1].username.as_deref(), Some("u"));
        let one = r#"{"iceServers":{"urls":["turn:turn.cloudflare.com:3478?transport=udp"],"username":"u","credential":"c"}}"#;
        let v = generated_servers(one).unwrap();
        assert_eq!(v.len(), 1);
        assert_eq!(v[0].credential.as_deref(), Some("c"));
        assert!(generated_servers(r#"{"result":null}"#).is_err());
    }

    #[test]
    fn tagged_credentials_get_the_firewall_ports() {
        let s = IceServer {
            urls: [
                "stun:stun.cloudflare.com:3478",
                "turn:turn.cloudflare.com:3478?transport=udp",
                "turn:turn.cloudflare.com:3478?transport=tcp",
                "turns:turn.cloudflare.com:5349?transport=tcp",
            ]
            .map(String::from)
            .to_vec(),
            username: Some("u".into()),
            credential: Some("c".into()),
            expires: None,
        };
        assert_eq!(
            with_firewall_ports(s).urls,
            [
                "turn:turn.cloudflare.com:3478?transport=udp",
                "turn:turn.cloudflare.com:3478?transport=tcp",
                "turn:turn.cloudflare.com:80?transport=tcp",
                "turns:turn.cloudflare.com:5349?transport=tcp",
                "turns:turn.cloudflare.com:443?transport=tcp",
            ]
        );
    }

    #[test]
    fn egress_is_summed_and_errors_are_errors() {
        let ok = r#"{"data":{"viewer":{"accounts":[{"callsTurnUsageAdaptiveGroups":[{"sum":{"egressBytes":1200}},{"sum":{"egressBytes":34}}]}]}},"errors":null}"#;
        assert_eq!(egress_bytes(ok), Ok(1234));
        let none = r#"{"data":{"viewer":{"accounts":[{"callsTurnUsageAdaptiveGroups":[]}]}},"errors":null}"#;
        assert_eq!(egress_bytes(none), Ok(0));
        let bad = r#"{"data":null,"errors":[{"message":"not authorized for that account"}]}"#;
        assert_eq!(
            egress_bytes(bad),
            Err("analytics error: not authorized for that account".into())
        );
        assert!(egress_bytes("<html>").is_err());
    }

    #[test]
    fn usage_is_refreshed_every_quarter_hour_and_refuses_at_the_budget() {
        let u = Usage {
            bytes: 500,
            checked_ms: 1_000_000,
        };
        assert!(u.fresh(1_000_000 + REFRESH_MS - 1));
        assert!(!u.fresh(1_000_000 + REFRESH_MS));
        assert!(!Usage::default().fresh(5));
        assert_eq!(u.refusal(501), None);
        let why = u.refusal(500).unwrap();
        assert!(why.starts_with(code::RELAY_QUOTA), "{why}");
        assert!(why.contains("500 of 500 bytes in 30 days"), "{why}");
    }
}
