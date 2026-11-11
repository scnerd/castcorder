//! capture-spike: command-line driver for the scenarios in scripts/run-all.sh.
//! All output is ASCII.

use anyhow::{Context, Result, anyhow, bail};
use capture_spike::audio::{self, AudioConfig, AudioPipeline, RecWriter, monotonic_ns};
use capture_spike::compare::compare_aligned;
use capture_spike::ports::{CaptureSource, PulseSource, TestToneSource, WavFileSource};
use capture_spike::{devices, video, wav};
use gst::prelude::*;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

struct Args {
    cmd: String,
    kv: HashMap<String, String>,
    rest: Vec<String>,
}

impl Args {
    fn parse() -> Args {
        let mut it = std::env::args().skip(1);
        let cmd = it.next().unwrap_or_else(|| "help".into());
        let mut kv = HashMap::new();
        let mut rest = Vec::new();
        let v: Vec<String> = it.collect();
        let mut i = 0;
        while i < v.len() {
            if v[i] == "--" {
                rest = v[i + 1..].to_vec();
                break;
            }
            if let Some(k) = v[i].strip_prefix("--") {
                let val = v.get(i + 1).cloned().unwrap_or_default();
                kv.insert(k.to_string(), val);
                i += 2;
            } else {
                i += 1;
            }
        }
        Args { cmd, kv, rest }
    }
    fn get(&self, k: &str) -> Result<&str> {
        self.kv
            .get(k)
            .map(|s| s.as_str())
            .ok_or_else(|| anyhow!("missing --{k}"))
    }
    fn opt(&self, k: &str, d: &str) -> String {
        self.kv.get(k).cloned().unwrap_or_else(|| d.to_string())
    }
    fn num(&self, k: &str, d: u64) -> Result<u64> {
        match self.kv.get(k) {
            Some(v) => v.parse().with_context(|| format!("--{k}")),
            None => Ok(d),
        }
    }
}

fn writer_from(a: &Args) -> Result<RecWriter> {
    Ok(match a.opt("writer", "app").as_str() {
        "wavenc" => RecWriter::WavEnc,
        "app" => RecWriter::App(wav::WriterOptions {
            header_interval: Duration::from_millis(a.num("header-ms", 1000)?),
            sync_on_header: a.opt("fsync", "no") == "yes",
            ..Default::default()
        }),
        other => bail!("unknown writer {other}"),
    })
}

fn source_from(a: &Args) -> Result<Box<dyn CaptureSource>> {
    let s = a.get("source")?;
    let caps = a.opt("caps", "audio/x-raw,format=S24LE,rate=48000,channels=2");
    Ok(if s == "tone" {
        Box::new(TestToneSource { caps })
    } else if let Some(dev) = s.strip_prefix("pulse:") {
        Box::new(PulseSource {
            device: dev.to_string(),
            caps: Some(caps),
        })
    } else if let Some(p) = s.strip_prefix("file:") {
        Box::new(WavFileSource { path: p.into() })
    } else {
        bail!("source must be tone | pulse:<device> | file:<path>")
    })
}

fn print_thread_info(st: &audio::Stats) {
    for l in st.rec_thread.lock().unwrap().iter() {
        println!("  rec-thread: {l}");
    }
    let e = st.rec_tid_enter.load(Ordering::SeqCst);
    let w = st.rec_tid_write.load(Ordering::SeqCst);
    println!(
        "  rec-thread: writer ran on tid {w}; rec_queue thread tid {e}; same thread: {}",
        e == w && e != 0
    );
    if e != 0 {
        if let Ok(s) = std::fs::read_to_string(format!("/proc/self/task/{e}/stat")) {
            // Fields after the comm: state ... priority(18) nice(19) ... policy(41)
            if let Some(idx) = s.rfind(')') {
                let f: Vec<&str> = s[idx + 2..].split(' ').collect();
                println!(
                    "  rec-thread: /proc stat priority={} nice={} rt_priority={} policy={}",
                    f.get(15).unwrap_or(&"?"),
                    f.get(16).unwrap_or(&"?"),
                    f.get(37).unwrap_or(&"?"),
                    f.get(38).unwrap_or(&"?")
                );
            }
        }
    }
}

