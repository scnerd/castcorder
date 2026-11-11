//! The audio pipeline: one capture, teed into
//!   (a) recording: queue (never leaky) -> appsink -> our WAV writer
//!       (or, as a baseline, queue -> wavenc -> filesink)
//!   (b) live: leaky queue -> audioconvert -> audioresample -> webrtcdsp
//!       (AEC + NS, with a far-end webrtcechoprobe) -> opusenc -> appsink
//!
//! The recording branch has no converter: whatever caps the source produces
//! are what the writer receives. The live branch starts with
//! audioconvert/audioresample so its constraints never reach the tee.

use crate::ports::CaptureSource;
use crate::wav::{WavWriter, WriterOptions};
use anyhow::{Context, Result, anyhow, bail};
use gst::prelude::*;
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

#[derive(Debug, Clone)]
pub enum RecWriter {
    /// queue -> appsink -> crate::wav::WavWriter
    App(WriterOptions),
    /// queue -> wavenc -> filesink (GStreamer's own writer, baseline)
    WavEnc,
}

#[derive(Debug, Clone)]
pub struct AudioConfig {
    pub out: PathBuf,
    pub writer: RecWriter,
    /// Sleep this long per live-branch buffer to simulate a stalled consumer.
    pub live_stall_ms: u64,
    /// Try to raise the recording thread to SCHED_FIFO.
    pub rt_priority: bool,
    /// Write a timing sidecar (<out>.timing.jsonl) once per second of audio.
    pub timing_sidecar: bool,
    /// Enable echo cancellation in the live branch (needs the far-end probe).
    pub echo_cancel: bool,
}

impl AudioConfig {
    pub fn new(out: &Path) -> Self {
        AudioConfig {
            out: out.to_path_buf(),
            writer: RecWriter::App(WriterOptions::default()),
            live_stall_ms: 0,
            rt_priority: true,
            timing_sidecar: true,
            echo_cancel: true,
        }
    }
}

#[derive(Default, Debug)]
pub struct Stats {
    pub rec_buffers: AtomicU64,
    pub rec_bytes: AtomicU64,
    pub rec_discont: AtomicU64,
    pub live_in_buffers: AtomicU64,
    pub live_out_buffers: AtomicU64,
    pub live_overruns: AtomicU64,
    pub rec_overruns: AtomicU64,
    pub opus_packets: AtomicU64,
    pub opus_bytes: AtomicU64,
    pub rec_eos: AtomicBool,
    pub dsp: Mutex<String>,
    pub src_caps: Mutex<Option<String>>,
    pub rec_caps: Mutex<Option<String>>,
    /// Outcome of the thread-priority request for the recording thread.
    pub rec_thread: Mutex<Vec<String>>,
    /// Kernel thread id of the recording queue's streaming thread.
    pub rec_tid_enter: AtomicU64,
    /// Kernel thread id that actually ran the writer callback.
    pub rec_tid_write: AtomicU64,
}

impl Stats {
    pub fn summary(&self) -> String {
        format!(
            "[{}] rec_buffers={} rec_bytes={} rec_discont={} rec_overruns={} live_in={} live_out={} live_dropped={} live_overruns={} opus_packets={} opus_bytes={}",
            self.dsp.lock().unwrap(),
            self.rec_buffers.load(Ordering::SeqCst),
            self.rec_bytes.load(Ordering::SeqCst),
            self.rec_discont.load(Ordering::SeqCst),
            self.rec_overruns.load(Ordering::SeqCst),
            self.live_in_buffers.load(Ordering::SeqCst),
            self.live_out_buffers.load(Ordering::SeqCst),
            self.live_dropped(),
            self.live_overruns.load(Ordering::SeqCst),
            self.opus_packets.load(Ordering::SeqCst),
            self.opus_bytes.load(Ordering::SeqCst),
        )
    }
    pub fn live_dropped(&self) -> u64 {
        self.live_in_buffers
            .load(Ordering::SeqCst)
            .saturating_sub(self.live_out_buffers.load(Ordering::SeqCst))
    }
}

pub fn gettid() -> u64 {
    #[cfg(target_os = "linux")]
    unsafe {
        libc::syscall(libc::SYS_gettid) as u64
    }
    #[cfg(not(target_os = "linux"))]
    0
}

