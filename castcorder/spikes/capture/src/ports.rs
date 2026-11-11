//! The capture port seam (ADR 0003): core code asks a `CaptureSource` for a
//! GStreamer element with one always "src" pad. Real implementations wrap
//! device elements; synthetic ones wrap files or test generators. The
//! pipeline built on top (tee, recording branch, live branch) is identical
//! for both, so tests exercise the real media path with a fake device.

use anyhow::{Context, Result};
use gst::prelude::*;
use std::path::PathBuf;

pub trait CaptureSource: Send + Sync {
    /// Human-readable description for logs.
    fn describe(&self) -> String;
    /// Build a fresh element (usually a bin with a ghost "src" pad).
    fn build(&self) -> Result<gst::Element>;
    /// True when the source runs in real time (device or live generator).
    fn is_live(&self) -> bool;
}

fn bin(desc: &str) -> Result<gst::Element> {
    let b = gst::parse::bin_from_description(desc, true)
        .with_context(|| format!("parse source bin: {desc}"))?;
    Ok(b.upcast())
}

/// Synthetic: a WAV file played as fast as the pipeline accepts it.
pub struct WavFileSource {
    pub path: PathBuf,
}

impl CaptureSource for WavFileSource {
    fn describe(&self) -> String {
        format!("wav-file {}", self.path.display())
    }
    fn build(&self) -> Result<gst::Element> {
        bin(&format!(
            "filesrc location=\"{}\" ! wavparse name=src_parse",
            self.path.display()
        ))
    }
    fn is_live(&self) -> bool {
        false
    }
}

/// Synthetic: a live test tone in a fixed format.
pub struct TestToneSource {
    pub caps: String,
}

impl CaptureSource for TestToneSource {
    fn describe(&self) -> String {
        format!("audiotestsrc live {}", self.caps)
    }
    fn build(&self) -> Result<gst::Element> {
        bin(&format!(
            "audiotestsrc is-live=true wave=sine freq=440 ! capsfilter caps=\"{}\"",
            self.caps
        ))
    }
    fn is_live(&self) -> bool {
        true
    }
}

/// Real: a PulseAudio (or pipewire-pulse) source. `caps` pins the format to
/// the device's configured format so the tee cannot negotiate a conversion.
pub struct PulseSource {
    pub device: String,
    pub caps: Option<String>,
}

impl CaptureSource for PulseSource {
    fn describe(&self) -> String {
        format!("pulsesrc device={} caps={:?}", self.device, self.caps)
    }
    fn build(&self) -> Result<gst::Element> {
        let caps = self.caps.clone().unwrap_or_else(|| "audio/x-raw".into());
        bin(&format!(
            "pulsesrc name=dev_src device=\"{}\" provide-clock=true ! capsfilter caps=\"{}\"",
            self.device, caps
        ))
    }
    fn is_live(&self) -> bool {
        true
    }
}

/// Real: whatever element a GstDevice creates (DeviceMonitor result).
pub struct DeviceSource {
    pub device: gst::Device,
    pub caps: Option<gst::Caps>,
}

impl CaptureSource for DeviceSource {
    fn describe(&self) -> String {
        format!("device {}", self.device.display_name())
    }
    fn build(&self) -> Result<gst::Element> {
        let b = gst::Bin::new();
        let src = self.device.create_element(Some("dev_src"))?;
        let cf = gst::ElementFactory::make("capsfilter").build()?;
        if let Some(c) = &self.caps {
            cf.set_property("caps", c);
        }
        b.add_many([&src, &cf])?;
        src.link(&cf)?;
        let pad = cf.static_pad("src").unwrap();
        b.add_pad(&gst::GhostPad::with_target(&pad)?)?;
        Ok(b.upcast())
    }
    fn is_live(&self) -> bool {
        true
    }
}
