//! The host's own participant (`--participate`, feature `native-capture`): the
//! microphone of the host machine, captured with GStreamer, heard by every
//! guest and recorded into the library; the call played on this machine.
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
//! The WAV is kept after conversion (the safety copy); nothing is added to the
//! take's manifest.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, TryRecvError};
use std::thread;
use std::time::Duration;

use castcorder_capture_gst::{
    GstCapture, LiveOptions, LiveOpus, Playback, Watchdog, raise_current_thread_priority,
};
use castcorder_port_capture::CaptureSource;
use castcorder_port_disk::{FsFile, FsStorage, Storage};
use castcorder_protocol::TakeId;
use castcorder_recorder::{FlacError, WavWriter, WriterOptions, wav_to_flac};
use tracing::{error, info, warn};

use crate::call::{Command, Heard, Participant, TakeCtl};

/// The echo probe's name in the playback pipeline.
const ECHO_PROBE: &str = "castcorder_far_end";
/// Wait between attempts to open (or reopen) the microphone.
const REOPEN: Duration = Duration::from_secs(1);

/// What `--participate` asked for.
#[derive(Clone, Debug)]
pub struct Config {
    /// Input device display name; `None` is the default input.
    pub device: Option<String>,
    /// Display name, `[A-Za-z0-9-]`.
    pub name: String,
}

/// Start playback and the recorder; returns the participant for the call.
pub fn start(cfg: Config, commands: mpsc::Sender<Command>) -> Participant {
    let (heard_tx, heard_rx) = mpsc::channel::<Heard>();
    let (ready_tx, ready_rx) = mpsc::channel::<bool>();
    thread::Builder::new()
        .name("playback".into())
        .spawn(move || {
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
        })
        .expect("spawn playback thread");
    let playing = ready_rx.recv().unwrap_or(false);

    let (takes_tx, takes_rx) = mpsc::channel();
    let echo = playing.then(|| ECHO_PROBE.to_string());
    let rec_cfg = cfg.clone();
    thread::Builder::new()
        .name("participant-rec".into())
        .spawn(move || run(rec_cfg, takes_rx, commands, echo))
        .expect("spawn participant thread");
    info!(name = %cfg.name, device = ?cfg.device, playing, "host participant started");
    Participant {
        name: cfg.name,
        takes: takes_tx,
        heard: playing.then_some(heard_tx),
    }
}

/// The recorder's take state.
struct Rec {
    name: String,
    take: Option<(TakeId, FsStorage)>,
    segment: u32,
    writer: Option<(String, WavWriter<FsFile>)>,
    /// Finalised WAV files of the current take.
    files: Vec<String>,
}

impl Rec {
    /// Apply a take command.
    fn control(&mut self, c: TakeCtl) {
        match c {
            TakeCtl::Start { take, dir } => {
                self.stop(None);
                info!(take = take.0, "host microphone: take started");
                self.take = Some((take, dir));
                self.segment = 0;
            }
            TakeCtl::Stop { done } => self.stop(done),
        }
    }

