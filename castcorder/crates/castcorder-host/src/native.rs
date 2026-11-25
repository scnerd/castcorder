//! The host's own participant (feature `native-capture`): the microphone of
//! the host machine, captured with GStreamer, heard by every guest and
//! recorded into the library; the call played on this machine. It joins and
//! leaves at runtime (`NativeCtl`: `--participate` at start, the producer
//! page's native microphone controls, the app's host control).
//!
//! Threads:
//!
//! - `participant-rec` (raised to the highest priority available) opens the
//!   capture teed into a recording branch and a live branch, and pulls the
//!   recording branch. During a take it writes
//!   `<library>/<call>/takeNN/takeNN_<name>_mic.wav` with the crash-tolerant
//!   [`WavWriter`] (header rewritten and synced every second). On take stop
//!   the WAV is finalised and converted to FLAC beside it, verified (a 32-bit
//!   float track stays WAV). Device loss ends the segment; the device is
//!   reopened every second, and a take still recording continues in
//!   `takeNN_<name>_mic-seg2.wav` and so on.
//! - `participant-live` (one per opened capture) pulls Opus frames and sends
//!   them to the host thread, which forwards them to every guest. RTP time
//!   continues across reopens.
//! - `playback` decodes the guests' audio to the default output through a
//!   `webrtcechoprobe`, the echo canceller's reference.
//!
//! Each take also gets `takeNN_<name>_mic.timing.jsonl` (castcorder-core
//! timing): a segment line per WAV file and a reference point a second from
//! each GStreamer buffer's capture time (PTS and the pipeline clock), on the
//! host timeline itself.
//!
//! The WAV is kept after conversion by default (`--keep-wav false` deletes it
//! after a verified FLAC). After the take the files are recorded in the take
//! manifest under the host's own device id (`local`), through the host thread
//! (`Command::LocalTracks`), which also holds alignment of the take until the
//! files are final (`Command::LocalTracksDone`). A take left unfinalised by a
//! crash is recovered at the next start (`local::recover_takes`); Ctrl+C
//! finalises the take in progress first.
//!
//! Backup microphones (`TakeCtl::AddBackup`): each extra input is recorded
//! by a `participant-backup` thread of its own, recorder only (no live
//! stream, no playback), as source `mic-N` like a browser guest's backups:
//! `takeNN_<name>_mic-N.wav` (segments `mic-N-segK`) with its own timing
//! file and manifest items. The main recorder hands each take start and stop
//! on to its backups, and a take's files count as final (the stop's `done`,
//! `Command::LocalTracksDone`) only once every backup's are too.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::Duration;

use castcorder_capture_gst::{
    CaptureTiming, GstCapture, LiveOptions, LiveOpus, Playback, Watchdog,
    raise_current_thread_priority,
};
use castcorder_core::clocksync::HostTimeline;
use castcorder_core::timing::{self, Header, RefPoint, RefPoints, TimingLine};
use castcorder_port_capture::CaptureSource;
use castcorder_port_clock::SystemClock;
use castcorder_port_disk::{FsFile, FsStorage, Storage, StorageFile};
use castcorder_protocol::TakeId;
use castcorder_recorder::{WavWriter, WriterOptions};
use tracing::{error, info, warn};

use crate::NativeInput;
use crate::call::{Command, Heard, Participant, TakeCtl};
use crate::local;

/// The echo probe's name in the playback pipeline.
const ECHO_PROBE: &str = "castcorder_far_end";
/// Wait between attempts to open (or reopen) the microphone.
const REOPEN: Duration = Duration::from_secs(1);
/// How long a take's stop waits for each backup's files to be final.
const BACKUP_FINISH: Duration = Duration::from_secs(60);

/// What the host's own participant captures and how it is recorded.
#[derive(Clone, Debug)]
pub struct Config {
    /// The microphone (or, for tests, a WAV file played once).
    pub input: NativeInput,
    /// Display name, `[A-Za-z0-9-]`.
    pub name: String,
    /// The call (timing metadata header).
    pub call: String,
    /// The host timeline: this machine's tracks are on it by definition.
    pub timeline: HostTimeline<SystemClock>,
    /// The host's own device id: the device of its manifest items.
    pub device_id: String,
    /// Keep each WAV after its FLAC is verified (`--keep-wav`, default on).
    pub keep_wav: bool,
    /// Muted in the call (the live stream only; the track keeps recording).
    pub muted: Arc<AtomicBool>,
    /// A device to switch to mid-call: the recorder ends its segment, opens
    /// it, and records the next segment (same participant, same take).
    pub switch: Arc<std::sync::Mutex<Option<NativeInput>>>,
}

/// A participant about to join: playback runs (a device input only), the
/// recorder starts with [`Prepared::start`] once the call has the
/// participant, so a take already recording reaches it before any audio.
pub struct Prepared {
    cfg: Config,
    takes: mpsc::Receiver<TakeCtl>,
    echo: Option<String>,
    threads: Vec<thread::JoinHandle<()>>,
}

/// Start playback (for a device input) and make the participant for the call.
pub fn prepare(cfg: Config) -> (Participant, Prepared) {
    let mut threads = vec![];
    let mut heard = None;
    if matches!(cfg.input, NativeInput::Device(_)) {
        let (heard_tx, heard_rx) = mpsc::channel::<Heard>();
        let (ready_tx, ready_rx) = mpsc::channel::<bool>();
        let spawned = thread::Builder::new().name("playback".into()).spawn(move || {
            let mut pb = match Playback::new(Some(ECHO_PROBE)) {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "local playback unavailable; the call is not played here and echo is not cancelled");
                    let _ = ready_tx.send(false);
                    return;
                }
            };
            let _ = ready_tx.send(true);
            while let Ok(h) = heard_rx.recv() {
                match h {
                    Heard::Audio(a) => pb.push(a.guest.0, a.rtp_time, &a.data),
                    Heard::Gone(g) => pb.remove(g.0),
                }
            }
        });
        match spawned {
            Ok(t) => {
                threads.push(t);
                if ready_rx.recv().unwrap_or(false) {
                    heard = Some(heard_tx);
                }
            }
            Err(e) => warn!(error = %e, "playback thread not started"),
        }
    }
    let (takes_tx, takes_rx) = mpsc::channel();
    let echo = heard.is_some().then(|| ECHO_PROBE.to_string());
    let participant = Participant {
        name: cfg.name.clone(),
        takes: takes_tx,
        heard,
    };
    (
        participant,
        Prepared {
            cfg,
            takes: takes_rx,
            echo,
            threads,
        },
    )
}

