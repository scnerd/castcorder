//! Castcorder capture spike library. See FINDINGS.md.

pub mod audio;
pub mod compare;
pub mod devices;
pub mod ports;
pub mod video;
pub mod wav;

/// Initialise GStreamer once.
pub fn init() -> anyhow::Result<()> {
    gst::init()?;
    Ok(())
}
