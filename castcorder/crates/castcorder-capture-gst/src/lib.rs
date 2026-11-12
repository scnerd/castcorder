//! [`CaptureSource`] over GStreamer.
//!
//! One `source -> appsink` pipeline and nothing else: no converter, no resampler,
//! no effects. The appsink only accepts interleaved little-endian S16, S24 (packed)
//! or F32, so whatever the source negotiates inside that set is what the caller
//! gets, and [`CaptureSource::format`] reports it.
//!
//! Sources: the platform microphone element (`wasapi2src` with low-latency on
//! Windows, `pulsesrc` on Linux, `osxaudiosrc` on macOS) via [`GstCapture::open_device`],
//! or a WAV file through `filesrc ! wavparse` via [`GstCapture::open_file`] (tests).
//!
//! Device loss is an end of the source: [`CaptureSource::next_buffer`] returns
//! `Ok(None)` and [`GstCapture::end_reason`] says why ([`EndReason::DeviceRemoved`]
//! from the device monitor, or [`EndReason::NoData`] from the watchdog, which is
//! the only signal when the sound server posts no error, as `pulsesrc` does).
//! Segmenting, reopening and reconnecting are the caller's business.
//!
//! The `*_with_live` opens tee the source: this [`CaptureSource`] is the
//! recording branch (never leaky, its streaming thread raised to the highest
//! priority available) and a [`LiveOpus`] is the live branch (leaky queue,
//! webrtcdsp, Opus); see the `live` module. [`Playback`] plays the call's
//! Opus streams on this machine and is the echo canceller's reference.

use std::io;
use std::path::Path;
use std::time::{Duration, Instant};

use castcorder_port_capture::{AudioBuffer, AudioFormat, CaptureSource, SampleFormat};
use gst::prelude::*;

mod live;
mod playback;

pub use live::{LiveOptions, LiveOpus, OpusPacket, raise_current_thread_priority};
pub use playback::Playback;

/// Device class of audio inputs in GStreamer's device monitor.
const INPUT_CLASS: &str = "Audio/Source";
/// How long one wait for a sample lasts before the loss checks run.
const POLL: Duration = Duration::from_millis(50);

/// An audio input device as the device monitor lists it.
#[derive(Clone, Debug)]
pub struct InputDevice {
    /// Name shown to a person; what [`GstCapture::open_device`] matches.
    pub display_name: String,
    /// The monitor's properties, as `key=value` text (ids, default flag, ...).
    pub properties: String,
    /// Whether the platform reports it as the default input.
    pub is_default: bool,
}

/// Why a source ended.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EndReason {
    /// The stream finished (a file ran out).
    Eos,
    /// The device monitor reported the chosen device removed.
    DeviceRemoved,
    /// No data arrived within the watchdog limit.
    NoData,
    /// The caller stopped the source.
    Stopped,
}

/// Timing of the loss checks for a live device.
#[derive(Clone, Copy, Debug)]
pub struct Watchdog {
    /// Longest wait for the first buffer after starting.
    pub first_data: Duration,
    /// Longest gap between buffers once data has flowed.
    pub gap: Duration,
}

impl Default for Watchdog {
    fn default() -> Self {
        Self {
            first_data: Duration::from_secs(5),
            gap: Duration::from_millis(1500),
        }
    }
}

fn err(msg: impl Into<String>) -> io::Error {
    io::Error::other(msg.into())
}

fn init() -> io::Result<()> {
    gst::init().map_err(|e| err(format!("gstreamer init failed: {e}")))
}

/// The platform's microphone element.
fn platform_source_factory() -> &'static str {
    if cfg!(target_os = "windows") {
        "wasapi2src"
    } else if cfg!(target_os = "macos") {
        "osxaudiosrc"
    } else {
        "pulsesrc"
    }
}

fn device_is_default(d: &gst::Device) -> bool {
    d.properties()
        .and_then(|p| p.get::<bool>("device.default").ok())
        .unwrap_or(false)
}

/// wasapi2 lists render endpoints (loopback) as sources; they are not microphones.
fn is_loopback(d: &gst::Device) -> bool {
    d.properties()
        .and_then(|p| p.get::<bool>("wasapi2.device.loopback").ok())
        .unwrap_or(false)
}