impl Prepared {
    /// Start the recorder; returns every thread of this participant (they
    /// end after [`TakeCtl::Quit`] and once the call drops the participant).
    pub fn start(self, commands: mpsc::Sender<Command>) -> Vec<thread::JoinHandle<()>> {
        let Prepared {
            cfg,
            takes,
            echo,
            mut threads,
        } = self;
        info!(name = %cfg.name, input = ?cfg.input, playing = echo.is_some(), "host participant started");
        match thread::Builder::new()
            .name("participant-rec".into())
            .spawn(move || run(cfg, takes, commands, echo))
        {
            Ok(t) => threads.push(t),
            Err(e) => error!(error = %e, "participant thread not started"),
        }
        threads
    }
}

/// The take's timing metadata file (castcorder-core timing), one per take.
struct Timing {
    file: String,
    handle: FsFile,
    len: u64,
    /// Reference points of the current segment.
    refs: RefPoints,
    /// Frames written to the current segment's file.
    frames: u64,
    /// Running times (us) of recent reference frames, where known.
    audio_ts: std::collections::VecDeque<(u64, i64)>,
    rate: u64,
}

impl Timing {
    fn append(&mut self, line: &TimingLine) -> std::io::Result<()> {
        let text = line.to_line();
        self.handle.write_at(self.len, text.as_bytes())?;
        self.len += text.len() as u64;
        Ok(())
    }

    /// Write a finished reference point.
    fn reference(&mut self, (frame, at): (u64, i64)) -> std::io::Result<()> {
        let mut r = RefPoint::on_host(frame, at);
        r.audio_ts_us = self
            .audio_ts
            .iter()
            .find(|(f, _)| *f == frame)
            .map(|(_, ts)| *ts);
        self.append(&TimingLine::Ref(r))?;
        self.handle.sync()
    }
}

/// A backup microphone's recorder thread.
struct Backup {
    source: String,
    takes: mpsc::Sender<TakeCtl>,
    thread: Option<thread::JoinHandle<()>>,
}

/// The recorder's take state.
struct Rec {
    name: String,
    /// The source id: `mic`, or a backup's `mic-N`.
    source: String,
    /// The main microphone (else a backup): tells the call where its track
    /// stands, and owns the backups.
    main: bool,
    /// Backup recorders (main only).
    backups: Vec<Backup>,
    /// Removed backups' "files final" answers for the take recording now.
    pending: Vec<mpsc::Receiver<Vec<String>>>,
    /// Removed backups' threads, joined when this recorder ends.
    retired: Vec<thread::JoinHandle<()>>,
    call: String,
    device_id: String,
    keep_wav: bool,
    commands: mpsc::Sender<Command>,
    timeline: HostTimeline<SystemClock>,
    take: Option<(TakeId, FsStorage)>,
    segment: u32,
    writer: Option<(String, WavWriter<FsFile>)>,
    /// Finalised WAV files of the current take.
    files: Vec<String>,
    timing: Option<Timing>,
}

impl Rec {
    fn new(cfg: &Config, source: &str, main: bool, commands: mpsc::Sender<Command>) -> Self {
        Rec {
            name: cfg.name.clone(),
            source: source.to_string(),
            main,
            backups: vec![],
            pending: vec![],
            retired: vec![],
            call: cfg.call.clone(),
            device_id: cfg.device_id.clone(),
            keep_wav: cfg.keep_wav,
            commands,
            timeline: cfg.timeline.clone(),
            timing: None,
            take: None,
            segment: 0,
            writer: None,
            files: vec![],
        }
    }

    /// Apply a take command; true for [`TakeCtl::Quit`] (the take, if any,
    /// is finished first).
    fn control(&mut self, c: TakeCtl, cfg: &Config) -> bool {
        match c {
            TakeCtl::Start { take, dir } => {
                self.stop(None);
                info!(take = take.0, source = %self.source, "host microphone: take started");
                for b in &self.backups {
                    let _ = b.takes.send(TakeCtl::Start {
                        take,
                        dir: dir.clone(),
                    });
                }
                self.take = Some((take, dir));
                self.segment = 0;
            }
            TakeCtl::Stop { done } => self.stop(done),
            TakeCtl::Quit => {
                self.stop(None);
                return true;
            }
            TakeCtl::AddBackup { input, source } => self.add_backup(cfg, input, source),
            TakeCtl::RemoveBackup { source } => self.remove_backup(&source),
        }
        false
    }

    /// Start recording `input` as backup `source`, from the take recording now.
    fn add_backup(&mut self, cfg: &Config, input: NativeInput, source: String) {
        if !self.main || self.backups.iter().any(|b| b.source == source) {
            warn!(source = %source, "backup microphone not added: not the main recorder, or already recording");
            return;
        }
        let (tx, rx) = mpsc::channel();
        if let Some((take, dir)) = &self.take {
            let _ = tx.send(TakeCtl::Start {
                take: *take,
                dir: dir.clone(),
            });
        }
        let rec = Rec::new(cfg, &source, false, self.commands.clone());
        info!(source = %source, input = ?input, "backup microphone added");
        let thread = thread::Builder::new()
            .name("participant-backup".into())
            .spawn(move || run_backup(input, rx, rec))
            .map_err(|e| error!(error = %e, "backup microphone thread not started"))
            .ok();
        self.backups.push(Backup {
            source,
            takes: tx,
            thread,
        });
    }

