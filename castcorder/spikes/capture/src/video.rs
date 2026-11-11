//! Video: one capture (v4l2src or videotestsrc), teed into a high-bitrate
//! H.264 recording muxed to a file and a low-bitrate live H.264 branch.

use anyhow::{Context, Result, bail};
use gst::prelude::*;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

#[derive(Debug, Clone, Copy)]
pub enum Mux {
    /// mp4mux, moov written at EOS only.
    Mp4,
    /// mp4mux with fragment-duration: moov up front, moof+mdat per fragment.
    Mp4Fragmented,
    /// matroskamux: clusters written as they complete.
    Matroska,
}

impl Mux {
    pub fn parse(s: &str) -> Result<Mux> {
        Ok(match s {
            "mp4" => Mux::Mp4,
            "mp4frag" => Mux::Mp4Fragmented,
            "mkv" => Mux::Matroska,
            _ => bail!("mux must be mp4|mp4frag|mkv"),
        })
    }
    fn desc(&self) -> &'static str {
        match self {
            Mux::Mp4 => "mp4mux",
            Mux::Mp4Fragmented => "mp4mux fragment-duration=1000",
            Mux::Matroska => "matroskamux",
        }
    }
}

/// Pick an H.264 encoder that exists here. Preference: non-GPL first.
pub fn pick_encoder() -> Result<&'static str> {
    for e in ["openh264enc", "vah264enc", "vaapih264enc", "x264enc"] {
        if gst::ElementFactory::find(e).is_some() {
            return Ok(e);
        }
    }
    bail!("no H.264 encoder available")
}

fn enc_desc(enc: &str, kbps: u32) -> String {
    match enc {
        "x264enc" => format!(
            "x264enc bitrate={kbps} speed-preset=ultrafast tune=zerolatency key-int-max=30"
        ),
        "openh264enc" => format!(
            "openh264enc bitrate={} gop-size=30 rate-control=bitrate complexity=low",
            kbps * 1000
        ),
        other => format!("{other} bitrate={kbps}"),
    }
}

pub struct VideoStats {
    pub rec_frames: AtomicU64,
    pub live_packets: AtomicU64,
    pub live_bytes: AtomicU64,
    pub live_dropped: AtomicU64,
}

pub struct VideoPipeline {
    pub pipeline: gst::Pipeline,
    pub stats: Arc<VideoStats>,
    pub encoder: &'static str,
}

pub fn build(source_desc: &str, out: &Path, mux: Mux, rec_kbps: u32) -> Result<VideoPipeline> {
    let encoder = pick_encoder()?;
    let desc = format!(
        "{source_desc} ! tee name=t \
         t. ! queue name=rec_queue max-size-buffers=0 max-size-bytes=0 max-size-time=5000000000 \
            ! videoconvert ! {rec_enc} ! h264parse ! {mux} ! filesink name=rec_sink location=\"{out}\" sync=false async=false \
         t. ! queue name=live_queue leaky=downstream max-size-buffers=3 max-size-bytes=0 max-size-time=0 \
            ! videoscale ! videoconvert ! video/x-raw,width=640,height=360 ! {live_enc} ! h264parse \
            ! appsink name=live_sink sync=false async=false max-buffers=1 drop=false",
        rec_enc = enc_desc(encoder, rec_kbps),
        live_enc = enc_desc(encoder, 500),
        mux = mux.desc(),
        out = out.display(),
    );
    let pipeline = gst::parse::launch(&desc)
        .with_context(|| format!("parse video pipeline: {desc}"))?
        .downcast::<gst::Pipeline>()
        .unwrap();
    let stats = Arc::new(VideoStats {
        rec_frames: AtomicU64::new(0),
        live_packets: AtomicU64::new(0),
        live_bytes: AtomicU64::new(0),
        live_dropped: AtomicU64::new(0),
    });
    let rq = pipeline.by_name("rec_queue").unwrap();
    let st = stats.clone();
    rq.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        st.rec_frames.fetch_add(1, Ordering::SeqCst);
        gst::PadProbeReturn::Ok
    });
    let lq = pipeline.by_name("live_queue").unwrap();
    let st = stats.clone();
    lq.connect("overrun", false, move |_| {
        st.live_dropped.fetch_add(1, Ordering::SeqCst);
        None
    });
    let sink = pipeline
        .by_name("live_sink")
        .unwrap()
        .downcast::<gst_app::AppSink>()
        .unwrap();
    let st = stats.clone();
    sink.set_callbacks(
        gst_app::AppSinkCallbacks::builder()
            .new_sample(move |s| {
                let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                if let Some(b) = sample.buffer() {
                    st.live_packets.fetch_add(1, Ordering::SeqCst);
                    st.live_bytes.fetch_add(b.size() as u64, Ordering::SeqCst);
                }
                Ok(gst::FlowSuccess::Ok)
            })
            .build(),
    );
    Ok(VideoPipeline {
        pipeline,
        stats,
        encoder,
    })
}

pub fn wait(pipeline: &gst::Pipeline, timeout: Duration) -> Result<()> {
    let bus = pipeline.bus().unwrap();
    match bus.timed_pop_filtered(
        gst::ClockTime::from_mseconds(timeout.as_millis() as u64),
        &[gst::MessageType::Eos, gst::MessageType::Error],
    ) {
        None => bail!("timeout"),
        Some(m) => match m.view() {
            gst::MessageView::Eos(_) => Ok(()),
            gst::MessageView::Error(e) => bail!("error: {} ({:?})", e.error(), e.debug()),
            _ => Ok(()),
        },
    }
}

/// Decode a recorded file and count video frames. Truncated files decode up
/// to the point the demuxer can follow; errors after that are reported.
pub fn count_decoded_frames(path: &Path) -> (u64, Option<String>) {
    let desc = format!(
        "filesrc location=\"{}\" ! decodebin ! video/x-raw ! fakesink name=f sync=false",
        path.display()
    );
    let pipeline = match gst::parse::launch(&desc) {
        Ok(p) => p.downcast::<gst::Pipeline>().unwrap(),
        Err(e) => return (0, Some(e.to_string())),
    };
    let n = Arc::new(AtomicU64::new(0));
    let sink = pipeline.by_name("f").unwrap();
    let n2 = n.clone();
    sink.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
        n2.fetch_add(1, Ordering::SeqCst);
        gst::PadProbeReturn::Ok
    });
    if pipeline.set_state(gst::State::Playing).is_err() {
        let _ = pipeline.set_state(gst::State::Null);
        return (0, Some("could not start decode pipeline".into()));
    }
    let r = wait(&pipeline, Duration::from_secs(60));
    let _ = pipeline.set_state(gst::State::Null);
    (n.load(Ordering::SeqCst), r.err().map(|e| e.to_string()))
}