fn describe(d: &gst::Device) -> InputDevice {
    InputDevice {
        display_name: d.display_name().to_string(),
        properties: d.properties().map(|p| p.to_string()).unwrap_or_default(),
        is_default: device_is_default(d),
    }
}

/// List the audio input devices the platform's device provider reports.
pub fn list_input_devices() -> io::Result<Vec<InputDevice>> {
    init()?;
    let monitor = gst::DeviceMonitor::new();
    monitor.add_filter(Some(INPUT_CLASS), None);
    monitor
        .start()
        .map_err(|e| err(format!("device monitor failed to start: {e}")))?;
    let devices: Vec<InputDevice> = monitor
        .devices()
        .iter()
        .filter(|d| !is_loopback(d))
        .map(describe)
        .collect();
    monitor.stop();
    tracing::info!(count = devices.len(), "enumerated audio input devices");
    Ok(devices)
}

/// A GStreamer capture: format fixed at open, buffers pulled with `next_buffer`.
pub struct GstCapture {
    pipeline: gst::Pipeline,
    sink: gst_app::AppSink,
    format: AudioFormat,
    /// Sample pulled while negotiating, handed out first.
    pending: Option<Vec<u8>>,
    /// Bytes of a partial trailing frame carried to the next buffer.
    carry: Vec<u8>,
    next_frame: u64,
    ended: Option<EndReason>,
    live: Option<LiveState>,
    timing: Option<CaptureTiming>,
}

/// When a buffer's first frame was captured, by the pipeline's clock (the
/// audio device's clock where its source provides one), measured as the
/// buffer was pulled. Timing metadata turns `age` into a capture time on the
/// caller's clock: `now - age`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct CaptureTiming {
    /// The frame (numbered as `AudioBuffer::first_frame`) this is for.
    pub frame: u64,
    /// Its running time (buffer PTS in the segment).
    pub running_time: Duration,
    /// Pipeline clock at the pull minus (base time + running time).
    pub age: Duration,
}

struct LiveState {
    monitor: gst::DeviceMonitor,
    /// Display name of the chosen device, when known.
    device_name: Option<String>,
    watchdog: Watchdog,
    last_data: Instant,
    got_data: bool,
}

fn appsink_caps() -> gst::Caps {
    gst::Caps::builder("audio/x-raw")
        .field("format", gst::List::new(["S16LE", "S24LE", "F32LE"]))
        .field("layout", "interleaved")
        .build()
}

fn build_sink() -> io::Result<gst_app::AppSink> {
    let sink = gst_app::AppSink::builder()
        .name("sink")
        .caps(&appsink_caps())
        .sync(false)
        .build();
    Ok(sink)
}

fn format_from_caps(caps: &gst::CapsRef) -> io::Result<AudioFormat> {
    let info = gst_audio::AudioInfo::from_caps(caps)
        .map_err(|e| err(format!("unusable caps {caps}: {e}")))?;
    let sample_format = match info.format() {
        gst_audio::AudioFormat::S16le => SampleFormat::I16,
        gst_audio::AudioFormat::S24le => SampleFormat::I24,
        gst_audio::AudioFormat::F32le => SampleFormat::F32,
        other => return Err(err(format!("unsupported sample format {other:?}"))),
    };
    Ok(AudioFormat {
        sample_rate: info.rate(),
        channels: info.channels() as u16,
        sample_format,
    })
}

impl GstCapture {
    /// Play a WAV file (`filesrc ! wavparse ! appsink`), as fast as the reader pulls.
    pub fn open_file(path: &Path) -> io::Result<Self> {
        Ok(Self::open_file_inner(path, None)?.0)
    }

    /// Play a WAV file teed into this source (the recording branch, as fast as
    /// the reader pulls) and a live branch producing Opus (see [`LiveOpus`]).
    pub fn open_file_with_live(path: &Path, live: &LiveOptions) -> io::Result<(Self, LiveOpus)> {
        let (cap, opus) = Self::open_file_inner(path, Some(live))?;
        Ok((cap, opus.ok_or_else(|| err("live branch missing"))?))
    }