    /// Stop recording backup `source`: its files of the take recording now
    /// are finished, and the take's stop waits for them.
    fn remove_backup(&mut self, source: &str) {
        let Some(i) = self.backups.iter().position(|b| b.source == source) else {
            return;
        };
        let b = self.backups.remove(i);
        let (done, rx) = mpsc::channel();
        if self.take.is_some() && b.takes.send(TakeCtl::Stop { done: Some(done) }).is_ok() {
            self.pending.push(rx);
        }
        let _ = b.takes.send(TakeCtl::Quit);
        self.retired.extend(b.thread);
        info!(source = %source, "backup microphone removed");
    }

    /// End the take: finalise, then convert to FLAC off this thread. The
    /// main recorder's take is final once its backups' are too.
    fn stop(&mut self, done: Option<mpsc::Sender<Vec<String>>>) {
        self.end_segment();
        let mut waits = std::mem::take(&mut self.pending);
        for b in &self.backups {
            let (tx, rx) = mpsc::channel();
            if b.takes.send(TakeCtl::Stop { done: Some(tx) }).is_ok() {
                waits.push(rx);
            }
        }
        let Some((take, dir)) = self.take.take() else {
            if let Some(d) = done {
                let mut out = vec![];
                for w in waits {
                    out.extend(w.recv_timeout(BACKUP_FINISH).unwrap_or_default());
                }
                let _ = d.send(out);
            }
            return;
        };
        let files = std::mem::take(&mut self.files);
        let timing = self.timing.take().and_then(|mut t| {
            let r = t
                .append(&TimingLine::End { frames: t.frames })
                .and_then(|()| t.handle.sync());
            match r {
                Ok(()) => Some(t.file),
                Err(e) => {
                    error!(file = %t.file, error = %e, "timing metadata not finalised");
                    None
                }
            }
        });
        info!(take = take.0, source = %self.source, files = ?files, timing = ?timing, "host microphone: take stopped");
        let (device_id, keep_wav, commands, participant, source, main) = (
            self.device_id.clone(),
            self.keep_wav,
            self.commands.clone(),
            self.name.clone(),
            self.source.clone(),
            self.main,
        );
        let spawned = thread::Builder::new()
            .name("participant-flac".into())
            .spawn(move || {
                // 1. Convert and read each FLAC back (the WAV stays meanwhile).
                let verified = local::convert_verified(&dir, &files);
                // 2. Record the finished files in the take manifest (the
                // host thread owns it); they are verified by being read whole.
                let mut recorded = false;
                match local::items(&dir, &files, timing.as_deref()) {
                    Ok(items) => {
                        let (ack, acked) = mpsc::channel();
                        let sent = commands.send(Command::LocalTracks {
                            take,
                            device: device_id,
                            participant,
                            source,
                            items,
                            ack,
                        });
                        recorded =
                            sent.is_ok() && acked.recv_timeout(Duration::from_secs(10)).is_ok();
                        if !recorded {
                            warn!(take = take.0, "host microphone files not recorded in the manifest");
                        }
                    }
                    Err(e) => {
                        error!(take = take.0, error = %e, "host microphone files not verified; not recorded in the manifest")
                    }
                }
                // 3. The WAV policy: only a verified FLAC, only once the
                // manifest item exists.
                if !keep_wav && recorded {
                    local::delete_wavs(&dir, &verified);
                } else if !keep_wav {
                    warn!(take = take.0, "--keep-wav false: WAVs kept, the manifest item was not recorded");
                }
                // 4. The backups' files of this take (main only): the take is
                // final once theirs are.
                let mut out: Vec<String> = Vec::new();
                for f in files.iter().map(|w| castcorder_recorder::flac_name(w)).chain(files.clone()).chain(timing) {
                    if dir.exists(&f).unwrap_or(false) && !out.contains(&f) {
                        out.push(f);
                    }
                }
                for w in waits {
                    match w.recv_timeout(BACKUP_FINISH) {
                        Ok(f) => out.extend(f),
                        Err(_) => warn!(take = take.0, "a backup microphone's files were not final in time"),
                    }
                }
                if main {
                    let _ = commands.send(Command::LocalTracksDone { take });
                }
                if let Some(d) = done {
                    let _ = d.send(out);
                }
            });
        if let Err(e) = spawned {
            error!(error = %e, "FLAC conversion not started");
        }
    }

    /// Finalise the open segment, if any.
    fn end_segment(&mut self) {
        if let Some((file, w)) = self.writer.take() {
            match w.finalize() {
                Ok(info) => {
                    info!(file = %file, bytes = info.data_len_header, "host microphone segment finalised")
                }
                Err(e) => {
                    error!(file = %file, error = %e, "segment not finalised; recover it from the file")
                }
            }
            self.files.push(file);
        }
        if let Some(t) = &mut self.timing
            && let Some(p) = t.refs.finish()
            && let Err(e) = t.reference(p)
        {
            error!(file = %t.file, error = %e, "timing metadata write failed");
        }
    }

    /// Tell the call where the microphone's track stands (the tiles' track
    /// chip), while a take records.
    fn source(&self, state: castcorder_protocol::SourceState) {
        if self.main && self.take.is_some() {
            let _ = self.commands.send(Command::NativeSource { state });
        }
    }

