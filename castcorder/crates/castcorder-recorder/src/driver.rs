//! Pulling buffers from a capture source into a WAV track.

use std::io;

use castcorder_port_capture::CaptureSource;
use castcorder_port_disk::StorageFile;

use crate::wav::{WavInfo, WavWriter, WriterOptions};

/// Pull up to `max_buffers` buffers (all, if `None`) from `source` into `writer`.
/// Returns `true` if the source ended. Fails with `InvalidData` if the source's
/// format differs from the writer's or its buffers are not contiguous.
pub fn pump<S: CaptureSource, F: StorageFile>(
    source: &mut S,
    writer: &mut WavWriter<F>,
    max_buffers: Option<u64>,
) -> io::Result<bool> {
    if source.format() != writer.format() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "source format differs from the track format",
        ));
    }
    let mut pulled = 0u64;
    while max_buffers.is_none_or(|m| pulled < m) {
        let Some(buffer) = source.next_buffer()? else {
            return Ok(true);
        };
        if buffer.first_frame != writer.frames() {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!(
                    "capture gap: buffer starts at frame {} but the track has {}",
                    buffer.first_frame,
                    writer.frames()
                ),
            ));
        }
        writer.write(&buffer.data)?;
        pulled += 1;
    }
    Ok(false)
}

/// Record `source` to the end into a new track in `file`, then finalise it.
pub fn record<S: CaptureSource, F: StorageFile>(
    source: &mut S,
    file: F,
    opts: WriterOptions,
) -> io::Result<WavInfo> {
    let mut writer = WavWriter::create(file, source.format(), opts)?;
    pump(source, &mut writer, None)?;
    writer.finalize()
}
