//! Relay (TURN) usage per host key, and the budget that stops minting relay
//! credentials for a host key that has used it up.
//!
//! Every credential the rendezvous mints is named `<expiry>:<host id>` (see
//! [`crate::turn`]), for the host and for the guests calling it, so coturn's
//! Prometheus exporter (`--prometheus --prometheus-username-labels`) reports
//! relayed bytes per credential, and so per host key. coturn adds a
//! session's bytes to its `turn_traffic_rcvb`/`turn_traffic_sentb` counters
//! when the session ends (deallocated, expired, or its credentials expired),
//! so usage lags a live allocation by up to the credential TTL.
//!
//! The [`Ledger`] turns those cumulative per-credential counters into bytes
//! per host key per UTC day: each scrape adds the growth of every counter
//! since the last scrape (all of it for a counter that is new, or that went
//! down because coturn restarted). Usage is the sum of the last
//! [`WINDOW_DAYS`] days. The ledger is one small JSON file in the data dir,
//! written atomically, so a restarted rendezvous neither forgets nor
//! re-counts. Without that file (a first start, no data dir, or the file was
//! lost) the ledger is [`Ledger::fresh`]: its first scrape only notes the
//! counters, since what coturn counted before cannot be told apart from what
//! a lost ledger already counted, and counting it again would put it all on
//! today and could refuse a host key that is within its budget (cast-gln).
//! Usage then under-counts what was relayed before, never over-counts it.
//!
//! Pure functions of their inputs and `now`; the scraping loop is in
//! [`crate::server`].

use std::collections::BTreeMap;
use std::io;
use std::path::Path;

use castcorder_link::HostId;
use serde::{Deserialize, Serialize};

/// The rolling budget window, in days.
pub const WINDOW_DAYS: u64 = castcorder_mailbox::cf_usage::WINDOW_DAYS;
const DAY_S: u64 = 86_400;

/// The ledger's file name in the data dir.
pub const LEDGER_FILE: &str = "relay-usage.json";

/// Relayed bytes per coturn username from a Prometheus text exposition:
/// `turn_traffic_rcvb` plus `turn_traffic_sentb` (bytes between the clients
/// and coturn; the peer-side counters mirror them and are not added).
/// Lines without a `user` label (coturn without
/// `--prometheus-username-labels`) are skipped.
pub fn parse_metrics(text: &str) -> BTreeMap<String, u64> {
    let mut out = BTreeMap::new();
    for line in text.lines() {
        let Some(rest) = line
            .strip_prefix("turn_traffic_rcvb{")
            .or_else(|| line.strip_prefix("turn_traffic_sentb{"))
        else {
            continue;
        };
        let Some((labels, value)) = rest.split_once('}') else {
            continue;
        };
        let Some(user) = labels
            .split_once("user=\"")
            .and_then(|(_, u)| u.split_once('"'))
            .map(|(u, _)| u)
        else {
            continue;
        };
        // Counters are printed as integers or floats; ignore a timestamp.
        let Some(v) = value
            .split_whitespace()
            .next()
            .and_then(|v| v.parse::<f64>().ok())
            .filter(|v| v.is_finite() && *v >= 0.0)
        else {
            continue;
        };
        // Truncation is fine: byte counters are integers.
        #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
        let v = v as u64;
        *out.entry(user.to_string()).or_insert(0) += v;
    }
    out
}

/// The host key a minted username belongs to: `<expiry>:<host id>[:...]`.
pub fn host_of_username(user: &str) -> Option<HostId> {
    let mut parts = user.split(':');
    parts.next()?.parse::<u64>().ok()?;
    HostId::parse(parts.next()?).ok()
}

fn day(now_unix: u64) -> u64 {
    now_unix / DAY_S
}

/// Relayed bytes per host key per day, and the last value of every coturn
/// counter seen.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Ledger {
    /// Host id (encoded) to UTC day number to bytes.
    hosts: BTreeMap<String, BTreeMap<u64, u64>>,
    /// coturn username to the counter value at the last scrape.
    seen: BTreeMap<String, u64>,
    /// Made without a ledger on disk and not scraped yet: the first scrape
    /// only notes the counters ([`Ledger::fresh`]).
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    fresh: bool,
}

/// One host key's usage, as reported to the operator.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct HostUsage {
    /// The host id.
    pub host: String,
    /// Bytes relayed in the last [`WINDOW_DAYS`] days.
    pub bytes: u64,
    /// Whether that is at or over the budget.
    pub over_budget: bool,
}