    /// Write a buffer, opening a segment first if a take is recording.
    /// `captured`: its first frame's capture time on the host timeline and its
    /// running time (us), from GStreamer; else it is placed by its arrival.
    fn write(
        &mut self,
        format: castcorder_port_capture::AudioFormat,
        data: &[u8],
        captured: Option<(i64, i64)>,
    ) {
        let Some((take, dir)) = self.take.clone() else {
            return;
        };
        if self.writer.is_none() {
            self.segment += 1;
            let src = if self.segment == 1 {
                self.source.clone()
            } else {
                format!("{}-seg{}", self.source, self.segment)
            };
            let file = (1..)
                .map(|n| {
                    let who = if n == 1 {
                        self.name.clone()
                    } else {
                        format!("{}-{n}", self.name)
                    };
                    format!("take{:02}_{who}_{src}.wav", take.0)
                })
                .find(|f| !dir.exists(f).unwrap_or(true))
                .expect("a free name");
            let opts = WriterOptions {
                sync_on_header: true,
                ..WriterOptions::default()
            };
            let created = dir
                .open(&file)
                .and_then(|f| WavWriter::create(f, format, opts));
            match created.and_then(|w| dir.sync_dir().map(|()| w)) {
                Ok(w) => {
                    info!(file = %file, source = %self.source, segment = self.segment, ?format, "host microphone segment started");
                    self.start_timing(take, dir.clone(), &file, format);
                    self.writer = Some((file, w));
                    self.source(castcorder_protocol::SourceState::Recording);
                }
                Err(e) => {
                    error!(file = %file, error = %e, "cannot create the track; this take is not recorded");
                    self.source(castcorder_protocol::SourceState::Unavailable);
                    self.take = None;
                    return;
                }
            }
        }
        if let Some((file, w)) = &mut self.writer
            && let Err(e) = w.write(data)
        {
            error!(file = %file, error = %e, "track write failed; segment ended");
            self.end_segment();
            return;
        }
        if let Some(t) = &mut self.timing {
            let n = (data.len() / format.frame_bytes()) as u64;
            let obs = match captured {
                Some((at, ts)) => {
                    let k = t.frames.div_ceil(t.rate) * t.rate;
                    if k < t.frames + n {
                        let ts = ts + ((k - t.frames) * 1_000_000 / t.rate) as i64;
                        t.audio_ts.push_back((k, ts));
                        if t.audio_ts.len() > 4 {
                            t.audio_ts.pop_front();
                        }
                    }
                    t.refs.observe(t.frames, at as f64)
                }
                None => t.refs.observe(t.frames + n, self.timeline.now_us() as f64),
            };
            t.frames += n;
            if let Some(p) = obs
                && let Err(e) = t.reference(p)
            {
                error!(file = %t.file, error = %e, "timing metadata write failed");
            }
        }
    }

    /// Begin the segment `wav` in the take's timing file, creating it (named
    /// after the first segment) on the first segment.
    fn start_timing(
        &mut self,
        take: TakeId,
        dir: FsStorage,
        wav: &str,
        format: castcorder_port_capture::AudioFormat,
    ) {
        let rate = format.sample_rate;
        let r = (|| -> std::io::Result<()> {
            if self.timing.is_none() {
                let file = format!("{}{}", wav.trim_end_matches(".wav"), timing::SUFFIX);
                let mut handle = dir.open(&file)?;
                handle.set_len(0)?;
                let mut t = Timing {
                    file,
                    handle,
                    len: 0,
                    refs: RefPoints::new(rate, u64::from(rate)),
                    frames: 0,
                    audio_ts: Default::default(),
                    rate: u64::from(rate.max(1)),
                };
                t.append(&TimingLine::Header(Header {
                    version: timing::VERSION,
                    call: self.call.clone(),
                    device: self.name.clone(),
                    take: take.0,
                    track: wav.to_string(),
                    source: self.source.clone(),
                    sample_rate: rate,
                    channels: format.channels,
                    device_clock: "the host timeline (this is the host)".into(),
                    host_clock: "host timeline: wall clock at host start + monotonic, us since the Unix epoch".into(),
                    host_timeline: self.timeline.id(),
                }))?;
                dir.sync_dir()?;
                self.timing = Some(t);
            }
            let t = self.timing.as_mut().expect("set above");
            t.refs = RefPoints::new(rate, u64::from(rate));
            t.frames = 0;
            t.audio_ts.clear();
            t.append(&TimingLine::Segment {
                segment: self.segment,
                file: wav.to_string(),
            })
        })();
        if let Err(e) = r {
            error!(wav = %wav, error = %e, "timing metadata not written for this take");
        }
    }
}