fn cmd_probe() -> Result<()> {
    let (ma, mi, mc, na) = gst::version();
    println!("GStreamer runtime version: {ma}.{mi}.{mc}.{na}");
    for e in [
        "filesrc", "wavparse", "wavenc", "tee", "queue", "appsink", "audioconvert", "audioresample",
        "webrtcdsp", "webrtcechoprobe", "opusenc", "pulsesrc", "pulsesink", "pipewiresrc",
        "alsasrc", "v4l2src", "videotestsrc", "openh264enc", "x264enc", "vah264enc",
        "vaapih264enc", "mp4mux", "matroskamux", "isofmp4mux", "splitmuxsink", "avdec_h264",
        "h264parse",
    ] {
        let f = gst::ElementFactory::find(e);
        let lic = f
            .as_ref()
            .and_then(|f| f.plugin())
            .map(|p| p.license().to_string())
            .unwrap_or_default();
        println!(
            "  element {:<16} {}{}",
            e,
            if f.is_some() { "present" } else { "MISSING" },
            if lic.is_empty() { String::new() } else { format!(" (plugin license {lic})") }
        );
    }
    let vids: Vec<String> = std::fs::read_dir("/dev")
        .map(|d| {
            d.filter_map(|e| e.ok())
                .map(|e| e.file_name().to_string_lossy().to_string())
                .filter(|n| n.starts_with("video"))
                .collect()
        })
        .unwrap_or_default();
    println!("  /dev/video* nodes: {vids:?}");
    for class in ["Audio/Source", "Video/Source"] {
        match devices::list(class) {
            Ok(l) => {
                println!("  DeviceMonitor {class}: {} device(s)", l.len());
                for d in l {
                    println!("    {d}");
                }
            }
            Err(e) => println!("  DeviceMonitor {class}: error {e:#}"),
        }
    }
    Ok(())
}

fn cmd_gen_wav(a: &Args) -> Result<()> {
    let rate = a.num("rate", 48000)? as u32;
    let bits = a.num("bits", 24)? as u16;
    let frames = a.num("frames", rate as u64 * 3 + 37)? as usize;
    let out = PathBuf::from(a.get("out")?);
    let data = wav::generate_signal(rate, bits, frames);
    wav::write_test_wav(&out, rate, bits, &data)?;
    println!("generated {} ({} Hz, {} bit, stereo, {} frames)", out.display(), rate, bits, frames);
    Ok(())
}

/// File source -> pipeline -> WAV; assert the data chunk is byte-identical.
fn cmd_record_file(a: &Args) -> Result<()> {
    let input = PathBuf::from(a.get("in")?);
    let out = PathBuf::from(a.get("out")?);
    let mut cfg = AudioConfig::new(&out);
    cfg.writer = writer_from(a)?;
    cfg.live_stall_ms = a.num("stall-ms", 0)?;
    let src = WavFileSource { path: input.clone() };
    let t = Instant::now();
    let st = audio::run_file(&src, cfg)?;
    let (ii, idata) = wav::read_data(&input)?;
    let (oi, odata) = wav::read_data(&out)?;
    println!("  {} -> {} in {} ms", input.display(), out.display(), t.elapsed().as_millis());
    println!("  stats: {}", st.summary());
    println!("  src caps: {:?}", st.src_caps.lock().unwrap());
    println!("  rec caps: {:?}", st.rec_caps.lock().unwrap());
    print_thread_info(&st);
    let same = idata == odata;
    println!(
        "  input data {} bytes ({} Hz {} bit), output data {} bytes ({} Hz {} bit), byte-identical: {}",
        idata.len(), ii.rate, ii.bits, odata.len(), oi.rate, oi.bits, same
    );
    if !same {
        bail!("recorded data differs from input");
    }
    if st.opus_packets.load(Ordering::SeqCst) == 0 {
        bail!("no opus packets");
    }
    Ok(())
}

fn write_marker(out: &Path, extra: &str) -> Result<()> {
    let p = PathBuf::from(format!("{}.playing", out.display()));
    std::fs::write(p, format!("{} {}\n", monotonic_ns(), extra))?;
    Ok(())
}