    fn open_file_inner(
        path: &Path,
        live: Option<&LiveOptions>,
    ) -> io::Result<(Self, Option<LiveOpus>)> {
        init()?;
        let pipeline = gst::Pipeline::new();
        let file = gst::ElementFactory::make("filesrc")
            .property("location", path.to_string_lossy().as_ref())
            .build()
            .map_err(|e| err(format!("filesrc: {e}")))?;
        let parse = gst::ElementFactory::make("wavparse")
            .build()
            .map_err(|e| err(format!("wavparse: {e}")))?;
        pipeline
            .add_many([&file, &parse])
            .map_err(|e| err(e.to_string()))?;
        file.link(&parse).map_err(|e| err(format!("link: {e}")))?;
        let sink = build_sink()?;
        // A file may be read ahead only a little: block upstream instead of buffering it all.
        sink.set_max_buffers(32);
        let opus = Self::assemble(&pipeline, &parse, &sink, live)?;
        tracing::info!(path = %path.display(), teed = live.is_some(), "capture source chosen: wavparse file");
        Ok((
            Self::start(pipeline, sink, None, Duration::from_secs(10))?,
            opus,
        ))
    }

    /// Link `src` straight into `sink`, or through a tee with a live branch.
    fn assemble(
        pipeline: &gst::Pipeline,
        src: &gst::Element,
        sink: &gst_app::AppSink,
        live: Option<&LiveOptions>,
    ) -> io::Result<Option<LiveOpus>> {
        if src.parent().is_none() {
            pipeline.add(src).map_err(|e| err(e.to_string()))?;
        }
        match live {
            Some(opts) => live::build_tee(pipeline, src, sink, opts).map(Some),
            None => {
                pipeline.add(sink).map_err(|e| err(e.to_string()))?;
                src.link(sink)
                    .map_err(|e| err(format!("link source to sink: {e}")))?;
                Ok(None)
            }
        }
    }

    /// Capture from an audio input. `name` is a display name from
    /// [`list_input_devices`]; `None` is the platform default input.
    pub fn open_device(name: Option<&str>, watchdog: Watchdog) -> io::Result<Self> {
        Ok(Self::open_device_inner(name, watchdog, None)?.0)
    }

    /// Capture from an audio input teed into this source (the recording
    /// branch, bit-exact, never dropping) and a live branch producing Opus
    /// (see [`LiveOpus`]). `name` as for [`GstCapture::open_device`].
    pub fn open_device_with_live(
        name: Option<&str>,
        watchdog: Watchdog,
        live: &LiveOptions,
    ) -> io::Result<(Self, LiveOpus)> {
        let (cap, opus) = Self::open_device_inner(name, watchdog, Some(live))?;
        Ok((cap, opus.ok_or_else(|| err("live branch missing"))?))
    }

    fn open_device_inner(
        name: Option<&str>,
        watchdog: Watchdog,
        live_opts: Option<&LiveOptions>,
    ) -> io::Result<(Self, Option<LiveOpus>)> {
        init()?;
        let monitor = gst::DeviceMonitor::new();
        monitor.add_filter(Some(INPUT_CLASS), None);
        monitor
            .start()
            .map_err(|e| err(format!("device monitor failed to start: {e}")))?;

        let (src, device_name) = match name {
            Some(name) => {
                let device = monitor
                    .devices()
                    .into_iter()
                    .find(|d| !is_loopback(d) && d.display_name() == name)
                    .ok_or_else(|| err(format!("no audio input named {name:?}")))?;
                let src = device
                    .create_element(None)
                    .map_err(|e| err(format!("device element: {e}")))?;
                (src, Some(name.to_string()))
            }
            None => {
                let factory = platform_source_factory();
                let src = gst::ElementFactory::make(factory)
                    .build()
                    .map_err(|e| err(format!("{factory}: {e}")))?;
                let default = monitor
                    .devices()
                    .into_iter()
                    .find(|d| !is_loopback(d) && device_is_default(d))
                    .map(|d| d.display_name().to_string());
                (src, default)
            }
        };
        if src.find_property("low-latency").is_some() {
            src.set_property("low-latency", true);
        }
        tracing::info!(
            element = %src.factory().map(|f| f.name().to_string()).unwrap_or_default(),
            device = ?device_name,
            "capture source chosen"
        );

        let pipeline = gst::Pipeline::new();
        let sink = build_sink()?;
        // Never block a live device on a slow reader: let the queue grow.
        sink.set_max_buffers(0);
        let opus = Self::assemble(&pipeline, &src, &sink, live_opts)?;
        let live = LiveState {
            monitor,
            device_name,
            watchdog,
            last_data: Instant::now(),
            got_data: false,
        };
        Ok((
            Self::start(pipeline, sink, Some(live), watchdog.first_data)?,
            opus,
        ))
    }