/// The recorder thread: open, pull, write, reopen, until the call ends.
fn run(
    cfg: Config,
    takes: mpsc::Receiver<TakeCtl>,
    commands: mpsc::Sender<Command>,
    echo_probe: Option<String>,
) {
    match castcorder_capture_gst::list_input_devices() {
        Ok(devices) => {
            for d in devices {
                info!(device = %d.display_name, default = d.is_default, "audio input (a --participate name)");
            }
        }
        Err(e) => warn!(error = %e, "audio inputs not listed"),
    }
    let outcome = raise_current_thread_priority();
    info!(thread = "participant-rec", outcome = %outcome, "recording thread priority");
    let mut opts = LiveOptions {
        echo_probe,
        ..LiveOptions::default()
    };
    let rtp_next = Arc::new(AtomicU64::new(0));
    let mut rec = Rec::new(&cfg, crate::local::SOURCE, true, commands.clone());
    let mut current = cfg.input.clone();
    loop {
        // Open already muted if the host is: muting only once the live loop
        // runs would send the first moments of audio to the call.
        opts.muted = cfg.muted.load(Ordering::Relaxed);
        let opened = match &current {
            NativeInput::Device(device) => {
                GstCapture::open_device_with_live(device.as_deref(), Watchdog::default(), &opts)
            }
            NativeInput::File(path) => GstCapture::open_file_with_live(path, &opts),
        };
        let (mut cap, live) = match opened {
            Ok(c) => c,
            Err(e) => {
                warn!(input = ?current, error = %e, "host microphone not available; retrying");
                match takes.recv_timeout(REOPEN) {
                    Ok(c) => {
                        if rec.control(c, &cfg) {
                            return;
                        }
                    }
                    Err(RecvTimeoutError::Timeout) => {}
                    Err(RecvTimeoutError::Disconnected) => {
                        rec.stop(None);
                        return;
                    }
                }
                continue;
            }
        };
        let format = cap.format();
        info!(
            ?format,
            integer = format.sample_format != castcorder_port_capture::SampleFormat::F32,
            flac = format.sample_format != castcorder_port_capture::SampleFormat::F32,
            "host microphone open: negotiated format (S24LE, then S16LE, preferred; a float device stays WAV)"
        );
        let stop = Arc::new(AtomicBool::new(false));
        let live_thread = {
            let (stop, commands, rtp_next, muted) = (
                stop.clone(),
                commands.clone(),
                rtp_next.clone(),
                cfg.muted.clone(),
            );
            thread::Builder::new()
                .name("participant-live".into())
                .spawn(move || live_loop(live, &stop, &commands, &rtp_next, &muted))
                .ok()
        };
        let mut switched = false;
        let quit = 'capture: loop {
            if let Some(next) = take_switch(&cfg.switch, &current) {
                info!(from = ?current, to = ?next, "host microphone switching");
                current = next;
                switched = true;
                break 'capture false;
            }
            loop {
                match takes.try_recv() {
                    Ok(c) => {
                        if rec.control(c, &cfg) {
                            break 'capture true;
                        }
                    }
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break 'capture true,
                }
            }
            match cap.next_buffer() {
                Ok(Some(b)) => {
                    let captured = cap.last_timing().filter(|t| t.frame == b.first_frame).map(
                        |t: CaptureTiming| {
                            (
                                cfg.timeline.now_us() - t.age.as_micros() as i64,
                                t.running_time.as_micros() as i64,
                            )
                        },
                    );
                    rec.write(format, &b.data, captured);
                }
                Ok(None) => {
                    warn!(reason = ?cap.end_reason(), "host microphone lost; segment ended");
                    break false;
                }
                Err(e) => {
                    warn!(error = %e, "host microphone failed; segment ended");
                    break false;
                }
            }
        };
        rec.end_segment();
        if !quit && !switched && matches!(current, NativeInput::Device(_)) {
            // The device went away mid-take: Mic lost until it is reopened
            // and a new segment records.
            rec.source(castcorder_protocol::SourceState::Lost);
        }
        stop.store(true, Ordering::Relaxed);
        cap.stop();
        if let Some(t) = live_thread {
            let _ = t.join();
        }
        drop(cap);
        if quit {
            rec.stop(None);
            return;
        }
        if matches!(current, NativeInput::File(_)) {
            // A file plays once: then only take commands, until quit.
            info!("host microphone file played to its end");
            loop {
                match takes.recv() {
                    Ok(c) => {
                        if rec.control(c, &cfg) {
                            return;
                        }
                    }
                    Err(_) => {
                        rec.stop(None);
                        return;
                    }
                }
            }
        }
        if !switched {
            thread::sleep(REOPEN);
        }
    }
}

impl Drop for Rec {
    /// A recorder ending ends its backups (each finishes its take first).
    fn drop(&mut self) {
        for b in self.backups.drain(..) {
            let _ = b.takes.send(TakeCtl::Quit);
            self.retired.extend(b.thread);
        }
        for t in self.retired.drain(..) {
            let _ = t.join();
        }
    }
}

/// Wait up to `wait` for take commands, applying each; true once told to end.
fn controls(
    rec: &mut Rec,
    cfg: &Config,
    takes: &mpsc::Receiver<TakeCtl>,
    wait: Option<Duration>,
) -> bool {
    let first = match wait {
        Some(w) => match takes.recv_timeout(w) {
            Ok(c) => Some(c),
            Err(RecvTimeoutError::Timeout) => None,
            Err(RecvTimeoutError::Disconnected) => return true,
        },
        None => match takes.try_recv() {
            Ok(c) => Some(c),
            Err(TryRecvError::Empty) => None,
            Err(TryRecvError::Disconnected) => return true,
        },
    };
    let Some(c) = first else {
        return false;
    };
    if rec.control(c, cfg) {
        return true;
    }
    loop {
        match takes.try_recv() {
            Ok(c) => {
                if rec.control(c, cfg) {
                    return true;
                }
            }
            Err(TryRecvError::Empty) => return false,
            Err(TryRecvError::Disconnected) => return true,
        }
    }
}

