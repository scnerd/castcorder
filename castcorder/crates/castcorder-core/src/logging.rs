//! Structured logs that merge across machines (cast-zoa).
//!
//! Every component (host, browser page, browser worker, rendezvous) logs with
//! `tracing` and writes JSON lines of one shape, so the lines from every machine
//! in a call, put in wall-clock order, are one timeline:
//!
//! ```text
//! {"ts":"2026-11-11T12:00:00.123Z","mono_ms":4521.7,"level":"INFO","target":"castcorder_core::outbox",
//!  "fields":{"call":"c1","device":"d1","take":1,"track":"t"},"message":"track added to outbox"}
//! ```
//!
//! - `ts`: wall-clock time, UTC, millisecond precision, fixed width, so plain
//!   string order is time order.
//! - `mono_ms`: monotonic milliseconds of that machine (or JavaScript realm)
//!   since it started logging; orders lines of one source exactly.
//!
//! # Field names
//!
//! Put these on events (or fill them with [`LogContext`]) so a reader can follow
//! one call, one machine, one take or one track: [`CALL`], [`DEVICE`], [`TAKE`],
//! [`TRACK`]. [`REALM`] says where a line came from (`host`, `page`, `worker`).
//!
//! The pieces are sans-IO: time comes from a caller-supplied function, lines go
//! to a caller-supplied sink, files go through the disk port ([`RotatingLog`]).

use std::collections::{BTreeMap, VecDeque};
use std::io;
use std::sync::{Arc, Mutex};

use castcorder_port_disk::{Storage, StorageFile};
use serde_json::Value;
use tracing::field::{Field, Visit};
use tracing::{Event, Level, Subscriber};
use tracing_subscriber::Layer;
use tracing_subscriber::layer::Context;

/// Field: the call (its id or, until rendezvous exists, the signaling URL).
pub const CALL: &str = "call";
/// Field: the device (one browser profile or host machine).
pub const DEVICE: &str = "device";
/// Field: the take number.
pub const TAKE: &str = "take";
/// Field: the track (file stem) a line is about.
pub const TRACK: &str = "track";
/// Field: where the line came from: `host`, `page` or `worker`.
pub const REALM: &str = "realm";

/// One instant, as both clocks report it.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Stamp {
    /// Wall clock: milliseconds since the Unix epoch.
    pub wall_ms: u64,
    /// Monotonic milliseconds since this source started.
    pub mono_ms: f64,
}

/// `2026-11-11T12:00:00.123Z` for `ms` since the Unix epoch.
pub fn utc_iso(ms: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(ms / 1000);
    format!(
        "{y:04}-{mo:02}-{d:02}T{h:02}:{mi:02}:{s:02}.{:03}Z",
        ms % 1000
    )
}

/// `20261111T120000Z`, for file names.
pub fn utc_compact(ms: u64) -> String {
    let (y, mo, d, h, mi, s) = civil(ms / 1000);
    format!("{y:04}{mo:02}{d:02}T{h:02}{mi:02}{s:02}Z")
}

/// (year, month, day, hour, minute, second) of `secs` since the Unix epoch.
fn civil(secs: u64) -> (u64, u64, u64, u64, u64, u64) {
    let days = secs / 86_400;
    let rem = secs % 86_400;
    // Howard Hinnant's civil_from_days, for days since 1970-01-01.
    let z = days + 719_468;
    let era = z / 146_097;
    let doe = z % 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + u64::from(m <= 2);
    (y, m, d, rem / 3600, rem % 3600 / 60, rem % 60)
}

