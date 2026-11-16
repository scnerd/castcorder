//! The live branch of a teed capture and the recording thread's priority.
//!
//! A teed pipeline is `source ! tee`, then
//!
//! - recording: `queue name=rec_queue` (never leaky; holds up to 10 s so a slow
//!   disk is absorbed there, not pushed back into the tee) `! appsink`, pulled as
//!   the [`CaptureSource`](castcorder_port_capture::CaptureSource);
//! - live: `queue` (leaky downstream, 200 ms) `! audioconvert ! audioresample !
//!   S16LE 48 kHz mono ! webrtcdsp ! audioconvert ! volume ! opusenc ! appsink`, pulled
//!   with [`LiveOpus::next_packet`].
//!
//! A live consumer that stops pulling blocks only its own appsink; the leaky
//! queue then drops the oldest live audio and the recording never waits. The
//! recording queue's streaming thread raises its own priority as it starts
//! (STREAM_STATUS from a sync bus handler, as in the capture spike).

use std::io;
use std::sync::Arc;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::time::Duration;

use gst::prelude::*;

use crate::err;

/// Name of the recording queue element in a teed pipeline.
pub(crate) const REC_QUEUE: &str = "rec_queue";

/// How the live branch processes and encodes.
#[derive(Clone, Debug)]
pub struct LiveOptions {
    /// Opus bitrate in bits per second.
    pub bitrate: i32,
    /// Run webrtcdsp noise suppression (if the element exists).
    pub noise_suppression: bool,
    /// The name of a `webrtcechoprobe` that sees what the speakers play (see
    /// [`crate::Playback`]); enables echo cancellation. It must exist, in a
    /// running pipeline, before the capture opens.
    pub echo_probe: Option<String>,
    /// Start with the live stream muted (see [`LiveOpus::set_muted`]). The
    /// branch is built muted, so no audio reaches the encoder before a later
    /// `set_muted` could take effect.
    pub muted: bool,
}

impl Default for LiveOptions {
    fn default() -> Self {
        Self {
            bitrate: 32_000,
            noise_suppression: true,
            echo_probe: None,
            muted: false,
        }
    }
}

/// One encoded Opus frame from the live branch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct OpusPacket {
    /// The frame.
    pub data: Vec<u8>,
    /// Its presentation time in 48 kHz ticks on the capture's timeline. Grows
    /// by 960 per 20 ms frame; jumps where the leaky queue dropped audio.
    pub rtp_time: u64,
}

/// The live branch's output: Opus frames, pulled.
pub struct LiveOpus {
    sink: gst_app::AppSink,
    overruns: Arc<AtomicU64>,
    /// Ticks handed out last, for frames without a timestamp.
    last: u64,
    /// The `volume` before the encoder: muting it silences the live stream
    /// only (the recording branch is untouched).
    mute: Option<gst::Element>,
    /// Peak of the last buffer before any processing (absolute S16), and of
    /// the last buffer the encoder got.
    raw_peak: Arc<AtomicU32>,
    sent_peak: Arc<AtomicU32>,
}

impl LiveOpus {
    /// Mute (true) or unmute the live stream: the encoder gets silence while
    /// muted; the recording branch never changes. False if the branch has no
    /// volume element.
    pub fn set_muted(&self, muted: bool) -> bool {
        match &self.mute {
            Some(v) => {
                v.set_property("mute", muted);
                true
            }
            None => false,
        }
    }

    /// The microphone's level as it arrived (before echo cancellation, noise
    /// suppression and mute): the last buffer's peak, 0 to 255, linear.
    pub fn raw_level(&self) -> u8 {
        (self.raw_peak.load(Ordering::Relaxed) / 128).min(255) as u8
    }

    /// The last buffer's peak as the encoder got it (absolute S16): zero
    /// while muted.
    pub fn sent_peak(&self) -> u32 {
        self.sent_peak.load(Ordering::Relaxed)
    }