    fn start(
        pipeline: gst::Pipeline,
        sink: gst_app::AppSink,
        live: Option<LiveState>,
        first_wait: Duration,
    ) -> io::Result<Self> {
        pipeline
            .set_state(gst::State::Playing)
            .map_err(|e| err(format!("cannot start pipeline: {e}")))?;
        let mut cap = Self {
            pipeline,
            sink,
            format: AudioFormat {
                sample_rate: 0,
                channels: 0,
                sample_format: SampleFormat::I16,
            },
            pending: None,
            carry: Vec::new(),
            next_frame: 0,
            ended: None,
            live,
            timing: None,
        };
        // The first sample carries the negotiated caps.
        let deadline = Instant::now() + first_wait;
        let sample = loop {
            if let Some(s) = cap.sink.try_pull_sample(gst::ClockTime::from_mseconds(50)) {
                break s;
            }
            cap.check_bus()?;
            if cap.sink.is_eos() {
                return Err(err("source ended before any audio"));
            }
            if Instant::now() > deadline {
                return Err(err("no audio before the start-up deadline"));
            }
        };
        let caps = sample.caps().ok_or_else(|| err("sample without caps"))?;
        cap.format = format_from_caps(caps)?;
        tracing::info!(
            rate = cap.format.sample_rate,
            channels = cap.format.channels,
            sample_format = ?cap.format.sample_format,
            caps = %caps,
            "caps negotiated"
        );
        cap.pending = Some(sample_bytes(&sample)?);
        if let Some(live) = cap.live.as_mut() {
            live.last_data = Instant::now();
            live.got_data = true;
        }
        Ok(cap)
    }

    /// The capture timing of the last buffer pulled from GStreamer (none for
    /// the buffer pulled while negotiating, or without a clock or PTS).
    pub fn last_timing(&self) -> Option<CaptureTiming> {
        self.timing
    }

    /// Why the source ended, once `next_buffer` has returned `None`.
    pub fn end_reason(&self) -> Option<EndReason> {
        self.ended
    }

    /// Stop capturing. Ends the source with [`EndReason::Stopped`].
    pub fn stop(&mut self) {
        self.end(EndReason::Stopped);
    }

    fn end(&mut self, reason: EndReason) {
        if self.ended.is_none() {
            self.ended = Some(reason);
            // At end of stream the pipeline stays up until drop or stop, so a
            // live branch can still drain to its own end.
            if reason != EndReason::Eos {
                let _ = self.pipeline.set_state(gst::State::Null);
            }
            if let Some(live) = &self.live {
                live.monitor.stop();
            }
            tracing::info!(?reason, frames = self.next_frame, "capture ended");
        }
    }