pub fn monotonic_ns() -> u64 {
    let mut ts = libc::timespec {
        tv_sec: 0,
        tv_nsec: 0,
    };
    unsafe { libc::clock_gettime(libc::CLOCK_MONOTONIC, &mut ts) };
    ts.tv_sec as u64 * 1_000_000_000 + ts.tv_nsec as u64
}

/// Called on the recording queue's new streaming thread (STREAM_STATUS
/// ENTER is posted synchronously from inside that thread).
fn raise_current_thread_priority(log: &mut Vec<String>) {
    #[cfg(target_os = "linux")]
    unsafe {
        let name = std::ffi::CString::new("cc-rec").unwrap();
        libc::prctl(libc::PR_SET_NAME, name.as_ptr(), 0, 0, 0);
        let param = libc::sched_param { sched_priority: 50 };
        let rc = libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param);
        if rc == 0 {
            log.push("SCHED_FIFO prio 50: ok".into());
            return;
        }
        log.push(format!(
            "SCHED_FIFO prio 50: failed errno {} ({})",
            rc,
            std::io::Error::from_raw_os_error(rc)
        ));
        let tid = libc::syscall(libc::SYS_gettid) as libc::id_t;
        let rc = libc::setpriority(libc::PRIO_PROCESS, tid, -10);
        if rc == 0 {
            log.push("nice -10: ok".into());
        } else {
            log.push(format!("nice -10: failed ({})", std::io::Error::last_os_error()));
        }
    }
    #[cfg(not(target_os = "linux"))]
    log.push("priority change not implemented on this OS".into());
}

pub struct AudioPipeline {
    pub pipeline: gst::Pipeline,
    pub stats: Arc<Stats>,
    writer: Arc<Mutex<Option<WavWriter>>>,
    cfg: AudioConfig,
    live: bool,
}

fn caps_bits(info: &gst_audio::AudioInfo) -> (u16, bool) {
    let fi = info.format_info();
    (fi.width() as u16, fi.is_float())
}