/// The operator's view: `GET /usage` on the admin listener, and
/// `castcorder-rendezvous usage --json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct UsageReport {
    /// The rolling window, in days.
    pub window_days: u64,
    /// Bytes per host key per window, if a budget is enforced.
    pub budget_bytes: Option<u64>,
    /// Host keys with usage, most bytes first.
    pub hosts: Vec<HostUsage>,
}

impl Ledger {
    /// A ledger made without one on disk: its first [`Ledger::observe`]
    /// takes coturn's counters as they are and adds nothing (see the module
    /// docs).
    pub fn fresh() -> Ledger {
        Ledger {
            fresh: true,
            ..Ledger::default()
        }
    }

    /// The operator's view at `now_unix`.
    pub fn usage_report(&self, budget: Option<u64>, now_unix: u64) -> UsageReport {
        UsageReport {
            window_days: WINDOW_DAYS,
            budget_bytes: budget,
            hosts: self.report(budget, now_unix),
        }
    }

    /// Account one scrape (`parse_metrics`) at `now_unix`. Returns the bytes
    /// added.
    pub fn observe(&mut self, counters: &BTreeMap<String, u64>, now_unix: u64) -> u64 {
        if std::mem::take(&mut self.fresh) {
            self.seen = counters
                .iter()
                .filter(|(u, _)| host_of_username(u).is_some())
                .map(|(u, &v)| (u.clone(), v))
                .collect();
            return 0;
        }
        let mut added = 0;
        for (user, &v) in counters {
            let Some(host) = host_of_username(user) else {
                continue;
            };
            let delta = match self.seen.get(user) {
                Some(&last) if v >= last => v - last,
                // New, or coturn restarted and counts from zero again.
                _ => v,
            };
            self.seen.insert(user.clone(), v);
            if delta > 0 {
                self.record(&host, delta, now_unix);
                added += delta;
            }
        }
        // A counter coturn no longer exports (it restarted) starts afresh if
        // it comes back.
        self.seen.retain(|u, _| counters.contains_key(u));
        self.prune(now_unix);
        added
    }

    /// Add `bytes` to `host` on the day of `now_unix`.
    pub fn record(&mut self, host: &HostId, bytes: u64, now_unix: u64) {
        let d = self.hosts.entry(host.encode()).or_default();
        let b = d.entry(day(now_unix)).or_insert(0);
        *b = b.saturating_add(bytes);
    }

    fn first_day(now_unix: u64) -> u64 {
        (day(now_unix) + 1).saturating_sub(WINDOW_DAYS)
    }

    fn prune(&mut self, now_unix: u64) {
        let first = Self::first_day(now_unix);
        for days in self.hosts.values_mut() {
            days.retain(|d, _| *d >= first);
        }
        self.hosts.retain(|_, days| !days.is_empty());
    }

    fn used_encoded(&self, host: &str, now_unix: u64) -> u64 {
        let first = Self::first_day(now_unix);
        self.hosts.get(host).map_or(0, |days| {
            days.range(first..)
                .fold(0u64, |a, (_, b)| a.saturating_add(*b))
        })
    }

    /// Bytes relayed for `host` in the last [`WINDOW_DAYS`] days.
    pub fn used(&self, host: &HostId, now_unix: u64) -> u64 {
        self.used_encoded(&host.encode(), now_unix)
    }

    /// Every host key with usage in the window, most bytes first.
    pub fn report(&self, budget: Option<u64>, now_unix: u64) -> Vec<HostUsage> {
        let mut v: Vec<HostUsage> = self
            .hosts
            .keys()
            .map(|h| {
                let bytes = self.used_encoded(h, now_unix);
                HostUsage {
                    host: h.clone(),
                    bytes,
                    over_budget: budget.is_some_and(|b| bytes >= b),
                }
            })
            .filter(|u| u.bytes > 0)
            .collect();
        v.sort_by(|a, b| b.bytes.cmp(&a.bytes).then_with(|| a.host.cmp(&b.host)));
        v
    }

    /// Why `host` gets no relay credentials, if it is at or over `budget`:
    /// `relay_quota: ...`.
    pub fn refusal(&self, host: &HostId, budget: Option<u64>, now_unix: u64) -> Option<String> {
        let budget = budget?;
        let used = self.used(host, now_unix);
        (used >= budget).then(|| castcorder_mailbox::cf_usage::budget_refusal(used, budget))
    }

