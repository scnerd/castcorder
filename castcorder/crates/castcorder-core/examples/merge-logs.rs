//! Print every `*.jsonl` log file in a directory merged by wall-clock time.
//!
//! ```text
//! cargo run -p castcorder-core --example merge-logs -- <library>/<call>/logs
//! ```
//!
//! Lines are compact: `ts level [device realm] target: message key=value ...`.
//! Pass `--raw` to print the original JSON lines instead. The sort is stable
//! and by `ts` (fixed width, so string order is time order), then by the line's
//! own monotonic time, so one source's lines never reorder. The library's
//! `manifest.jsonl` and transfer leftovers are skipped. Output is ASCII.

use std::path::PathBuf;
use std::process::ExitCode;

use serde_json::Value;

fn main() -> ExitCode {
    let mut raw = false;
    let mut dir: Option<PathBuf> = None;
    for a in std::env::args().skip(1) {
        match a.as_str() {
            "--raw" => raw = true,
            _ => dir = Some(PathBuf::from(a)),
        }
    }
    let Some(dir) = dir else {
        eprintln!("usage: merge-logs [--raw] <library>/<call>/logs");
        return ExitCode::from(2);
    };
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) => {
            eprintln!("cannot read {}: {e}", dir.display());
            return ExitCode::from(1);
        }
    };
    // (ts, mono, file index, line number, json, raw text)
    let mut lines: Vec<(String, f64, usize, usize, Value, String)> = Vec::new();
    let mut names: Vec<String> = Vec::new();
    let mut bad = 0usize;
    let mut files: Vec<PathBuf> = entries.filter_map(|e| e.ok().map(|e| e.path())).collect();
    files.sort();
    for path in files {
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .to_string();
        if !name.ends_with(".jsonl") || name == "manifest.jsonl" {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            eprintln!("skipped unreadable {name}");
            continue;
        };
        let file = names.len();
        names.push(name);
        for (n, l) in text
            .lines()
            .enumerate()
            .filter(|(_, l)| !l.trim().is_empty())
        {
            match serde_json::from_str::<Value>(l) {
                Ok(v) if v["ts"].is_string() => {
                    let ts = v["ts"].as_str().unwrap_or_default().to_string();
                    let mono = v["mono_ms"].as_f64().unwrap_or(0.0);
                    lines.push((ts, mono, file, n, v, l.to_string()));
                }
                _ => bad += 1,
            }
        }
    }
    lines.sort_by(|a, b| {
        a.0.cmp(&b.0)
            .then(a.2.cmp(&b.2))
            .then(a.1.total_cmp(&b.1))
            .then(a.3.cmp(&b.3))
    });
    for (ts, _, _, _, v, text) in &lines {
        if raw {
            println!("{text}");
            continue;
        }
        let f = &v["fields"];
        let s = |k: &str| f[k].as_str().unwrap_or("-").to_string();
        let mut extra = String::new();
        if let Some(o) = f.as_object() {
            for (k, val) in o {
                if matches!(k.as_str(), "call" | "device" | "realm") {
                    continue;
                }
                let t = match val {
                    Value::String(x) => x.clone(),
                    other => other.to_string(),
                };
                extra.push_str(&format!(" {k}={t}"));
            }
        }
        println!(
            "{ts} {:5} [{} {}] {}: {}{extra}",
            v["level"].as_str().unwrap_or("?"),
            s("device"),
            s("realm"),
            v["target"].as_str().unwrap_or("?"),
            v["message"].as_str().unwrap_or(""),
        );
    }
    eprintln!(
        "merged {} lines from {} files ({} unparsable lines skipped)",
        lines.len(),
        names.len(),
        bad
    );
    ExitCode::SUCCESS
}