/// One log line (no trailing newline) in the shared shape.
pub fn format_line(
    stamp: Stamp,
    level: &str,
    target: &str,
    fields: &[(String, Value)],
    message: &str,
) -> String {
    let mut f = serde_json::Map::new();
    for (k, v) in fields {
        f.insert(k.clone(), v.clone());
    }
    let s = |v: &str| Value::String(v.into()).to_string();
    format!(
        "{{\"ts\":{},\"mono_ms\":{:.1},\"level\":{},\"target\":{},\"fields\":{},\"message\":{}}}",
        s(&utc_iso(stamp.wall_ms)),
        stamp.mono_ms,
        s(level),
        s(target),
        Value::Object(f),
        s(message)
    )
}

/// Fields added to every line that does not carry them itself (call, device,
/// realm). Cheap to clone; all clones share the values.
#[derive(Clone, Default)]
pub struct LogContext(Arc<Mutex<BTreeMap<String, String>>>);

impl LogContext {
    /// An empty context.
    pub fn new() -> Self {
        Self::default()
    }

    /// Set (or replace) one field.
    pub fn set(&self, key: &str, value: &str) {
        if let Ok(mut m) = self.0.lock() {
            m.insert(key.into(), value.into());
        }
    }

    /// The value of one field.
    pub fn get(&self, key: &str) -> Option<String> {
        self.0.lock().ok().and_then(|m| m.get(key).cloned())
    }

    fn snapshot(&self) -> Vec<(String, String)> {
        self.0
            .lock()
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
            .unwrap_or_default()
    }
}

#[derive(Default)]
struct Collect {
    message: String,
    fields: Vec<(String, Value)>,
}

impl Visit for Collect {
    fn record_str(&mut self, f: &Field, v: &str) {
        if f.name() == "message" {
            self.message = v.into();
        } else {
            self.fields.push((f.name().into(), Value::String(v.into())));
        }
    }
    fn record_i64(&mut self, f: &Field, v: i64) {
        self.fields.push((f.name().into(), v.into()));
    }
    fn record_u64(&mut self, f: &Field, v: u64) {
        self.fields.push((f.name().into(), v.into()));
    }
    fn record_bool(&mut self, f: &Field, v: bool) {
        self.fields.push((f.name().into(), v.into()));
    }
    fn record_f64(&mut self, f: &Field, v: f64) {
        self.fields.push((f.name().into(), v.into()));
    }
    fn record_debug(&mut self, f: &Field, v: &dyn std::fmt::Debug) {
        if f.name() == "message" {
            self.message = format!("{v:?}");
        } else {
            self.fields
                .push((f.name().into(), Value::String(format!("{v:?}"))));
        }
    }
}

/// A `tracing` layer that turns each event into a JSON line and hands it to a
/// sink (a file, a ring buffer, a channel).
pub struct JsonLayer {
    stamp: Box<dyn Fn() -> Stamp + Send + Sync>,
    sink: Box<dyn Fn(String) + Send + Sync>,
    context: LogContext,
    max_level: Level,
}

impl JsonLayer {
    /// Events at `max_level` or more severe are written. `stamp` reads the
    /// clocks, `sink` takes each finished line (without a newline).
    pub fn new(
        max_level: Level,
        context: LogContext,
        stamp: impl Fn() -> Stamp + Send + Sync + 'static,
        sink: impl Fn(String) + Send + Sync + 'static,
    ) -> Self {
        Self {
            stamp: Box::new(stamp),
            sink: Box::new(sink),
            context,
            max_level,
        }
    }
}

impl<S: Subscriber> Layer<S> for JsonLayer {
    fn on_event(&self, event: &Event<'_>, _ctx: Context<'_, S>) {
        let meta = event.metadata();
        if *meta.level() > self.max_level {
            return;
        }
        let mut c = Collect::default();
        event.record(&mut c);
        for (k, v) in self.context.snapshot() {
            if !c.fields.iter().any(|(n, _)| *n == k) {
                c.fields.push((k, Value::String(v)));
            }
        }
        let line = format_line(
            (self.stamp)(),
            meta.level().as_str(),
            meta.target(),
            &c.fields,
            &c.message,
        );
        (self.sink)(line);
    }
}