impl AudioPipeline {
    pub fn build(source: &dyn CaptureSource, cfg: AudioConfig) -> Result<AudioPipeline> {
        let live = source.is_live();
        let pipeline = gst::Pipeline::with_name("capture");
        let stats = Arc::new(Stats::default());
        let src = source.build()?;
        src.set_property("name", "source");
        let tee = gst::ElementFactory::make("tee").name("tee").build()?;
        pipeline.add_many([&src, &tee])?;
        src.link(&tee)?;

        // ---- recording branch -------------------------------------------
        // Not leaky; generous limits so a slow disk is absorbed here instead
        // of back-pressuring the tee (and with it the live branch).
        let rec_q = gst::ElementFactory::make("queue")
            .name("rec_queue")
            .property("max-size-buffers", 0u32)
            .property("max-size-bytes", 0u32)
            .property("max-size-time", 10_000_000_000u64)
            .build()?;
        pipeline.add(&rec_q)?;
        tee.link(&rec_q)?;
        let writer: Arc<Mutex<Option<WavWriter>>> = Arc::new(Mutex::new(None));
        match &cfg.writer {
            RecWriter::WavEnc => {
                let enc = gst::ElementFactory::make("wavenc").name("wavenc").build()?;
                let fs = gst::ElementFactory::make("filesink")
                    .name("rec_sink")
                    .property("location", cfg.out.to_str().unwrap())
                    .property("sync", false)
                    .property("async", false)
                    .build()?;
                pipeline.add_many([&enc, &fs])?;
                gst::Element::link_many([&rec_q, &enc, &fs])?;
                let st = stats.clone();
                enc.static_pad("sink").unwrap().add_probe(
                    gst::PadProbeType::BUFFER,
                    move |pad, info| {
                        if let Some(gst::PadProbeData::Buffer(b)) = &info.data {
                            st.rec_buffers.fetch_add(1, Ordering::SeqCst);
                            st.rec_bytes.fetch_add(b.size() as u64, Ordering::SeqCst);
                            st.rec_tid_write.store(gettid(), Ordering::SeqCst);
                            let mut c = st.rec_caps.lock().unwrap();
                            if c.is_none() {
                                *c = pad.current_caps().map(|c| c.to_string());
                            }
                        }
                        gst::PadProbeReturn::Ok
                    },
                );
            }
            RecWriter::App(opts) => {
                let sink = gst_app::AppSink::builder()
                    .name("rec_sink")
                    .sync(false)
                    .max_buffers(0)
                    .build();
                sink.set_property("async", false);
                pipeline.add(&sink)?;
                rec_q.link(&sink)?;
                let st = stats.clone();
                let st_eos = stats.clone();
                let w = writer.clone();
                let w_eos = writer.clone();
                let out = cfg.out.clone();
                let opts = opts.clone();
                let timing = cfg.timing_sidecar;
                let mut timing_file = None;
                let mut frames_total: u64 = 0;
                let mut next_mark: u64 = 0;
                let mut rate: u64 = 0;
                let mut block_align: u64 = 1;
                sink.set_callbacks(
                    gst_app::AppSinkCallbacks::builder()
                        .new_sample(move |appsink| {
                            let sample = appsink.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                            let buffer = sample.buffer().ok_or(gst::FlowError::Error)?;
                            st.rec_tid_write.store(gettid(), Ordering::SeqCst);
                            if buffer.flags().contains(gst::BufferFlags::DISCONT)
                                && st.rec_buffers.load(Ordering::SeqCst) > 0
                            {
                                st.rec_discont.fetch_add(1, Ordering::SeqCst);
                            }
                            let mut guard = w.lock().unwrap();
                            if guard.is_none() {
                                let caps = sample.caps().ok_or(gst::FlowError::NotNegotiated)?;
                                *st.rec_caps.lock().unwrap() = Some(caps.to_string());
                                let info = gst_audio::AudioInfo::from_caps(caps)
                                    .map_err(|_| gst::FlowError::NotNegotiated)?;
                                let (bits, float) = caps_bits(&info);
                                rate = info.rate() as u64;
                                block_align = info.bpf() as u64;
                                let ww = WavWriter::create(
                                    &out,
                                    info.channels() as u16,
                                    info.rate(),
                                    bits,
                                    float,
                                    opts.clone(),
                                )
                                .map_err(|e| {
                                    eprintln!("writer create failed: {e:#}");
                                    gst::FlowError::Error
                                })?;
                                *guard = Some(ww);
                                if timing {
                                    let tp = PathBuf::from(format!("{}.timing.jsonl", out.display()));
                                    timing_file = OpenOptions::new()
                                        .create(true)
                                        .truncate(true)
                                        .write(true)
                                        .open(tp)
                                        .ok();
                                }
                            }
                            let map = buffer.map_readable().map_err(|_| gst::FlowError::Error)?;
                            if let Some(ww) = guard.as_mut() {
                                ww.write(map.as_slice()).map_err(|e| {
                                    eprintln!("write failed: {e:#}");
                                    gst::FlowError::Error
                                })?;
                            } else {
                                // Segment already closed (device lost): drop.
                                return Err(gst::FlowError::Eos);
                            }
                            drop(guard);
                            // Timing metadata: one point per second of audio.
                            if frames_total >= next_mark {
                                if let Some(f) = timing_file.as_mut() {
                                    let base = appsink.base_time().map(|t| t.nseconds());
                                    let seg = sample.segment().and_then(|s| {
                                        s.downcast_ref::<gst::ClockTime>().cloned()
                                    });
                                    let running = match (seg, buffer.pts()) {
                                        (Some(s), Some(p)) => s.to_running_time(p).map(|t| t.nseconds()),
                                        _ => None,
                                    };
                                    let clock_now = appsink.clock().map(|c| c.time().nseconds());
                                    let line = format!(
                                        "{{\"sample_index\":{},\"pts_ns\":{},\"running_ns\":{},\"base_time_ns\":{},\"clock_ns\":{},\"clock_now_ns\":{},\"monotonic_now_ns\":{},\"buffer_offset\":{}}}\n",
                                        frames_total,
                                        opt(buffer.pts().map(|t| t.nseconds())),
                                        opt(running),
                                        opt(base),
                                        opt(match (base, running) { (Some(b), Some(r)) => Some(b + r), _ => None }),
                                        opt(clock_now),
                                        monotonic_ns(),
                                        if buffer.offset() == u64::MAX { "null".to_string() } else { buffer.offset().to_string() },
                                    );
                                    let _ = f.write_all(line.as_bytes());
                                }
                                next_mark += rate.max(1);
                            }
                            frames_total += map.size() as u64 / block_align;
                            st.rec_buffers.fetch_add(1, Ordering::SeqCst);
                            st.rec_bytes.fetch_add(map.size() as u64, Ordering::SeqCst);
                            Ok(gst::FlowSuccess::Ok)
                        })
                        .eos(move |_| {
                            if let Some(ww) = w_eos.lock().unwrap().take() {
                                if let Err(e) = ww.finalize() {
                                    eprintln!("finalize failed: {e:#}");
                                }
                            }
                            st_eos.rec_eos.store(true, Ordering::SeqCst);
                        })
                        .build(),
                );
            }
        }
        {
            let st = stats.clone();
            rec_q.connect("overrun", false, move |_| {
                st.rec_overruns.fetch_add(1, Ordering::SeqCst);
                None
            });
        }

        // ---- live branch --------------------------------------------------
        let have_dsp = gst::ElementFactory::find("webrtcdsp").is_some();
        let echo_cancel = cfg.echo_cancel && have_dsp;
        let dsp = if !have_dsp {
            *stats.dsp.lock().unwrap() = "webrtcdsp MISSING: live branch runs without AEC/NS".into();
            String::new()
        } else {
            *stats.dsp.lock().unwrap() = format!(
                "webrtcdsp: echo-cancel={} (far-end probe), noise-suppression=true",
                echo_cancel
            );
            format!(
                "! webrtcdsp name=dsp echo-cancel={} {} noise-suppression=true gain-control=false",
                echo_cancel,
                if echo_cancel { "probe=far_probe" } else { "" }
            )
        };
        let live_desc = format!(
            "queue name=live_queue leaky=downstream max-size-buffers=0 max-size-bytes=0 max-size-time=200000000 \
             ! audioconvert ! audioresample ! audio/x-raw,format=S16LE,rate=48000,channels=1 \
             {dsp} ! audioconvert ! opusenc bitrate=32000 frame-size=20 \
             ! appsink name=live_sink sync=false async=false max-buffers=1 drop=false"
        );
        let live_bin = gst::parse::bin_from_description(&live_desc, true)
            .context("live branch")?;
        live_bin.set_property("name", "live_branch");
        pipeline.add(&live_bin)?;
        tee.link(&live_bin)?;
        let live_q = live_bin.by_name("live_queue").unwrap();
        {
            let st = stats.clone();
            live_q.connect("overrun", false, move |_| {
                st.live_overruns.fetch_add(1, Ordering::SeqCst);
                None
            });
            let st = stats.clone();
            live_q.static_pad("sink").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                st.live_in_buffers.fetch_add(1, Ordering::SeqCst);
                gst::PadProbeReturn::Ok
            });
            let st = stats.clone();
            live_q.static_pad("src").unwrap().add_probe(gst::PadProbeType::BUFFER, move |_, _| {
                st.live_out_buffers.fetch_add(1, Ordering::SeqCst);
                gst::PadProbeReturn::Ok
            });
        }
        let live_sink = live_bin
            .by_name("live_sink")
            .unwrap()
            .downcast::<gst_app::AppSink>()
            .map_err(|_| anyhow!("live_sink not appsink"))?;
        {
            let st = stats.clone();
            let stall = cfg.live_stall_ms;
            live_sink.set_callbacks(
                gst_app::AppSinkCallbacks::builder()
                    .new_sample(move |s| {
                        let sample = s.pull_sample().map_err(|_| gst::FlowError::Eos)?;
                        if let Some(b) = sample.buffer() {
                            st.opus_packets.fetch_add(1, Ordering::SeqCst);
                            st.opus_bytes.fetch_add(b.size() as u64, Ordering::SeqCst);
                        }
                        if stall > 0 {
                            std::thread::sleep(Duration::from_millis(stall));
                        }
                        Ok(gst::FlowSuccess::Ok)
                    })
                    .build(),
            );
        }

        // ---- far end (what the call plays out; the AEC reference) ---------
        if echo_cancel {
            let far_desc = if live {
                "audiotestsrc name=far_src is-live=true wave=sine freq=300 volume=0.1 samplesperbuffer=480 \
                 ! audio/x-raw,format=S16LE,rate=48000,channels=1 ! webrtcechoprobe name=far_probe \
                 ! fakesink sync=true async=false"
                    .to_string()
            } else {
                // Non-live run: bound the far end so the pipeline reaches EOS.
                "audiotestsrc name=far_src wave=sine freq=300 volume=0.1 samplesperbuffer=480 num-buffers=100 \
                 ! audio/x-raw,format=S16LE,rate=48000,channels=1 ! webrtcechoprobe name=far_probe \
                 ! fakesink sync=false async=false"
                    .to_string()
            };
            let far = gst::parse::bin_from_description(&far_desc, false).context("far end")?;
            far.set_property("name", "far_end");
            pipeline.add(&far)?;
        }

        // Record the caps leaving the source, to compare with what the
        // recording sink got.
        {
            let st = stats.clone();
            src.static_pad("src").unwrap().add_probe(gst::PadProbeType::EVENT_DOWNSTREAM, move |_, info| {
                if let Some(gst::PadProbeData::Event(ev)) = &info.data {
                    if let gst::EventView::Caps(c) = ev.view() {
                        *st.src_caps.lock().unwrap() = Some(c.caps().to_string());
                    }
                }
                gst::PadProbeReturn::Ok
            });
        }

        // Thread priority: STREAM_STATUS ENTER for rec_queue's src task is
        // posted synchronously from the new thread itself.
        {
            let st = stats.clone();
            let want_rt = cfg.rt_priority;
            pipeline.bus().unwrap().set_sync_handler(move |_, msg| {
                if let gst::MessageView::StreamStatus(s) = msg.view() {
                    let (kind, owner) = s.get();
                    if kind == gst::StreamStatusType::Enter && owner.name() == "rec_queue" {
                        st.rec_tid_enter.store(gettid(), Ordering::SeqCst);
                        let mut log = st.rec_thread.lock().unwrap();
                        log.push(format!("rec_queue streaming thread entered (tid {})", gettid()));
                        if want_rt {
                            raise_current_thread_priority(&mut log);
                        }
                    }
                }
                gst::BusSyncReply::Pass
            });
        }

        Ok(AudioPipeline {
            pipeline,
            stats,
            writer,
            cfg,
            live,
        })
    }

    pub fn start(&self) -> Result<()> {
        self.pipeline
            .set_state(gst::State::Playing)
            .context("set PLAYING")?;
        Ok(())
    }

    /// Wait for EOS (or error). For non-live sources the stream ends by itself.
    pub fn wait_eos(&self, timeout: Duration) -> Result<()> {
        let bus = self.pipeline.bus().unwrap();
        let deadline = std::time::Instant::now() + timeout;
        loop {
            let left = deadline.saturating_duration_since(std::time::Instant::now());
            if left.is_zero() {
                bail!("timeout waiting for EOS");
            }
            let Some(msg) = bus.timed_pop_filtered(
                gst::ClockTime::from_mseconds(left.as_millis() as u64),
                &[gst::MessageType::Eos, gst::MessageType::Error],
            ) else {
                continue;
            };
            match msg.view() {
                gst::MessageView::Eos(_) => return Ok(()),
                gst::MessageView::Error(e) => {
                    bail!(
                        "pipeline error from {:?}: {} ({:?})",
                        e.src().map(|s| s.path_string()),
                        e.error(),
                        e.debug()
                    )
                }
                _ => {}
            }
        }
    }

    /// Ask a live pipeline to finish: EOS into the sources, then wait.
    pub fn stop(&self, timeout: Duration) -> Result<()> {
        if self.live {
            self.pipeline.send_event(gst::event::Eos::new());
        }
        let r = self.wait_eos(timeout);
        self.pipeline.set_state(gst::State::Null)?;
        r
    }

    /// Close the current file without EOS (device vanished, pipeline in
    /// error). Returns bytes written. Used for segment handling.
    pub fn close_segment(&self) -> Result<u64> {
        let _ = self.pipeline.set_state(gst::State::Null);
        let w = self.writer.lock().unwrap().take();
        match w {
            Some(w) => w.finalize(),
            None => Ok(0),
        }
    }

    pub fn shutdown(&self) {
        let _ = self.pipeline.set_state(gst::State::Null);
    }

    pub fn config(&self) -> &AudioConfig {
        &self.cfg
    }
}

fn opt(v: Option<u64>) -> String {
    v.map(|x| x.to_string()).unwrap_or_else(|| "null".into())
}

/// Run a non-live file through the pipeline to completion.
pub fn run_file(source: &dyn CaptureSource, cfg: AudioConfig) -> Result<Arc<Stats>> {
    let p = AudioPipeline::build(source, cfg)?;
    p.start()?;
    let r = p.wait_eos(Duration::from_secs(120));
    p.shutdown();
    r?;
    Ok(p.stats.clone())
}
