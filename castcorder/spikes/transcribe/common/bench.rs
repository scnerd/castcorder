// Shared bench driver: load wav, warm up, run whole clip and consecutive 5 s / 10 s chunks,
// print one JSON line with timings, texts and peak RAM. Included by each engine crate via #[path].
use std::time::Instant;

pub trait Engine {
    fn transcribe(&mut self, pcm: &[f32], chunk_secs: Option<f32>) -> String;
}

pub fn load_wav(path: &str) -> Vec<f32> {
    let mut r = hound::WavReader::open(path).expect("open wav");
    let spec = r.spec();
    assert_eq!(spec.sample_rate, 16000);
    assert_eq!(spec.channels, 1);
    r.samples::<i16>().map(|s| s.unwrap() as f32 / 32768.0).collect()
}

fn esc(s: &str) -> String {
    s.chars()
        .filter(|c| c.is_ascii() && !c.is_ascii_control())
        .collect::<String>()
        .replace('\\', "\\\\")
        .replace('"', "\\\"")
}

#[cfg(windows)]
pub fn peak_rss_mb() -> f64 {
    use windows_sys::Win32::System::ProcessStatus::{GetProcessMemoryInfo, PROCESS_MEMORY_COUNTERS};
    use windows_sys::Win32::System::Threading::GetCurrentProcess;
    unsafe {
        let mut c: PROCESS_MEMORY_COUNTERS = std::mem::zeroed();
        c.cb = std::mem::size_of::<PROCESS_MEMORY_COUNTERS>() as u32;
        GetProcessMemoryInfo(GetCurrentProcess(), &mut c, c.cb);
        c.PeakWorkingSetSize as f64 / 1048576.0
    }
}

fn median(v: &mut [f64]) -> f64 {
    v.sort_by(|a, b| a.partial_cmp(b).unwrap());
    v[v.len() / 2]
}

/// `load` builds the engine; its time is reported as load_s.
pub fn run<E: Engine>(label: &str, wav: &str, load: impl FnOnce() -> E) {
    let pcm = load_wav(wav);
    let dur = pcm.len() as f64 / 16000.0;
    let t = Instant::now();
    let mut e = load();
    let load_s = t.elapsed().as_secs_f64();
    // Warm-up on the first 5 s.
    let t = Instant::now();
    let _ = e.transcribe(&pcm[..16000 * 5], Some(5.0));
    let warm_s = t.elapsed().as_secs_f64();

    let t = Instant::now();
    let full = e.transcribe(&pcm, None);
    let full_s = t.elapsed().as_secs_f64();

    let mut out = format!(
        "{{\"label\":\"{}\",\"audio_s\":{:.2},\"load_s\":{:.3},\"first_s\":{:.3},\"full_s\":{:.3},\"rtf\":{:.4},\"full_text\":\"{}\"",
        esc(label), dur, load_s, warm_s, full_s, full_s / dur, esc(&full)
    );
    for secs in [5usize, 10] {
        let n = 16000 * secs;
        let mut lat = Vec::new();
        let mut text = String::new();
        for c in pcm.chunks(n) {
            if c.len() < 16000 {
                break;
            }
            let t = Instant::now();
            let s = e.transcribe(c, Some(c.len() as f32 / 16000.0));
            lat.push(t.elapsed().as_secs_f64());
            text.push_str(s.trim());
            text.push(' ');
        }
        let mx = lat.iter().cloned().fold(0.0, f64::max);
        out.push_str(&format!(
            ",\"c{s}_med_s\":{:.3},\"c{s}_max_s\":{:.3},\"c{s}_text\":\"{}\"",
            median(&mut lat), mx, esc(&text), s = secs
        ));
    }
    out.push_str(&format!(",\"peak_rss_mb\":{:.0}}}", peak_rss_mb()));
    println!("RESULT {out}");
}