/// Lines waiting to be written, bounded: the oldest are dropped when full.
pub struct LineBuffer {
    lines: VecDeque<String>,
    max_lines: usize,
    dropped: u64,
}

impl LineBuffer {
    /// A buffer holding at most `max_lines` lines.
    pub fn new(max_lines: usize) -> Self {
        Self {
            lines: VecDeque::new(),
            max_lines: max_lines.max(1),
            dropped: 0,
        }
    }

    /// Add a line, dropping the oldest if the buffer is full.
    pub fn push(&mut self, line: String) {
        if self.lines.len() >= self.max_lines {
            self.lines.pop_front();
            self.dropped += 1;
        }
        self.lines.push_back(line);
    }

    /// Everything buffered as newline-terminated text, emptying the buffer.
    pub fn take_text(&mut self) -> String {
        let mut out = String::new();
        for l in self.lines.drain(..) {
            out.push_str(&l);
            out.push('\n');
        }
        out
    }

    /// Lines dropped because the buffer was full.
    pub fn dropped(&self) -> u64 {
        self.dropped
    }

    /// Whether nothing is buffered.
    pub fn is_empty(&self) -> bool {
        self.lines.is_empty()
    }
}

/// Size-rotated JSON-lines files in one directory: `<prefix>.jsonl`, then
/// `<prefix>.1.jsonl`, `<prefix>.2.jsonl`, ... each at most about `max_bytes`.
pub struct RotatingLog<S: Storage> {
    dir: S,
    prefix: String,
    max_bytes: u64,
    seq: u32,
    file: Option<S::File>,
    len: u64,
    /// When the open file was opened: it is closed and reopened every
    /// [`REOPEN_EVERY`], see [`RotatingLog::write_line`].
    opened: Option<std::time::Instant>,
}

/// How long the log file stays open before it is closed and reopened. Windows
/// reports an open file's size in directory listings (Explorer, `dir`) as of
/// when it was opened, so a running app's log looked empty for hours; closing
/// it brings the listing up to date.
const REOPEN_EVERY: std::time::Duration = std::time::Duration::from_secs(2);

impl<S: Storage> RotatingLog<S> {
    /// Log into `dir`; nothing is created until the first line.
    pub fn new(dir: S, prefix: impl Into<String>, max_bytes: u64) -> Self {
        Self {
            dir,
            prefix: prefix.into(),
            max_bytes: max_bytes.max(1),
            seq: 0,
            file: None,
            len: 0,
            opened: None,
        }
    }

    /// The name of the file currently written (or next to be).
    pub fn current_name(&self) -> String {
        if self.seq == 0 {
            format!("{}.jsonl", self.prefix)
        } else {
            format!("{}.{}.jsonl", self.prefix, self.seq)
        }
    }

