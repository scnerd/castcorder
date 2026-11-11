//! Device enumeration (GstDeviceMonitor) and Segment handling: when the
//! source disappears mid-capture, close the current file cleanly; when it
//! returns, continue the track in a new segment file.

use crate::audio::{AudioConfig, AudioPipeline};
use crate::ports::DeviceSource;
use anyhow::{Result, bail};
use gst::prelude::*;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

pub fn monitor(class: &str) -> Result<gst::DeviceMonitor> {
    let m = gst::DeviceMonitor::new();
    m.add_filter(Some(class), None);
    m.start()?;
    Ok(m)
}

pub fn list(class: &str) -> Result<Vec<String>> {
    let m = monitor(class)?;
    let mut out = Vec::new();
    for d in m.devices() {
        out.push(format!(
            "name=\"{}\" internal-name={:?} class={} caps={} props={}",
            d.display_name(),
            internal_name(&d),
            d.device_class(),
            d.caps().map(|c| c.to_string()).unwrap_or_default(),
            d.properties().map(|p| p.to_string()).unwrap_or_default()
        ));
    }
    m.stop();
    Ok(out)
}

/// The sound server's own name for the device (pulse: source name).
pub fn internal_name(d: &gst::Device) -> Option<String> {
    if d.find_property("internal-name").is_some() {
        d.property::<Option<String>>("internal-name")
    } else {
        None
    }
}

fn matches(d: &gst::Device, name: &str) -> bool {
    if d.display_name() == name || internal_name(d).as_deref() == Some(name) {
        return true;
    }
    d.properties()
        .map(|p| {
            p.iter().any(|(_, v)| {
                v.get::<String>().map(|s| s == name).unwrap_or(false)
            })
        })
        .unwrap_or(false)
}

#[derive(Debug)]
pub struct SegmentResult {
    pub path: PathBuf,
    pub bytes: u64,
    pub end_reason: String,
    pub started_at_ms: u128,
    pub ended_at_ms: u128,
}

/// Capture from the device matching `name` for `total`, opening a new
/// segment file each time the device (re)appears. `events` is a log of what
/// GStreamer told us and when.
pub fn run_segments(
    name: &str,
    dir: &Path,
    total: Duration,
    events: &mut Vec<String>,
) -> Result<Vec<SegmentResult>> {
    let mon = monitor("Audio/Source")?;
    let mbus = mon.bus();
    let t0 = Instant::now();
    let ms = |t0: Instant| t0.elapsed().as_millis();
    let mut segments = Vec::new();
    let mut current: Option<(AudioPipeline, PathBuf, u128)> = None;
    let mut last_bytes = 0u64;
    let mut last_progress = Instant::now();
    let mut removed_at: Option<Instant> = None;

    while t0.elapsed() < total {
        // Device monitor messages.
        while let Some(msg) = mbus.pop() {
            match msg.view() {
                gst::MessageView::DeviceAdded(a) => {
                    let d = a.device();
                    events.push(format!("{} ms: DeviceAdded \"{}\"", ms(t0), d.display_name()));
                }
                gst::MessageView::DeviceRemoved(r) => {
                    let d = r.device();
                    events.push(format!("{} ms: DeviceRemoved \"{}\"", ms(t0), d.display_name()));
                    if matches(&d, name) && current.is_some() {
                        removed_at = Some(Instant::now());
                    }
                }
                _ => {}
            }
        }
        match current.take() {
            None => {
                if let Some(d) = mon.devices().into_iter().find(|d| matches(d, name)) {
                    let idx = segments.len() + 1;
                    let path = dir.join(format!("segment-{idx:03}.wav"));
                    let src = DeviceSource { device: d, caps: None };
                    let mut cfg = AudioConfig::new(&path);
                    cfg.echo_cancel = false;
                    cfg.rt_priority = false;
                    let p = AudioPipeline::build(&src, cfg)?;
                    p.start()?;
                    events.push(format!("{} ms: segment {} started -> {}", ms(t0), idx, path.display()));
                    last_bytes = 0;
                    last_progress = Instant::now();
                    removed_at = None;
                    current = Some((p, path, ms(t0)));
                } else {
                    std::thread::sleep(Duration::from_millis(20));
                }
            }
            Some((p, path, started)) => {
                let mut end: Option<String> = None;
                while let Some(msg) = p.pipeline.bus().unwrap().pop_filtered(&[gst::MessageType::Error, gst::MessageType::Warning]) {
                    match msg.view() {
                        gst::MessageView::Error(e) => {
                            events.push(format!("{} ms: pipeline ERROR: {} ({:?})", ms(t0), e.error(), e.debug()));
                            end = Some(format!("pipeline error: {}", e.error()));
                        }
                        gst::MessageView::Warning(w) => {
                            events.push(format!("{} ms: pipeline WARNING: {}", ms(t0), w.error()));
                        }
                        _ => {}
                    }
                }
                let bytes = p.stats.rec_bytes.load(Ordering::SeqCst);
                if bytes != last_bytes {
                    last_bytes = bytes;
                    last_progress = Instant::now();
                }
                let limit = if last_bytes == 0 { Duration::from_millis(3000) } else { Duration::from_millis(500) };
                if end.is_none() && last_progress.elapsed() > limit {
                    events.push(format!("{} ms: watchdog: no data within watchdog limit", ms(t0)));
                    end = Some("watchdog: no data".into());
                }
                if end.is_none() {
                    if let Some(r) = removed_at {
                        // Give GStreamer 300 ms to report what it does with a
                        // vanished device, then close regardless (never keep
                        // recording a source we did not choose).
                        if r.elapsed() > Duration::from_millis(300) {
                            events.push(format!(
                                "{} ms: device removed; data still flowing after removal: {}",
                                ms(t0),
                                last_progress > r
                            ));
                            end = Some("device removed".into());
                        }
                    }
                }
                match end {
                    Some(reason) => {
                        let b = p.close_segment()?;
                        events.push(format!("{} ms: segment closed ({}), {} bytes", ms(t0), reason, b));
                        segments.push(SegmentResult {
                            path,
                            bytes: b,
                            end_reason: reason,
                            started_at_ms: started,
                            ended_at_ms: ms(t0),
                        });
                    }
                    None => {
                        current = Some((p, path, started));
                        std::thread::sleep(Duration::from_millis(10));
                    }
                }
            }
        }
    }
    if let Some((p, path, started)) = current.take() {
        p.stop(Duration::from_secs(5))?;
        let b = crate::wav::read_info(&path).map(|i| i.data_len_header).unwrap_or(0);
        events.push(format!("{} ms: segment closed (end of run, EOS), {} bytes", ms(t0), b));
        segments.push(SegmentResult {
            path,
            bytes: b,
            end_reason: "end of run".into(),
            started_at_ms: started,
            ended_at_ms: ms(t0),
        });
    }
    mon.stop();
    if segments.is_empty() {
        bail!("device {name} never appeared");
    }
    Ok(segments)
}