    /// End the take: finalise, then convert to FLAC off this thread.
    fn stop(&mut self, done: Option<mpsc::Sender<Vec<String>>>) {
        self.end_segment();
        let Some((take, dir)) = self.take.take() else {
            if let Some(d) = done {
                let _ = d.send(vec![]);
            }
            return;
        };
        let files = std::mem::take(&mut self.files);
        info!(take = take.0, files = ?files, "host microphone: take stopped");
        let spawned = thread::Builder::new()
            .name("participant-flac".into())
            .spawn(move || {
                let mut out = files.clone();
                for wav in &files {
                    let flac = format!("{}.flac", wav.trim_end_matches(".wav"));
                    match wav_to_flac(&dir, wav, &flac) {
                        Ok(r) => {
                            info!(wav = %wav, flac = %flac, frames = r.frames, bytes = r.flac_bytes, "host microphone track converted to FLAC");
                            out.push(flac);
                        }
                        Err(FlacError::NotApplicable(m)) => info!(wav = %wav, reason = %m, "kept as WAV"),
                        Err(e) => error!(wav = %wav, error = %e, "FLAC conversion failed; the WAV is kept"),
                    }
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
    }

    /// Write a buffer, opening a segment first if a take is recording.
    fn write(&mut self, format: castcorder_port_capture::AudioFormat, data: &[u8]) {
        let Some((take, dir)) = &self.take else {
            return;
        };
        if self.writer.is_none() {
            self.segment += 1;
            let src = if self.segment == 1 {
                "mic".to_string()
            } else {
                format!("mic-seg{}", self.segment)
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
                    info!(file = %file, segment = self.segment, ?format, "host microphone segment started");
                    self.writer = Some((file, w));
                }
                Err(e) => {
                    error!(file = %file, error = %e, "cannot create the track; this take is not recorded");
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
    let opts = LiveOptions {
        echo_probe,
        ..LiveOptions::default()
    };
    let rtp_next = Arc::new(AtomicU64::new(0));
    let mut rec = Rec {
        name: cfg.name.clone(),
        take: None,
        segment: 0,
        writer: None,
        files: vec![],
    };
    loop {
        let (mut cap, live) = match GstCapture::open_device_with_live(
            cfg.device.as_deref(),
            Watchdog::default(),
            &opts,
        ) {
            Ok(c) => c,
            Err(e) => {
                warn!(device = ?cfg.device, error = %e, "host microphone not available; retrying");
                match takes.recv_timeout(REOPEN) {
                    Ok(c) => rec.control(c),
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
        info!(?format, "host microphone open");
        let stop = Arc::new(AtomicBool::new(false));
        let live_thread = {
            let (stop, commands, rtp_next) = (stop.clone(), commands.clone(), rtp_next.clone());
            thread::Builder::new()
                .name("participant-live".into())
                .spawn(move || live_loop(live, &stop, &commands, &rtp_next))
                .ok()
        };
        let quit = 'capture: loop {
            loop {
                match takes.try_recv() {
                    Ok(c) => rec.control(c),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => break 'capture true,
                }
            }
            match cap.next_buffer() {
                Ok(Some(b)) => rec.write(format, &b.data),
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
        thread::sleep(REOPEN);
    }
}

/// Pull the live branch and hand its frames to the host thread, with RTP time
/// continuing after the previous capture's last frame.
fn live_loop(
    mut live: LiveOpus,
    stop: &AtomicBool,
    commands: &mpsc::Sender<Command>,
    rtp_next: &AtomicU64,
) {
    let base = rtp_next.load(Ordering::Relaxed);
    let mut first = None;
    while !stop.load(Ordering::Relaxed) && !live.is_eos() {
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

    /// Real hardware: the default microphone through the full path (tee,
    /// recorder thread, WavWriter in a library take directory, FLAC) while the
    /// live branch makes Opus. Skips without an input device.
    #[test]
    fn default_microphone_records_a_take_into_the_library() {
        let devices = castcorder_capture_gst::list_input_devices().expect("enumeration");
        if devices.is_empty() {
            println!("SKIP: no audio input device on this machine");
            return;
        }
        let root = std::env::temp_dir().join(format!("castcorder-native-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let take_dir = FsStorage::new(root.join("call").join("take01")).unwrap();

        let (cmd_tx, cmd_rx) = mpsc::channel();
        let (take_tx, take_rx) = mpsc::channel();
        let cfg = Config {
            device: None,
            name: "host".into(),
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
            println!("SKIP: the default input delivered nothing");
            drop(take_tx);
            let _ = rec.join();
            return;
        }
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
        take_tx
            .send(TakeCtl::Stop {
                done: Some(done_tx),
            })
            .unwrap();
        let files = done_rx
            .recv_timeout(Duration::from_secs(30))
            .expect("take finished");
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
        let _ = std::fs::remove_dir_all(&root);
    }
}