    /// Append one line (a newline is added), starting a new file first if this
    /// one would pass the size limit.
    pub fn write_line(&mut self, line: &str) -> io::Result<()> {
        let bytes = line.len() as u64 + 1;
        if self.opened.is_some_and(|t| t.elapsed() >= REOPEN_EVERY)
            && let Some(mut f) = self.file.take()
        {
            f.sync()?;
        }
        if self.file.is_some() && self.len > 0 && self.len + bytes > self.max_bytes {
            if let Some(mut f) = self.file.take() {
                f.sync()?;
            }
            self.seq += 1;
            self.len = 0;
        }
        if self.file.is_none() {
            let name = self.current_name();
            let f = self.dir.open(&name)?;
            self.len = f.len()?;
            self.file = Some(f);
            self.opened = Some(std::time::Instant::now());
        }
        let f = self.file.as_mut().expect("opened above");
        let mut buf = Vec::with_capacity(line.len() + 1);
        buf.extend_from_slice(line.as_bytes());
        buf.push(b'\n');
        f.write_at(self.len, &buf)?;
        self.len += bytes;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_port_disk::SimDisk;
    use tracing_subscriber::layer::SubscriberExt;

    #[test]
    fn utc_time_is_fixed_width_and_correct() {
        assert_eq!(utc_iso(0), "1970-01-01T00:00:00.000Z");
        // 2024-02-29 (leap day) 23:59:58.007
        assert_eq!(utc_iso(1_709_251_198_007), "2024-02-29T23:59:58.007Z");
        assert_eq!(utc_compact(1_709_251_198_007), "20240229T235958Z");
    }

    #[test]
    fn line_has_the_shared_shape() {
        let line = format_line(
            Stamp {
                wall_ms: 1_709_251_198_007,
                mono_ms: 12.34,
            },
            "INFO",
            "castcorder_core::outbox",
            &[
                (CALL.into(), "c\"1".into()),
                (DEVICE.into(), "d1".into()),
                (TAKE.into(), 2.into()),
            ],
            "hello\nworld",
        );
        assert!(!line.contains('\n'), "one line: {line}");
        let v: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(v["ts"], "2024-02-29T23:59:58.007Z");
        assert_eq!(v["mono_ms"], 12.3);
        assert_eq!(v["level"], "INFO");
        assert_eq!(v["target"], "castcorder_core::outbox");
        assert_eq!(v["fields"]["call"], "c\"1");
        assert_eq!(v["fields"]["device"], "d1");
        assert_eq!(v["fields"]["take"], 2);
        assert_eq!(v["message"], "hello\nworld");
        assert!(line.is_ascii());
    }

    #[test]
    fn layer_writes_events_with_context_and_respects_the_level() {
        let lines = Arc::new(Mutex::new(Vec::<String>::new()));
        let sink = lines.clone();
        let ctx = LogContext::new();
        ctx.set(DEVICE, "dev-ctx");
        ctx.set(REALM, "test");
        let layer = JsonLayer::new(
            Level::INFO,
            ctx,
            || Stamp {
                wall_ms: 5,
                mono_ms: 1.0,
            },
            move |l| sink.lock().unwrap().push(l),
        );
        let subscriber = tracing_subscriber::registry().with(layer);
        tracing::subscriber::with_default(subscriber, || {
            tracing::info!(call = "c1", take = 3u32, track = %"t", "started");
            tracing::debug!("not logged");
        });
        let lines = lines.lock().unwrap();
        assert_eq!(lines.len(), 1);
        let v: Value = serde_json::from_str(&lines[0]).unwrap();
        assert_eq!(v["message"], "started");
        assert_eq!(v["fields"]["call"], "c1");
        assert_eq!(v["fields"]["take"], 3);
        assert_eq!(v["fields"]["track"], "t");
        assert_eq!(v["fields"]["device"], "dev-ctx");
        assert_eq!(v["fields"]["realm"], "test");
    }

    #[test]
    fn rotating_log_rotates_by_size() {
        let disk = SimDisk::new(1);
        let mut log = RotatingLog::new(disk.clone(), "host-x", 100);
        let line = "x".repeat(39); // 40 bytes with the newline
        for _ in 0..5 {
            log.write_line(&line).unwrap();
        }
        let mut names = disk.list().unwrap();
        names.sort();
        assert_eq!(names, ["host-x.1.jsonl", "host-x.2.jsonl", "host-x.jsonl"]);
        let mut f = disk.open("host-x.jsonl").unwrap();
        assert_eq!(f.len().unwrap(), 80);
        let mut buf = vec![0u8; 80];
        f.read_at(0, &mut buf).unwrap();
        assert_eq!(buf, format!("{line}\n{line}\n").into_bytes());
    }

    #[test]
    fn buffer_drops_the_oldest() {
        let mut b = LineBuffer::new(2);
        for l in ["a", "b", "c"] {
            b.push(l.into());
        }
        assert_eq!(b.dropped(), 1);
        assert_eq!(b.take_text(), "b\nc\n");
        assert!(b.is_empty());
    }
}