    /// The next Opus frame, waiting at most `timeout`. `None` on timeout or
    /// once the branch has ended (see [`LiveOpus::is_eos`]).
    pub fn next_packet(&mut self, timeout: Duration) -> Option<OpusPacket> {
        let sample = self
            .sink
            .try_pull_sample(gst::ClockTime::from_nseconds(timeout.as_nanos() as u64))?;
        let buffer = sample.buffer()?;
        let rtp_time = buffer
            .pts()
            .map(|t| t.nseconds() * 48 / 1_000_000)
            .unwrap_or(self.last + 960);
        self.last = rtp_time;
        let map = buffer.map_readable().ok()?;
        Some(OpusPacket {
            data: map.as_slice().to_vec(),
            rtp_time,
        })
    }

    /// Whether the live branch has ended (the source ended or was stopped).
    pub fn is_eos(&self) -> bool {
        self.sink.is_eos()
    }

    /// How many times the leaky live queue was full and dropped audio.
    pub fn overruns(&self) -> u64 {
        self.overruns.load(Ordering::Relaxed)
    }
}

/// Link `src ! tee` with the recording branch into `rec_sink` and the live
/// branch; returns the live output.
pub(crate) fn build_tee(
    pipeline: &gst::Pipeline,
    src: &gst::Element,
    rec_sink: &gst_app::AppSink,
    opts: &LiveOptions,
) -> io::Result<LiveOpus> {
    let make = |factory: &str, name: &str| {
        gst::ElementFactory::make(factory)
            .name(name)
            .build()
            .map_err(|e| err(format!("{factory}: {e}")))
    };
    let tee = make("tee", "tee")?;
    let rec_q = make("queue", REC_QUEUE)?;
    rec_q.set_property("max-size-buffers", 0u32);
    rec_q.set_property("max-size-bytes", 0u32);
    rec_q.set_property("max-size-time", 10_000_000_000u64);
    pipeline
        .add_many([&tee, &rec_q, rec_sink.upcast_ref()])
        .map_err(|e| err(e.to_string()))?;
    gst::Element::link_many([src, &tee, &rec_q, rec_sink.upcast_ref()])
        .map_err(|e| err(format!("link recording branch: {e}")))?;

    let have_dsp = gst::ElementFactory::find("webrtcdsp").is_some();
    let dsp = if !have_dsp {
        tracing::warn!(
            "webrtcdsp missing: live audio goes out without echo cancellation or noise suppression"
        );
        String::new()
    } else if !opts.noise_suppression && opts.echo_probe.is_none() {
        String::new()
    } else {
        tracing::info!(
            echo_cancel = opts.echo_probe.is_some(),
            noise_suppression = opts.noise_suppression,
            "live branch: webrtcdsp"
        );
        format!(
            "! webrtcdsp echo-cancel={} {} noise-suppression={} gain-control=false ",
            opts.echo_probe.is_some(),
            opts.echo_probe
                .as_ref()
                .map(|p| format!("probe={p}"))
                .unwrap_or_default(),
            opts.noise_suppression
        )
    };
    let desc = format!(
        "queue name=live_queue leaky=downstream max-size-buffers=0 max-size-bytes=0 \
         max-size-time=200000000 ! audioconvert ! audioresample \
         ! capsfilter name=live_caps caps=audio/x-raw,format=S16LE,rate=48000,channels=1 \
         {dsp}! audioconvert ! audio/x-raw,format=S16LE,rate=48000,channels=1 \
         ! volume name=live_mute mute={} ! opusenc name=live_enc bitrate={} frame-size=20 \
         ! appsink name=live_sink sync=false async=false max-buffers=1 drop=false",
        opts.muted, opts.bitrate
    );
    let bin = gst::parse::bin_from_description(&desc, true)
        .map_err(|e| err(format!("live branch: {e}")))?;
    pipeline.add(&bin).map_err(|e| err(e.to_string()))?;
    tee.link(&bin)
        .map_err(|e| err(format!("link live branch: {e}")))?;
    let overruns = Arc::new(AtomicU64::new(0));
    if let Some(q) = bin.by_name("live_queue") {
        let o = overruns.clone();
        q.connect("overrun", false, move |_| {
            o.fetch_add(1, Ordering::Relaxed);
            None
        });
    }
    let sink = bin
        .by_name("live_sink")
        .and_then(|e| e.downcast::<gst_app::AppSink>().ok())
        .ok_or_else(|| err("live branch has no appsink"))?;

    let raw_peak = Arc::new(AtomicU32::new(0));
    let sent_peak = Arc::new(AtomicU32::new(0));
    if let Some(pad) = bin.by_name("live_caps").and_then(|e| e.static_pad("src")) {
        peak_probe(&pad, raw_peak.clone());
    }
    if let Some(pad) = bin.by_name("live_enc").and_then(|e| e.static_pad("sink")) {
        peak_probe(&pad, sent_peak.clone());
    }
    raise_recording_thread(pipeline);
    Ok(LiveOpus {
        sink,
        overruns,
        last: 0,
        mute: bin.by_name("live_mute"),
        raw_peak,
        sent_peak,
    })
}

