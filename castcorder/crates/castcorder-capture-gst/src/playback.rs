//! Playing the call on this machine: one Opus stream per remote participant,
//! decoded and mixed, through a `webrtcechoprobe` (the echo canceller's far-end
//! reference) to the default output.
//!
//! ```text
//! audiotestsrc (silence, live) --\
//! appsrc ! opusdec ! convert -----> audiomixer ! webrtcechoprobe ! autoaudiosink
//! appsrc ! opusdec ! convert ----/
//! ```
//!
//! The silent source keeps the mixer, and so the probe, running with nobody
//! talking. Each stream is played a fixed delay after its first frame arrived,
//! timed by its RTP clock (a minimal jitter buffer); a frame later than that
//! re-anchors the stream.

use std::collections::BTreeMap;
use std::io;
use std::time::Duration;

use gst::prelude::*;

use crate::{err, init};

/// How long after arrival a stream's first frame plays.
const PLAYOUT_DELAY: Duration = Duration::from_millis(80);
/// Caps every stream is converted to before the mixer.
const MIX_CAPS: &str = "audio/x-raw,format=S16LE,rate=48000,channels=2,layout=interleaved";

struct Stream {
    bin: gst::Bin,
    src: gst_app::AppSrc,
    /// (RTP ticks, running time) of the anchor frame.
    anchor: Option<(u64, gst::ClockTime)>,
}

/// The call's audio, played locally.
pub struct Playback {
    pipeline: gst::Pipeline,
    mixer: gst::Element,
    streams: BTreeMap<u64, Stream>,
}

impl Playback {
    /// Start playing silence to the default output. With `echo_probe`, the
    /// mix passes a `webrtcechoprobe` of that name (for
    /// [`crate::LiveOptions::echo_probe`]); it is running when this returns.
    pub fn new(echo_probe: Option<&str>) -> io::Result<Self> {
        init()?;
        let probe = echo_probe
            .map(|p| format!("! webrtcechoprobe name={p} "))
            .unwrap_or_default();
        let desc = format!(
            "audiotestsrc is-live=true wave=silence samplesperbuffer=480 ! {MIX_CAPS} \
             ! audiomixer name=mix ! {MIX_CAPS} {probe}! audioconvert ! audioresample \
             ! autoaudiosink"
        );
        let pipeline = gst::parse::launch(&desc)
            .map_err(|e| err(format!("playback pipeline: {e}")))?
            .downcast::<gst::Pipeline>()
            .map_err(|_| err("playback is not a pipeline"))?;
        let mixer = pipeline
            .by_name("mix")
            .ok_or_else(|| err("playback has no mixer"))?;
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| err(format!("cannot start playback: {e}")))?;
        let (res, state, _) = pipeline.state(gst::ClockTime::from_seconds(3));
        if res.is_err() || state != gst::State::Playing {
            let _ = pipeline.set_state(gst::State::Null);
            return Err(err(format!("playback did not start ({state:?})")));
        }
        tracing::info!(echo_probe = ?echo_probe, "local playback started");
        Ok(Self {
            pipeline,
            mixer,
            streams: BTreeMap::new(),
        })
    }

    fn add(&mut self, key: u64) -> io::Result<()> {
        let desc = format!(
            "appsrc name=src is-live=true format=time do-timestamp=false \
             caps=audio/x-opus,channel-mapping-family=0,channels=2,rate=48000 \
             ! opusdec plc=true ! audioconvert ! audioresample ! {MIX_CAPS}"
        );
        let bin = gst::parse::bin_from_description(&desc, true)
            .map_err(|e| err(format!("playback stream: {e}")))?;
        let src = bin
            .by_name("src")
            .and_then(|e| e.downcast::<gst_app::AppSrc>().ok())
            .ok_or_else(|| err("playback stream has no appsrc"))?;
        self.pipeline.add(&bin).map_err(|e| err(e.to_string()))?;
        bin.link(&self.mixer)
            .map_err(|e| err(format!("link stream to mixer: {e}")))?;
        bin.sync_state_with_parent()
            .map_err(|e| err(format!("start stream: {e}")))?;
        tracing::info!(stream = key, "playback stream added");
        self.streams.insert(
            key,
            Stream {
                bin,
                src,
                anchor: None,
            },
        );
        Ok(())
    }

    /// Play one Opus frame of stream `key` (added on its first frame).
    /// `rtp_time` is in 48 kHz ticks.
    pub fn push(&mut self, key: u64, rtp_time: u64, opus: &[u8]) {
        if !self.streams.contains_key(&key)
            && let Err(e) = self.add(key)
        {
            tracing::warn!(stream = key, error = %e, "playback stream not added");
            return;
        }
        let Some(now) = self.pipeline.current_running_time() else {
            return;
        };
        let Some(s) = self.streams.get_mut(&key) else {
            return;
        };
        let pts = s.anchor.and_then(|(rtp0, rt0)| {
            let ticks = rtp_time.checked_sub(rtp0)?;
            let pts = rt0 + gst::ClockTime::from_nseconds(ticks * 1_000_000_000 / 48_000);
            (pts >= now && pts <= now + gst::ClockTime::from_seconds(1)).then_some(pts)
        });
        let mut buffer = gst::Buffer::from_slice(opus.to_vec());
        let pts = match pts {
            Some(p) => p,
            None => {
                let p = now + gst::ClockTime::from_nseconds(PLAYOUT_DELAY.as_nanos() as u64);
                if s.anchor.is_some() {
                    tracing::debug!(stream = key, "playback stream re-anchored");
                }
                s.anchor = Some((rtp_time, p));
                buffer
                    .get_mut()
                    .expect("new buffer is writable")
                    .set_flags(gst::BufferFlags::DISCONT);
                p
            }
        };
        if let Some(b) = buffer.get_mut() {
            b.set_pts(pts);
        }
        if let Err(e) = s.src.push_buffer(buffer) {
            tracing::debug!(stream = key, error = ?e, "playback frame not pushed");
        }
    }

    /// Stop playing stream `key`.
    pub fn remove(&mut self, key: u64) {
        let Some(s) = self.streams.remove(&key) else {
            return;
        };
        let peer = s.bin.static_pad("src").and_then(|p| p.peer());
        let _ = s.bin.set_state(gst::State::Null);
        let _ = self.pipeline.remove(&s.bin);
        if let Some(p) = peer {
            self.mixer.release_request_pad(&p);
        }
        tracing::info!(stream = key, "playback stream removed");
    }
}

impl Drop for Playback {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }
}
