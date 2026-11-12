//! Re-render a take's aligned tracks by hand:
//! `cargo run -p castcorder-align -- <take dir> [--offset-only] [--wav] [--min-drift-span-s N]`.

use castcorder_align::{AlignConfig, Mode, OutputFormat, align_take};
use castcorder_port_disk::FsStorage;

const USAGE: &str =
    "usage: castcorder-align <take dir> [--offset-only] [--wav] [--min-drift-span-s SECONDS]";

fn fail(msg: &str) -> ! {
    eprintln!("castcorder-align: {msg}");
    eprintln!("{USAGE}");
    std::process::exit(2);
}

fn main() {
    tracing_subscriber::fmt().with_ansi(false).init();
    let mut cfg = AlignConfig::default();
    let mut dir = None;
    let mut args = std::env::args().skip(1);
    while let Some(a) = args.next() {
        match a.as_str() {
            "--offset-only" => cfg.mode = Mode::OffsetOnly,
            "--wav" => cfg.format = OutputFormat::Wav,
            "--min-drift-span-s" => {
                let s: f64 = args
                    .next()
                    .and_then(|v| v.parse().ok())
                    .unwrap_or_else(|| fail("--min-drift-span-s needs a number"));
                cfg.min_drift_span_us = (s * 1e6) as i64;
            }
            "-h" | "--help" => {
                println!("{USAGE}");
                return;
            }
            other if other.starts_with("--") => fail(&format!("unknown argument {other}")),
            other => dir = Some(other.to_string()),
        }
    }
    let Some(dir) = dir else {
        fail("no take directory")
    };
    if !std::path::Path::new(&dir).is_dir() {
        fail(&format!("{dir} is not a directory"));
    }
    let storage = FsStorage::new(&dir).unwrap_or_else(|e| fail(&e.to_string()));
    match align_take(&storage, &cfg) {
        Ok(s) => {
            println!(
                "take {}: {} aligned tracks, {} frames at {} Hz, in {}",
                s.take,
                s.tracks.len(),
                s.frames,
                s.sample_rate,
                std::path::Path::new(&dir).join("aligned").display()
            );
            for t in &s.tracks {
                println!(
                    "  {}: offset {:.6} s, clock offset {:.0} us, drift {:.2} ppm, {} segments, {} gaps",
                    t.aligned,
                    t.offset_s,
                    t.clock.offset_us,
                    t.drift_ppm,
                    t.segments.len(),
                    t.gaps.len()
                );
            }
            for k in &s.skipped {
                println!("  skipped: {k}");
            }
        }
        Err(e) => {
            eprintln!("castcorder-align: {e}");
            std::process::exit(1);
        }
    }
}