/// Keep the peak (absolute S16LE) of every buffer through `pad` in `slot`.
fn peak_probe(pad: &gst::Pad, slot: Arc<AtomicU32>) {
    pad.add_probe(gst::PadProbeType::BUFFER, move |_, info| {
        if let Some(b) = info.buffer()
            && let Ok(m) = b.map_readable()
        {
            let peak = m
                .as_chunks::<2>()
                .0
                .iter()
                .map(|c| u32::from(i16::from_le_bytes(*c).unsigned_abs()))
                .max()
                .unwrap_or(0);
            slot.store(peak, Ordering::Relaxed);
        }
        gst::PadProbeReturn::Ok
    });
}

/// Raise the recording queue's streaming thread as it starts. The message is
/// posted synchronously from that thread, so the handler runs on it.
fn raise_recording_thread(pipeline: &gst::Pipeline) {
    let Some(bus) = pipeline.bus() else {
        return;
    };
    bus.set_sync_handler(|_, msg| {
        if let gst::MessageView::StreamStatus(s) = msg.view() {
            let (kind, owner) = s.get();
            if kind == gst::StreamStatusType::Enter && owner.name() == REC_QUEUE {
                let outcome = raise_current_thread_priority();
                tracing::info!(thread = "rec_queue", outcome = %outcome, "recording thread priority");
            }
        }
        gst::BusSyncReply::Pass
    });
}

/// Raise the calling thread to the highest priority available: on Windows
/// `THREAD_PRIORITY_TIME_CRITICAL`; elsewhere `SCHED_FIFO` 50 (needs
/// `CAP_SYS_NICE` or an rtprio limit), else the highest normal priority.
/// Returns what happened, for the log.
pub fn raise_current_thread_priority() -> String {
    use thread_priority::*;
    #[cfg(windows)]
    {
        match set_current_thread_priority(ThreadPriority::Os(
            WinAPIThreadPriority::TimeCritical.into(),
        )) {
            Ok(()) => "time critical".into(),
            Err(e) => format!("unchanged ({e:?})"),
        }
    }
    #[cfg(unix)]
    {
        let fifo = 50u8.try_into().map(ThreadPriority::Crossplatform);
        let rt = fifo.map_err(|e| format!("{e:?}")).and_then(|p| {
            set_thread_priority_and_policy(
                thread_native_id(),
                p,
                ThreadSchedulePolicy::Realtime(RealtimeThreadSchedulePolicy::Fifo),
            )
            .map_err(|e| format!("{e:?}"))
        });
        match rt {
            Ok(()) => "SCHED_FIFO 50".into(),
            Err(e) => match set_current_thread_priority(ThreadPriority::Max) {
                Ok(()) => format!("highest normal priority (SCHED_FIFO refused: {e})"),
                Err(e2) => format!("unchanged ({e}; {e2:?})"),
            },
        }
    }
    #[cfg(not(any(windows, unix)))]
    {
        "unchanged (not supported on this platform)".into()
    }
}