/// A backup microphone's thread: open `input` recorder-only (no live stream,
/// no playback), record each take into its own track, reopen a lost device
/// (a new segment), until told to end.
fn run_backup(input: NativeInput, takes: mpsc::Receiver<TakeCtl>, mut rec: Rec) {
    let outcome = raise_current_thread_priority();
    info!(thread = "participant-backup", source = %rec.source, outcome = %outcome, "recording thread priority");
    // A backup records no live stream: only the take state of `cfg` is used.
    let cfg = Config {
        input: input.clone(),
        name: rec.name.clone(),
        call: rec.call.clone(),
        timeline: rec.timeline.clone(),
        device_id: rec.device_id.clone(),
        keep_wav: rec.keep_wav,
        muted: Default::default(),
        switch: Default::default(),
    };
    loop {
        let opened = match &input {
            NativeInput::Device(device) => {
                GstCapture::open_device(device.as_deref(), Watchdog::default())
            }
            NativeInput::File(path) => GstCapture::open_file(path),
        };
        let mut cap = match opened {
            Ok(c) => c,
            Err(e) => {
                warn!(source = %rec.source, input = ?input, error = %e, "backup microphone not available; retrying");
                if controls(&mut rec, &cfg, &takes, Some(REOPEN)) {
                    rec.stop(None);
                    return;
                }
                continue;
            }
        };
        let format = cap.format();
        info!(source = %rec.source, ?format, "backup microphone open");
        let quit = loop {
            if controls(&mut rec, &cfg, &takes, None) {
                break true;
            }
            match cap.next_buffer() {
                Ok(Some(b)) => {
                    let captured = cap.last_timing().filter(|t| t.frame == b.first_frame).map(
                        |t: CaptureTiming| {
                            (
                                rec.timeline.now_us() - t.age.as_micros() as i64,
                                t.running_time.as_micros() as i64,
                            )
                        },
                    );
                    rec.write(format, &b.data, captured);
                }
                Ok(None) => {
                    warn!(source = %rec.source, reason = ?cap.end_reason(), "backup microphone lost; segment ended");
                    break false;
                }
                Err(e) => {
                    warn!(source = %rec.source, error = %e, "backup microphone failed; segment ended");
                    break false;
                }
            }
        };
        rec.end_segment();
        cap.stop();
        drop(cap);
        if quit {
            rec.stop(None);
            return;
        }
        if matches!(input, NativeInput::File(_)) {
            // A file plays once: then only take commands, until quit.
            info!(source = %rec.source, "backup microphone file played to its end");
            while !controls(&mut rec, &cfg, &takes, Some(Duration::from_secs(3600))) {}
            rec.stop(None);
            return;
        }
        if controls(&mut rec, &cfg, &takes, Some(REOPEN)) {
            rec.stop(None);
            return;
        }
    }
}

/// A pending switch to another device, taken from `slot` (only between
/// devices: a file input is a test input and is never switched).
fn take_switch(
    slot: &std::sync::Mutex<Option<NativeInput>>,
    current: &NativeInput,
) -> Option<NativeInput> {
    let mut slot = slot.lock().ok()?;
    match (slot.as_ref()?, current) {
        (NativeInput::Device(_), NativeInput::Device(_)) => slot.take(),
        _ => {
            slot.take();
            None
        }
    }
}

/// How often a muted microphone reports its raw level (as browser guests do).
const MIC_EVERY: Duration = Duration::from_millis(100);