fn read_marker(out: &Path) -> Option<u64> {
    let p = PathBuf::from(format!("{}.playing", out.display()));
    std::fs::read_to_string(p)
        .ok()?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

/// Live source for N seconds (or until killed).
fn cmd_record_live(a: &Args) -> Result<()> {
    let out = PathBuf::from(a.get("out")?);
    let secs = a.num("seconds", 5)?;
    let src = source_from(a)?;
    let mut cfg = AudioConfig::new(&out);
    cfg.writer = writer_from(a)?;
    let p = AudioPipeline::build(src.as_ref(), cfg)?;
    p.start()?;
    let t = Instant::now();
    while p.stats.rec_bytes.load(Ordering::SeqCst) == 0 {
        if t.elapsed() > Duration::from_secs(10) {
            bail!("no data from source within 10 s");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    write_marker(&out, "")?;
    std::thread::sleep(Duration::from_secs(secs));
    p.stop(Duration::from_secs(10))?;
    println!("  stats: {}", p.stats.summary());
    print_thread_info(&p.stats);
    Ok(())
}

fn cmd_kill_audio(a: &Args) -> Result<()> {
    let out = PathBuf::from(a.get("out")?);
    let after = a.num("after-ms", 3000)?;
    let _ = std::fs::remove_file(format!("{}.playing", out.display()));
    let exe = std::env::current_exe()?;
    let mut child_args = vec!["record-live".to_string()];
    child_args.extend(a.rest.iter().cloned());
    child_args.extend(["--out".into(), out.display().to_string(), "--seconds".into(), "3600".into()]);
    let mut child = std::process::Command::new(exe).args(&child_args).spawn()?;
    let t = Instant::now();
    let start_ns = loop {
        if let Some(n) = read_marker(&out) {
            break n;
        }
        if t.elapsed() > Duration::from_secs(20) {
            child.kill().ok();
            bail!("child never started recording");
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    std::thread::sleep(Duration::from_millis(after));
    let kill_ns = monotonic_ns();
    child.kill()?; // SIGKILL on Unix
    let status = child.wait()?;
    let elapsed_ms = (kill_ns - start_ns) as f64 / 1e6;
    let info = wav::read_info(&out)?;
    let bps = info.bytes_per_second() as f64;
    let expected = elapsed_ms / 1000.0 * bps;
    let on_disk = info.data_len_on_disk() as f64;
    println!("  child exit status: {status} (killed {elapsed_ms:.1} ms after first data)");
    println!(
        "  header: riff={} rate={} bits={} ch={} data_offset={}",
        if info.rf64 { "RF64" } else { "RIFF" }, info.rate, info.bits, info.channels, info.data_offset
    );
    println!(
        "  expected ~{:.0} bytes ({:.3} s); header claims {} bytes ({:.3} s); on disk {} bytes ({:.3} s)",
        expected,
        expected / bps,
        info.data_len_header,
        info.data_len_header as f64 / bps,
        on_disk,
        on_disk / bps
    );
    println!(
        "  readable without repair: {:.1}% of expected; physically present: {:.1}% (lost tail ~{:.1} ms)",
        100.0 * (info.data_len_header.min(info.data_len_on_disk()) as f64) / expected,
        100.0 * on_disk / expected,
        (expected - on_disk) / bps * 1000.0
    );
    let r = wav::recover(&out)?;
    let after_info = wav::read_info(&out)?;
    println!(
        "  recover(): frames per header before={} frames on disk={} partial bytes dropped={} repaired={} -> header now claims {:.3} s",
        r.frames_header_before,
        r.frames_on_disk,
        r.trailing_partial_bytes,
        r.repaired,
        after_info.data_len_header as f64 / bps
    );
    let tp = PathBuf::from(format!("{}.timing.jsonl", out.display()));
    if let Ok(t) = std::fs::read_to_string(&tp) {
        println!("  timing sidecar lines survived: {}", t.lines().count());
    }
    Ok(())
}

fn cmd_pulse_roundtrip(a: &Args) -> Result<()> {
    let input = PathBuf::from(a.get("in")?);
    let out = PathBuf::from(a.get("out")?);
    let device = a.get("device")?.to_string();
    let mode = a.get("mode")?.to_string();
    let (ii, idata) = wav::read_data(&input)?;
    let fmt = if ii.bits == 24 { "S24LE" } else { "S16LE" };
    let caps = a.opt(
        "caps",
        &format!("audio/x-raw,format={fmt},rate={},channels={}", ii.rate, ii.channels),
    );
    let src = PulseSource { device: device.clone(), caps: Some(caps.clone()) };
    let cfg = AudioConfig::new(&out);
    let p = AudioPipeline::build(&src, cfg)?;
    p.start()?;
    // module-pipe-source produces nothing until a writer appears, so the
    // feeder starts at once; alignment later finds the offset.
    let feeder = if mode == "pipe" {
        let fifo = a.get("fifo")?.to_string();
        let d = idata.clone();
        Some(std::thread::spawn(move || -> std::io::Result<()> {
            use std::io::Write;
            let mut f = std::fs::OpenOptions::new().write(true).open(fifo)?;
            for chunk in d.chunks(4096) {
                f.write_all(chunk)?;
            }
            Ok(())
        }))
    } else {
        None
    };
    let t = Instant::now();
    while p.stats.rec_bytes.load(Ordering::SeqCst) == 0 {
        if t.elapsed() > Duration::from_secs(10) {
            bail!("no data from pulsesrc within 10 s");
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    std::thread::sleep(Duration::from_millis(300));
    match mode.as_str() {
        "pipe" => {
            feeder.unwrap().join().map_err(|_| anyhow!("feeder panicked"))??;
        }
        "monitor" | "play" => {
            let sink = a.get("sink")?;
            let play = gst::parse::launch(&format!(
                "filesrc location=\"{}\" ! wavparse ! pulsesink device=\"{}\"",
                input.display(),
                sink
            ))?
            .downcast::<gst::Pipeline>()
            .unwrap();
            play.set_state(gst::State::Playing)?;
            video::wait(&play, Duration::from_secs(60))?;
            play.set_state(gst::State::Null)?;
        }
        _ => bail!("mode must be pipe|monitor"),
    }
    let dur_ms = idata.len() as u64 * 1000 / ii.bytes_per_second();
    // Let the tail drain through the server.
    std::thread::sleep(Duration::from_millis(dur_ms.min(1000) + 1000));
    p.stop(Duration::from_secs(10))?;
    let (oi, odata) = wav::read_data(&out)?;
    println!("  device={device} mode={mode} requested caps={caps}");
    println!("  src caps: {:?}", p.stats.src_caps.lock().unwrap());
    println!("  rec caps: {:?}", p.stats.rec_caps.lock().unwrap());
    println!("  stats: {}", p.stats.summary());
    print_thread_info(&p.stats);
    if oi.bits != ii.bits || oi.channels != ii.channels || oi.rate != ii.rate {
        println!(
            "  FORMAT MISMATCH: input {} Hz {} bit {} ch, captured {} Hz {} bit {} ch",
            ii.rate, ii.bits, ii.channels, oi.rate, oi.bits, oi.channels
        );
        println!("  RESULT bit_exact=false");
        return Ok(());
    }
    let c = compare_aligned(&idata, &odata, ii.bits, ii.channels as usize);
    println!("  compare: {}", c.summary());
    println!("  RESULT bit_exact={}", c.bit_exact());
    if a.opt("expect-exact", "no") == "yes" && !c.bit_exact() {
        bail!("expected bit-exact capture");
    }
    Ok(())
}

fn cmd_record_video(a: &Args) -> Result<()> {
    let out = PathBuf::from(a.get("out")?);
    let mux = video::Mux::parse(&a.opt("mux", "mkv"))?;
    let secs = a.num("seconds", 4)?;
    let source = a.opt(
        "source",
        "videotestsrc is-live=true pattern=snow ! video/x-raw,width=1280,height=720,framerate=30/1,format=I420",
    );
    let vp = video::build(&source, &out, mux, a.num("kbps", 8000)? as u32)?;
    vp.pipeline.set_state(gst::State::Playing)?;
    let t = Instant::now();
    while vp.stats.rec_frames.load(Ordering::SeqCst) == 0 {
        if t.elapsed() > Duration::from_secs(10) {
            bail!("no video frames within 10 s");
        }
        std::thread::sleep(Duration::from_millis(1));
    }
    write_marker(&out, vp.encoder)?;
    std::thread::sleep(Duration::from_secs(secs));
    vp.pipeline.send_event(gst::event::Eos::new());
    video::wait(&vp.pipeline, Duration::from_secs(10))?;
    vp.pipeline.set_state(gst::State::Null)?;
    println!(
        "  encoder={} rec_frames={} live_packets={} live_bytes={} live_queue_overruns={}",
        vp.encoder,
        vp.stats.rec_frames.load(Ordering::SeqCst),
        vp.stats.live_packets.load(Ordering::SeqCst),
        vp.stats.live_bytes.load(Ordering::SeqCst),
        vp.stats.live_dropped.load(Ordering::SeqCst)
    );
    let (n, err) = video::count_decoded_frames(&out);
    println!("  clean file: {} bytes, decoded frames {} err={:?}", std::fs::metadata(&out)?.len(), n, err);
    if n == 0 {
        bail!("clean recording decodes to 0 frames");
    }
    Ok(())
}

fn cmd_kill_video(a: &Args) -> Result<()> {
    let out = PathBuf::from(a.get("out")?);
    let after = a.num("after-ms", 3000)?;
    let mux = a.opt("mux", "mkv");
    let _ = std::fs::remove_file(format!("{}.playing", out.display()));
    let exe = std::env::current_exe()?;
    let mut child = std::process::Command::new(exe)
        .args(["record-video", "--mux", &mux, "--out", &out.display().to_string(), "--seconds", "3600"])
        .stdout(std::process::Stdio::null())
        .spawn()?;
    let t = Instant::now();
    let start_ns = loop {
        if let Some(n) = read_marker(&out) {
            break n;
        }
        if t.elapsed() > Duration::from_secs(20) {
            child.kill().ok();
            bail!("child never started");
        }
        std::thread::sleep(Duration::from_millis(2));
    };
    std::thread::sleep(Duration::from_millis(after));
    let kill_ns = monotonic_ns();
    child.kill()?;
    child.wait()?;
    let elapsed = (kill_ns - start_ns) as f64 / 1e9;
    let size = std::fs::metadata(&out).map(|m| m.len()).unwrap_or(0);
    let (n, err) = video::count_decoded_frames(&out);
    let expected = elapsed * 30.0;
    println!(
        "  mux={mux}: killed after {:.2} s (~{:.0} frames captured), file {} bytes, decodable frames {} ({:.1}%), decode error: {:?}",
        elapsed, expected, size, n, 100.0 * n as f64 / expected, err
    );
    println!("  RESULT mux={mux} survives_kill={}", n > 0);
    Ok(())
}

fn cmd_segments(a: &Args) -> Result<()> {
    let name = a.get("device")?.to_string();
    let dir = PathBuf::from(a.get("dir")?);
    std::fs::create_dir_all(&dir)?;
    let secs = a.num("seconds", 8)?;
    // Fault script (e.g. pactl unload/load) runs concurrently.
    let script = a.opt("during", "");
    let handle = if !script.is_empty() {
        Some(std::process::Command::new("sh").args(["-c", &script]).spawn()?)
    } else {
        None
    };
    let mut events = Vec::new();
    let r = devices::run_segments(&name, &dir, Duration::from_secs(secs), &mut events);
    if let Some(mut h) = handle {
        let _ = h.wait();
    }
    for e in &events {
        println!("  event: {e}");
    }
    let segs = r?;
    let mut ok = true;
    for s in &segs {
        let Ok(info) = wav::read_info(&s.path) else {
            println!("  segment {}: no file (no data before close), ended by: {}", s.path.display(), s.end_reason);
            continue;
        };
        let clean = info.data_len_header == info.data_len_on_disk();
        ok &= clean && info.data_len_header > 0;
        println!(
            "  segment {}: {:.3} s of audio, wall {}..{} ms, ended by: {}, header consistent with file: {}",
            s.path.display(),
            info.data_len_header as f64 / info.bytes_per_second() as f64,
            s.started_at_ms,
            s.ended_at_ms,
            s.end_reason,
            clean
        );
    }
    println!("  RESULT segments={} all_clean={}", segs.len(), ok);
    let want = a.num("expect-segments", 0)? as usize;
    if want > 0 && (segs.len() < want || !ok) {
        bail!("expected >= {want} clean segments");
    }
    Ok(())
}

fn main() {
    let a = Args::parse();
    if let Err(e) = capture_spike::init() {
        eprintln!("gst init failed: {e:#}");
        std::process::exit(2);
    }
    let r = match a.cmd.as_str() {
        "probe" => cmd_probe(),
        "gen-wav" => cmd_gen_wav(&a),
        "record-file" => cmd_record_file(&a),
        "record-live" => cmd_record_live(&a),
        "kill-audio" => cmd_kill_audio(&a),
        "pulse-roundtrip" => cmd_pulse_roundtrip(&a),
        "record-video" => cmd_record_video(&a),
        "kill-video" => cmd_kill_video(&a),
        "count-frames" => {
            let (n, e) = video::count_decoded_frames(Path::new(&a.opt("in", "")));
            println!("  frames={n} err={e:?}");
            Ok(())
        }
        "segments" => cmd_segments(&a),
        _ => {
            println!("commands: probe gen-wav record-file record-live kill-audio pulse-roundtrip record-video kill-video count-frames segments");
            Ok(())
        }
    };
    if let Err(e) = r {
        println!("  FAIL: {e:#}");
        std::process::exit(1);
    }
}