    /// Pipeline errors become `Err`; everything else on the bus is ignored.
    fn check_bus(&self) -> io::Result<()> {
        let Some(bus) = self.pipeline.bus() else {
            return Ok(());
        };
        while let Some(msg) =
            bus.pop_filtered(&[gst::MessageType::Error, gst::MessageType::Warning])
        {
            match msg.view() {
                gst::MessageView::Error(e) => {
                    tracing::error!(error = %e.error(), debug = ?e.debug(), "pipeline error");
                    return Err(err(format!("pipeline error: {}", e.error())));
                }
                gst::MessageView::Warning(w) => {
                    tracing::warn!(warning = %w.error(), "pipeline warning");
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// Run the device-loss checks; returns the reason if the device is gone.
    fn check_loss(&mut self) -> Option<EndReason> {
        let live = self.live.as_mut()?;
        let bus = live.monitor.bus();
        while let Some(msg) = bus.pop() {
            match msg.view() {
                gst::MessageView::DeviceRemoved(r) => {
                    let name = r.device().display_name().to_string();
                    let ours = live.device_name.as_deref().is_none_or(|n| n == name);
                    tracing::info!(device = %name, ours, "device removed");
                    if ours {
                        return Some(EndReason::DeviceRemoved);
                    }
                }
                gst::MessageView::DeviceAdded(a) => {
                    let name = a.device().display_name().to_string();
                    tracing::info!(device = %name, "device added or returned");
                }
                _ => {}
            }
        }
        let limit = if live.got_data {
            live.watchdog.gap
        } else {
            live.watchdog.first_data
        };
        if live.last_data.elapsed() > limit {
            tracing::warn!(
                limit_ms = limit.as_millis() as u64,
                "watchdog fired: no data"
            );
            return Some(EndReason::NoData);
        }
        None
    }
}

/// How long ago the first byte of `sample` was captured, by `pipeline`'s clock.
fn sample_age(pipeline: &gst::Pipeline, sample: &gst::Sample) -> Option<(Duration, Duration)> {
    let pts = sample.buffer()?.pts()?;
    let segment = sample.segment()?.downcast_ref::<gst::ClockTime>()?;
    let running = segment.to_running_time(pts)?;
    let now = pipeline.clock()?.time();
    let captured = pipeline.base_time()? + running;
    Some((
        Duration::from_nanos(now.saturating_sub(captured).nseconds()),
        Duration::from_nanos(running.nseconds()),
    ))
}

fn sample_bytes(sample: &gst::Sample) -> io::Result<Vec<u8>> {
    let buffer = sample
        .buffer()
        .ok_or_else(|| err("sample without buffer"))?;
    let map = buffer.map_readable().map_err(|e| err(e.to_string()))?;
    Ok(map.as_slice().to_vec())
}

impl CaptureSource for GstCapture {
    fn format(&self) -> AudioFormat {
        self.format
    }

    fn next_buffer(&mut self) -> io::Result<Option<AudioBuffer>> {
        let frame_bytes = self.format.frame_bytes();
        loop {
            if self.ended.is_some() {
                return Ok(None);
            }
            let bytes = match self.pending.take() {
                Some(b) => b,
                None => match self
                    .sink
                    .try_pull_sample(gst::ClockTime::from_mseconds(POLL.as_millis() as u64))
                {
                    Some(sample) => {
                        if let Some(live) = self.live.as_mut() {
                            live.last_data = Instant::now();
                            live.got_data = true;
                        }
                        let frame = self.next_frame;
                        self.timing =
                            sample_age(&self.pipeline, &sample).map(|(age, running_time)| {
                                CaptureTiming {
                                    frame,
                                    running_time,
                                    age,
                                }
                            });
                        sample_bytes(&sample)?
                    }
                    None => {
                        if self.sink.is_eos() {
                            self.end(EndReason::Eos);
                            return Ok(None);
                        }
                        if let Err(e) = self.check_bus() {
                            self.end(EndReason::Stopped);
                            return Err(e);
                        }
                        if let Some(reason) = self.check_loss() {
                            self.end(reason);
                            return Ok(None);
                        }
                        continue;
                    }
                },
            };
            let mut data = std::mem::take(&mut self.carry);
            data.extend_from_slice(&bytes);
            let whole = data.len() - data.len() % frame_bytes;
            self.carry = data.split_off(whole);
            if data.is_empty() {
                continue;
            }
            let first_frame = self.next_frame;
            self.next_frame += (data.len() / frame_bytes) as u64;
            return Ok(Some(AudioBuffer { first_frame, data }));
        }
    }
}

impl Drop for GstCapture {
    fn drop(&mut self) {
        let _ = self.pipeline.set_state(gst::State::Null);
        if let Some(live) = &self.live {
            live.monitor.stop();
        }
    }
}