/// Pull the live branch and hand its frames to the host thread, with RTP time
/// continuing after the previous capture's last frame. Mute (call-only)
/// silences the live branch's encoder; while muted the raw level is reported
/// ten times a second, and every change once (`Command::NativeMic`).
fn live_loop(
    mut live: LiveOpus,
    stop: &AtomicBool,
    commands: &mpsc::Sender<Command>,
    rtp_next: &AtomicU64,
    muted: &AtomicBool,
) {
    let base = rtp_next.load(Ordering::Relaxed);
    let mut first = None;
    let mut applied = None;
    let mut next_report = std::time::Instant::now();
    while !stop.load(Ordering::Relaxed) && !live.is_eos() {
        let m = muted.load(Ordering::Relaxed);
        let now = std::time::Instant::now();
        let changed = applied != Some(m);
        if changed {
            live.set_muted(m);
            applied = Some(m);
            info!(muted = m, "host microphone live stream");
        }
        if changed || (m && now >= next_report) {
            next_report = now + MIC_EVERY;
            let level = live.raw_level();
            if commands
                .send(Command::NativeMic { muted: m, level })
                .is_err()
            {
                return;
            }
        }
        let Some(p) = live.next_packet(Duration::from_millis(50)) else {
            continue;
        };
        let f = *first.get_or_insert(p.rtp_time);
        let rtp_time = base + p.rtp_time.saturating_sub(f);
        rtp_next.store(rtp_time + 960, Ordering::Relaxed);
        if commands
            .send(Command::LocalAudio {
                rtp_time,
                data: p.data,
            })
            .is_err()
        {
            return;
        }
    }
    if live.overruns() > 0 {
        info!(overruns = live.overruns(), "live queue dropped audio");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn a_switch_is_taken_once_and_only_between_devices() {
        let slot = std::sync::Mutex::new(None);
        let cur = NativeInput::Device(None);
        assert_eq!(take_switch(&slot, &cur), None);
        let usb = NativeInput::Device(Some("USB".into()));
        *slot.lock().unwrap() = Some(usb.clone());
        assert_eq!(take_switch(&slot, &cur), Some(usb.clone()));
        assert_eq!(take_switch(&slot, &usb), None, "taken once");
        // A file input is never switched, and the request is dropped.
        let file = NativeInput::File("a.wav".into());
        *slot.lock().unwrap() = Some(usb);
        assert_eq!(take_switch(&slot, &file), None);
        assert!(slot.lock().unwrap().is_none());
    }

    /// Wait until `path` holds at least `len` bytes.
    fn wait_len(path: &std::path::Path, len: u64) {
        let end = Instant::now() + Duration::from_secs(30);
        while std::fs::metadata(path).map(|m| m.len()).unwrap_or(0) < len {
            assert!(
                Instant::now() < end,
                "{} short of {len} bytes",
                path.display()
            );
            thread::sleep(Duration::from_millis(20));
        }
    }

    /// A take with a backup microphone is final (its stop's `done`, and
    /// `LocalTracksDone` for alignment) only once the backup's files are
    /// recorded too: here the backup's manifest ack is held back, and the
    /// take stays unfinished until it is given. Two WAV files stand in for
    /// the microphones (GStreamer's file source, as `task test:gst` runs).
    #[test]
    fn a_take_is_final_only_once_its_backups_files_are() {
        use castcorder_port_capture::{AudioFormat, Generator, SampleFormat, contract, encode_wav};
        let root = std::env::temp_dir().join(format!("castcorder-backup-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let format = AudioFormat {
            sample_rate: 48_000,
            channels: 1,
            sample_format: SampleFormat::I16,
        };
        let (_, pcm) = contract::drain(Generator::new(format, 480, Some(4_800)));
        let (main_wav, backup_wav) = (root.join("main.wav"), root.join("backup.wav"));
        std::fs::write(&main_wav, encode_wav(format, &pcm)).unwrap();
        std::fs::write(&backup_wav, encode_wav(format, &pcm)).unwrap();
        let take_dir = FsStorage::new(root.join("call").join("take01")).unwrap();

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (take_tx, take_rx) = mpsc::channel();
        // Queued before the recorder reads: the take, then the backup.
        take_tx
            .send(TakeCtl::Start {
                take: TakeId(1),
                dir: take_dir.clone(),
            })
            .unwrap();
        take_tx
            .send(TakeCtl::AddBackup {
                input: NativeInput::File(backup_wav),
                source: "mic-2".into(),
            })
            .unwrap();
        let cfg = Config {
            input: NativeInput::File(main_wav),
            name: "host".into(),
            call: "call".into(),
            timeline: HostTimeline::new(SystemClock::new()),
            device_id: "host-test".into(),
            keep_wav: true,
            muted: Default::default(),
            switch: Default::default(),
        };
        let rec = thread::spawn(move || run(cfg, take_rx, cmd_tx, None));
        let dir = root.join("call").join("take01");
        wait_len(&dir.join("take01_host_mic.wav"), 44 + pcm.len() as u64);
        wait_len(&dir.join("take01_host_mic-2.wav"), 44 + pcm.len() as u64);
        let (done_tx, done_rx) = mpsc::channel();
        take_tx
            .send(TakeCtl::Stop {
                done: Some(done_tx),
            })
            .unwrap();
        // Both sources are recorded in the manifest; the backup's ack is held.
        let mut held = None;
        let mut sources = vec![];
        let mut done_early = false;
        let end = Instant::now() + Duration::from_secs(30);
        while held.is_none() || sources.len() < 2 {
            assert!(Instant::now() < end, "manifest items not sent: {sources:?}");
            match cmd_rx.recv_timeout(Duration::from_millis(50)) {
                Ok(Command::LocalTracks {
                    source, items, ack, ..
                }) => {
                    let names: Vec<_> = items.iter().map(|i| i.name.clone()).collect();
                    sources.push((source.clone(), names));
                    if source == "mic-2" {
                        held = Some(ack);
                    } else {
                        let _ = ack.send(());
                    }
                }
                Ok(Command::LocalTracksDone { .. }) => done_early = true,
                _ => {}
            }
        }
        // Main's files are done; the take is not, while the backup's are not.
        let until = Instant::now() + Duration::from_millis(300);
        while Instant::now() < until {
            if let Ok(Command::LocalTracksDone { .. }) =
                cmd_rx.recv_timeout(Duration::from_millis(20))
            {
                done_early = true;
            }
        }
        assert!(!done_early, "the take was final before its backup's files");
        assert!(
            done_rx.try_recv().is_err(),
            "done before the backup's files"
        );
        let _ = held.take().unwrap().send(());
        let files = done_rx.recv_timeout(Duration::from_secs(30)).expect("done");
        let end = Instant::now() + Duration::from_secs(10);
        loop {
            assert!(Instant::now() < end, "no LocalTracksDone");
            if let Ok(Command::LocalTracksDone { take }) =
                cmd_rx.recv_timeout(Duration::from_millis(50))
            {
                assert_eq!(take, TakeId(1));
                break;
            }
        }
        sources.sort();
        assert_eq!(
            sources,
            [
                ("mic", vec!["take01_host_mic", "take01_host_mic-timing"]),
                (
                    "mic-2",
                    vec!["take01_host_mic-2", "take01_host_mic-2-timing"]
                ),
            ]
            .map(|(s, n)| (s.to_string(), n.iter().map(|x| x.to_string()).collect()))
        );
        for f in [
            "take01_host_mic.flac",
            "take01_host_mic-2.flac",
            "take01_host_mic-2.wav",
            "take01_host_mic-2.timing.jsonl",
        ] {
            assert!(files.contains(&f.to_string()), "{f} not in {files:?}");
        }
        drop(take_tx);
        drop(cmd_rx);
        rec.join().unwrap();
        let _ = std::fs::remove_dir_all(&root);
    }

    /// Loose bound on start and stop latency (logged, not the point).
    const SANITY_US: i64 = 3_000_000;

    /// Real hardware: the default microphone through the full path (tee,
    /// recorder thread, WavWriter in a library take directory, FLAC) while the
    /// live branch makes Opus. Skips without an input device (fails instead under CASTCORDER_CI).
    #[test]
    fn default_microphone_records_a_take_into_the_library() {
        let devices = castcorder_capture_gst::list_input_devices().expect("enumeration");
        if devices.is_empty() {
            assert!(
                std::env::var_os("CASTCORDER_CI").is_none(),
                "CASTCORDER_CI is set but this machine has no audio input device (CI must provide one, e.g. a pulseaudio null sink)"
            );
            println!("SKIP: no audio input device on this machine");
            return;
        }
        let root = std::env::temp_dir().join(format!("castcorder-native-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let take_dir = FsStorage::new(root.join("call").join("take01")).unwrap();

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (take_tx, take_rx) = mpsc::channel();
        let timeline = HostTimeline::new(SystemClock::new());
        let cfg = Config {
            input: NativeInput::Device(None),
            name: "host".into(),
            call: "call".into(),
            timeline: timeline.clone(),
            device_id: "host-test".into(),
            keep_wav: true,
            muted: Default::default(),
            switch: Default::default(),
        };
        let rec = thread::spawn(move || run(cfg, take_rx, cmd_tx, None));
        // The first Opus frame means the capture is open.
        let opened = Instant::now() + Duration::from_secs(10);
        let mut packets = 0usize;
        while Instant::now() < opened && packets == 0 {
            if let Ok(Command::LocalAudio { .. }) = cmd_rx.recv_timeout(Duration::from_millis(100))
            {
                packets += 1;
            }
        }
        if packets == 0 {
            assert!(
                std::env::var_os("CASTCORDER_CI").is_none(),
                "CASTCORDER_CI is set but the default input delivered nothing"
            );
            println!("SKIP: the default input delivered nothing");
            drop(take_tx);
            let _ = rec.join();
            return;
        }
        let take_start_us = timeline.now_us();
        take_tx
            .send(TakeCtl::Start {
                take: TakeId(1),
                dir: take_dir.clone(),
            })
            .unwrap();
        let until = Instant::now() + Duration::from_millis(1500);
        while Instant::now() < until {
            if let Ok(Command::LocalAudio { data, .. }) =
                cmd_rx.recv_timeout(Duration::from_millis(50))
            {
                assert!(!data.is_empty());
                packets += 1;
            }
        }
        let (done_tx, done_rx) = mpsc::channel();
        let take_stop_us = timeline.now_us();
        take_tx
            .send(TakeCtl::Stop {
                done: Some(done_tx),
            })
            .unwrap();
        // The recorder asks the host thread (here: this test) to record the
        // finished files in the manifest.
        let mut items = vec![];
        let deadline = Instant::now() + Duration::from_secs(30);
        let files = loop {
            match done_rx.try_recv() {
                Ok(f) => break f,
                Err(mpsc::TryRecvError::Disconnected) => panic!("take not finished"),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            assert!(Instant::now() < deadline, "take not finished");
            if let Ok(Command::LocalTracks { items: i, ack, .. }) =
                cmd_rx.recv_timeout(Duration::from_millis(50))
            {
                items = i;
                let _ = ack.send(());
            }
        };
        println!("files: {files:?}; live packets: {packets}");
        drop(take_tx);
        drop(cmd_rx);
        rec.join().unwrap();

        assert!(
            files.contains(&"take01_host_mic.wav".to_string()),
            "{files:?}"
        );
        let mut f = take_dir.open("take01_host_mic.wav").unwrap();
        let info = castcorder_recorder::read_info(&mut f).unwrap();
        let frames = info.data_len_header / info.format.frame_bytes() as u64;
        println!("wav: {:?}, {frames} frames", info.format);
        assert_eq!(
            info.data_len_header,
            info.data_len_on_disk(),
            "header finalised"
        );
        assert!(frames > 0, "no frames recorded");
        assert!(packets > 25, "too few live packets: {packets}");
        // The manifest items: the WAV and the timing file, hashed whole.
        let names: Vec<_> = items.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(
            names,
            ["take01_host_mic", "take01_host_mic-timing"],
            "{items:?}"
        );
        // Integer formats become FLAC; a float device stays WAV.
        println!(
            "flac made: {}",
            files.contains(&"take01_host_mic.flac".to_string())
        );
        if info.format.sample_format != castcorder_port_capture::SampleFormat::F32 {
            assert!(
                files.contains(&"take01_host_mic.flac".to_string()),
                "{files:?}"
            );
        }
        // The timing metadata: header, segment, a reference point placing frame
        // 0 just after the take started, and the end line.
        let timing_file = "take01_host_mic.timing.jsonl";
        assert!(files.contains(&timing_file.to_string()), "{files:?}");
        let text =
            std::fs::read_to_string(root.join("call").join("take01").join(timing_file)).unwrap();
        println!(
            "timing metadata:
{text}"
        );
        let map = timing::TimingMap::from_lines(&timing::parse(&text).unwrap());
        let header = map.header.as_ref().expect("header");
        assert_eq!(
            (header.take, header.sample_rate),
            (1, info.format.sample_rate)
        );
        assert_eq!(map.segments.len(), 1);
        assert_eq!(map.segments[0].file, "take01_host_mic.wav");
        assert!(!map.segments[0].refs.is_empty(), "no reference points");
        assert_eq!(map.end_frames, Some(frames));
        let start = map.host_us(1, 0).unwrap() as i64;
        println!(
            "frame 0 at {} ms after the take started",
            (start - take_start_us) as f64 / 1000.0
        );
        // When capture began against the take start is logged, and bounded
        // only loosely (a recorder that never started fails): a fresh
        // pulsesrc delivers its first audio a second or more late, and the
        // metadata must say when it was captured, not pretend it was on time.
        assert!(
            (start - take_start_us).abs() < SANITY_US,
            "frame 0 at {start}, take at {take_start_us}"
        );
        // Ground truth for the mapping: the audio's own sample count. Each
        // reference point lies as far after the one before on the host
        // timeline as its frames are (pulsesrc's clock, running ahead and
        // then standing still, put 1 s of audio over 1.49 s: cast-y52).
        let refs = &map.segments[0].refs;
        for w in refs.windows(2) {
            let ((f0, h0), (f1, h1)) = (w[0], w[1]);
            let audio_us = (f1 - f0) as f64 / f64::from(header.sample_rate) * 1e6;
            let host_us = (h1 - h0) as f64;
            assert!(
                (host_us - audio_us).abs() < 20_000.0,
                "frames {f0}..{f1}: {audio_us} us of audio over {host_us} us of host time"
            );
        }
        let end = map.host_us(1, frames).unwrap() as i64;
        println!(
            "last frame at {} ms after the take stopped",
            (end - take_stop_us) as f64 / 1000.0
        );
        assert!(
            (end - take_stop_us).abs() < SANITY_US,
            "last frame at {end}, stop at {take_stop_us}"
        );
        let _ = std::fs::remove_dir_all(&root);
    }
}