    /// The ledger as stored.
    pub fn to_json(&self) -> String {
        serde_json::to_string(self).expect("a ledger serializes")
    }

    /// Read the ledger at `path`; [`Ledger::fresh`] if there is none.
    pub fn load(path: &Path) -> io::Result<Ledger> {
        match std::fs::read(path) {
            Ok(b) => serde_json::from_slice(&b).map_err(io::Error::other),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Ledger::fresh()),
            Err(e) => Err(e),
        }
    }
}

/// Write `json` (a [`Ledger::to_json`]) to `path` atomically: a temporary
/// file beside it, then a rename.
pub fn save(path: &Path, json: &str) -> io::Result<()> {
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json)?;
    std::fs::rename(&tmp, path)
}

/// Parse a byte count with an optional SI suffix: `500000`, `20G`, `1.5T`.
pub fn parse_bytes(v: &str) -> Option<u64> {
    let v = v.trim();
    let (num, mult) = match v.char_indices().last()? {
        (i, 'k' | 'K') => (&v[..i], 1e3),
        (i, 'm' | 'M') => (&v[..i], 1e6),
        (i, 'g' | 'G') => (&v[..i], 1e9),
        (i, 't' | 'T') => (&v[..i], 1e12),
        _ => (v, 1.0),
    };
    let n: f64 = num.parse().ok()?;
    let b = n * mult;
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    (0.0..1.8e19).contains(&b).then_some(b as u64)
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_link::HostKey;

    fn id(n: u8) -> HostId {
        HostKey::from_secret([n; 32]).id()
    }

    fn user(expiry: u64, host: &HostId) -> String {
        format!("{expiry}:{}", host.encode())
    }

    const T0: u64 = 1_800_000_000;

    #[test]
    fn parses_coturn_exposition() {
        let h = id(1);
        let u = user(T0, &h);
        // Shape of coturn 4.6's output with --prometheus-username-labels.
        let text = format!(
            "# HELP turn_traffic_sentb Represents finished sessions sent bytes\n\
             # TYPE turn_traffic_sentb counter\n\
             turn_traffic_sentb{{realm=\"r\",user=\"{u}\"}} 1394776\n\
             turn_traffic_rcvb{{realm=\"r\",user=\"{u}\"}} 1395536\n\
             turn_traffic_peer_sentb{{realm=\"r\",user=\"{u}\"}} 999\n\
             turn_traffic_rcvp{{realm=\"r\",user=\"{u}\"}} 5000\n\
             turn_total_traffic_rcvb 7\n\
             turn_traffic_rcvb{{realm=\"r\"}} 5\n\
             turn_traffic_sentb{{realm=\"r\",user=\"other\"}} 1.5e3\n\
             turn_total_allocations{{type=\"UDP\"}} 5\n"
        );
        let m = parse_metrics(&text);
        assert_eq!(m.get(&u), Some(&(1_394_776 + 1_395_536)));
        assert_eq!(m.get("other"), Some(&1500));
        assert_eq!(m.len(), 2);
        assert_eq!(host_of_username(&u), Some(h));
        assert_eq!(
            host_of_username(&format!("{T0}:{}:guest", h.encode())),
            Some(h)
        );
        assert_eq!(host_of_username("other"), None);
        assert_eq!(host_of_username("123:not-a-host"), None);
    }

    #[test]
    fn counts_counter_growth_per_host_key_once() {
        let (a, b) = (id(1), id(2));
        let mut l = Ledger::default();
        let mut c = BTreeMap::new();
        c.insert(user(T0, &a), 100);
        c.insert(user(T0 + 5, &a), 50);
        c.insert(user(T0, &b), 7);
        c.insert("junk".into(), 1_000);
        assert_eq!(l.observe(&c, T0), 157);
        assert_eq!(l.used(&a, T0), 150);
        assert_eq!(l.used(&b, T0), 7);
        // The same counters again add nothing; growth adds the difference.
        assert_eq!(l.observe(&c, T0 + 30), 0);
        c.insert(user(T0, &a), 130);
        assert_eq!(l.observe(&c, T0 + 60), 30);
        assert_eq!(l.used(&a, T0 + 60), 180);
        // coturn restarted: the counters are gone, then count from zero.
        assert_eq!(l.observe(&BTreeMap::new(), T0 + 90), 0);
        let mut again = BTreeMap::new();
        again.insert(user(T0, &a), 20);
        assert_eq!(l.observe(&again, T0 + 120), 20);
        assert_eq!(l.used(&a, T0 + 120), 200);
        // A counter that went down without vanishing between scrapes.
        again.insert(user(T0, &a), 5);
        assert_eq!(l.observe(&again, T0 + 150), 5);
    }

    #[test]
    fn a_ledger_made_afresh_does_not_count_coturns_counters_again() {
        // The ledger was lost (or this is a first start) while coturn kept
        // its counters: they were counted before, so the first scrape only
        // notes them; growth after it counts as usual.
        let a = id(1);
        let dir = std::env::temp_dir().join(format!("cc-ledger-lost-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LEDGER_FILE);
        let mut l = Ledger::load(&path).unwrap();
        let mut c = BTreeMap::new();
        c.insert(user(T0, &a), 5_000_000);
        assert_eq!(l.observe(&c, T0), 0);
        assert_eq!(l.used(&a, T0), 0);
        c.insert(user(T0, &a), 5_000_100);
        c.insert(user(T0 + 300, &a), 40);
        assert_eq!(l.observe(&c, T0 + 30), 140);
        assert_eq!(l.used(&a, T0 + 30), 140);
        // Saved and loaded again it is no longer fresh: nothing is skipped.
        save(&path, &l.to_json()).unwrap();
        let mut again = Ledger::load(&path).unwrap();
        c.insert(user(T0 + 300, &a), 60);
        assert_eq!(again.observe(&c, T0 + 60), 20);
        assert_eq!(again.used(&a, T0 + 60), 160);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn usage_rolls_off_after_the_window() {
        let a = id(1);
        let mut l = Ledger::default();
        l.record(&a, 1_000, T0);
        l.record(&a, 10, T0 + 10 * DAY_S);
        assert_eq!(l.used(&a, T0 + 10 * DAY_S), 1_010);
        assert_eq!(l.used(&a, T0 + (WINDOW_DAYS - 1) * DAY_S), 1_010);
        assert_eq!(l.used(&a, T0 + WINDOW_DAYS * DAY_S), 10);
        l.observe(&BTreeMap::new(), T0 + 50 * DAY_S);
        assert_eq!(l, Ledger::default(), "old days are pruned");
    }

    #[test]
    fn budget_refuses_at_or_over() {
        let (a, b) = (id(1), id(2));
        let mut l = Ledger::default();
        l.record(&a, 100, T0);
        assert_eq!(l.refusal(&a, None, T0), None);
        assert_eq!(l.refusal(&a, Some(101), T0), None);
        let why = l.refusal(&a, Some(100), T0).unwrap();
        assert!(why.starts_with("relay_quota: "), "{why}");
        assert!(why.is_ascii());
        assert_eq!(l.refusal(&b, Some(1), T0), None, "no usage, no refusal");
        l.record(&b, 5, T0);
        let r = l.report(Some(50), T0);
        assert_eq!(
            r,
            vec![
                HostUsage {
                    host: a.encode(),
                    bytes: 100,
                    over_budget: true
                },
                HostUsage {
                    host: b.encode(),
                    bytes: 5,
                    over_budget: false
                },
            ]
        );
    }

    #[test]
    fn survives_a_restart_on_disk() {
        let dir = std::env::temp_dir().join(format!("cc-ledger-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(LEDGER_FILE);
        let _ = std::fs::remove_file(&path);
        assert_eq!(Ledger::load(&path).unwrap(), Ledger::fresh());
        let a = id(3);
        let mut l = Ledger::default();
        let mut c = BTreeMap::new();
        c.insert(user(T0, &a), 400);
        l.observe(&c, T0);
        save(&path, &l.to_json()).unwrap();
        let mut back = Ledger::load(&path).unwrap();
        assert_eq!(back, l);
        // The restarted rendezvous scrapes the same counters: no re-count.
        assert_eq!(back.observe(&c, T0 + 30), 0);
        assert_eq!(back.used(&a, T0 + 30), 400);
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn parses_byte_counts() {
        assert_eq!(parse_bytes("500000"), Some(500_000));
        assert_eq!(parse_bytes("1"), Some(1));
        assert_eq!(parse_bytes("20G"), Some(20_000_000_000));
        assert_eq!(parse_bytes("1.5T"), Some(1_500_000_000_000));
        assert_eq!(parse_bytes("64k"), Some(64_000));
        assert_eq!(parse_bytes("-1"), None);
        assert_eq!(parse_bytes("lots"), None);
        assert_eq!(parse_bytes(""), None);
    }
}
