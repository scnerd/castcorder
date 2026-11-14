//! The host's own participant joining and leaving at runtime: whoever is at
//! the host machine takes part with its microphone, captured natively
//! (feature `native-capture`; the `native` module), without a browser.
//!
//! [`NativeCtl`] is shared by [`crate::Running`] (the app's host control,
//! `--participate` at start) and the localhost listener (`/native`, the
//! producer page's controls). Joining prepares the participant (playback
//! running), hands it to the call thread, which sends it the take recording
//! now, if any, and only then starts its recorder, so no audio of that take
//! is missed. Leaving tells the call thread first (the take's files are
//! finished and recorded in the manifest), then ends the recorder and waits
//! for its threads, so the input is closed before the next join.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, mpsc};
use std::thread::JoinHandle;
use std::time::Duration;

use castcorder_core::clocksync::HostTimeline;
use castcorder_port_clock::SystemClock;
use tracing::{info, warn};

use crate::call::{Command, TakeCtl};

/// How long the call thread may take to answer a join or a leave.
const ANSWER: Duration = Duration::from_secs(10);
/// How long a leave waits for the take's microphone files to be final.
const FINISH: Duration = Duration::from_secs(60);

/// What the host's own participant captures.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum NativeInput {
    /// An audio input by display name (as listed in [`NativeState::devices`]);
    /// `None` is the default input.
    Device(Option<String>),
    /// A WAV file played once, as fast as it is read, then silence (tests;
    /// nothing is played on this machine).
    File(PathBuf),
}

impl NativeInput {
    #[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
    fn label(&self) -> String {
        match self {
            NativeInput::Device(Some(name)) => name.clone(),
            NativeInput::Device(None) => "default input".into(),
            NativeInput::File(path) => format!("file {}", path.display()),
        }
    }
}

/// An audio input the host's participant can join with.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct NativeDevice {
    /// Its display name: what [`NativeInput::Device`] takes.
    pub name: String,
    /// The system's default input.
    pub default: bool,
}

/// Whether the host's own participant can join natively, and with what.
#[derive(Clone, Debug, PartialEq, Eq, serde::Serialize)]
pub struct NativeState {
    /// This build captures natively (feature `native-capture`).
    pub available: bool,
    /// The input it is in the call with, if joined.
    pub joined: Option<String>,
    /// The audio inputs GStreamer's device monitor reports.
    pub devices: Vec<NativeDevice>,
    /// Why the inputs could not be listed, if they could not.
    pub error: Option<String>,
}

/// What every join of the host's participant records with.
#[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
pub(crate) struct NativeBase {
    /// Display name, `[A-Za-z0-9-]`.
    pub name: String,
    /// The call (timing metadata header).
    pub call: String,
    /// The host timeline.
    pub timeline: HostTimeline<SystemClock>,
    /// The host's own device id (manifest items).
    pub device_id: String,
    /// Keep each WAV after its FLAC is verified.
    pub keep_wav: bool,
}

#[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
struct Joined {
    label: String,
    takes: mpsc::Sender<TakeCtl>,
    threads: Vec<JoinHandle<()>>,
}

/// Joins and leaves the host's own participant (see the module docs).
pub(crate) struct NativeCtl {
    tx: Mutex<mpsc::Sender<Command>>,
    /// The joined recorder's take control, shared with the
    /// [`crate::Finisher`] (Ctrl+C finishes its take).
    takes: Arc<Mutex<Option<mpsc::Sender<TakeCtl>>>>,
    #[cfg_attr(not(feature = "native-capture"), allow(dead_code))]
    base: NativeBase,
    joined: Mutex<Option<Joined>>,
}

impl NativeCtl {
    pub fn new(
        tx: mpsc::Sender<Command>,
        takes: Arc<Mutex<Option<mpsc::Sender<TakeCtl>>>>,
        base: NativeBase,
    ) -> Self {
        NativeCtl {
            tx: Mutex::new(tx),
            takes,
            base,
            joined: Mutex::new(None),
        }
    }

    /// A control for a call thread on `tx` (unit tests).
    #[cfg(test)]
    pub fn for_test(tx: mpsc::Sender<Command>) -> Self {
        Self::new(
            tx,
            Default::default(),
            NativeBase {
                name: "host".into(),
                call: "test".into(),
                timeline: HostTimeline::new(SystemClock::new()),
                device_id: "host-test".into(),
                keep_wav: true,
            },
        )
    }

    /// Join the call with `input`. Errors are a sentence for the person at
    /// the host.
    pub fn join(&self, input: NativeInput) -> Result<(), String> {
        #[cfg(not(feature = "native-capture"))]
        {
            let _ = input;
            Err(crate::NO_NATIVE.into())
        }
        #[cfg(feature = "native-capture")]
        {
            let mut joined = self.joined.lock().map_err(|_| "native lock poisoned")?;
            if joined.is_some() {
                return Err("the host microphone is already in the call".into());
            }
            let label = input.label();
            let (participant, prepared) = crate::native::prepare(crate::native::Config {
                input,
                name: self.base.name.clone(),
                call: self.base.call.clone(),
                timeline: self.base.timeline.clone(),
                device_id: self.base.device_id.clone(),
                keep_wav: self.base.keep_wav,
            });
            let takes = participant.takes.clone();
            let tx = self.tx.lock().map_err(|_| "host lock poisoned")?.clone();
            let (reply, answer) = mpsc::channel();
            tx.send(Command::JoinNative { participant, reply })
                .map_err(|_| "the host is not running")?;
            answer
                .recv_timeout(ANSWER)
                .map_err(|_| "the host did not answer".to_string())??;
            let threads = prepared.start(tx);
            if let Ok(mut t) = self.takes.lock() {
                *t = Some(takes.clone());
            }
            info!(input = %label, "host microphone joined");
            *joined = Some(Joined {
                label,
                takes,
                threads,
            });
            Ok(())
        }
    }

    /// Leave the call: the take recording now (if any) gets the microphone's
    /// files finished, then the input is closed. False if it was not joined.
    pub fn leave(&self) -> Result<bool, String> {
        let mut joined = self.joined.lock().map_err(|_| "native lock poisoned")?;
        let Some(j) = joined.take() else {
            return Ok(false);
        };
        if let Ok(mut t) = self.takes.lock() {
            *t = None;
        }
        let (reply, answer) = mpsc::channel();
        let sent = self
            .tx
            .lock()
            .is_ok_and(|tx| tx.send(Command::LeaveNative { reply }).is_ok());
        if sent {
            match answer.recv_timeout(ANSWER) {
                Ok(Ok(Some(done))) => match done.recv_timeout(FINISH) {
                    Ok(files) => info!(files = ?files, "host microphone take finalised"),
                    Err(_) => warn!(
                        "host microphone take not finalised in time; recovery runs at the next start"
                    ),
                },
                Ok(Ok(None)) => {}
                Ok(Err(e)) => warn!(error = %e, "host microphone leave"),
                Err(_) => warn!("the host did not answer the leave"),
            }
        }
        let _ = j.takes.send(TakeCtl::Quit);
        for t in j.threads {
            let _ = t.join();
        }
        info!(input = %j.label, "host microphone left");
        Ok(true)
    }

    /// Whether it can join, with what, and whether it is joined.
    pub fn state(&self) -> NativeState {
        let joined = self
            .joined
            .lock()
            .ok()
            .and_then(|j| j.as_ref().map(|j| j.label.clone()));
        let (devices, error) = list_devices();
        NativeState {
            available: crate::NATIVE_CAPTURE,
            joined,
            devices,
            error,
        }
    }
}

#[cfg(feature = "native-capture")]
fn list_devices() -> (Vec<NativeDevice>, Option<String>) {
    match castcorder_capture_gst::list_input_devices() {
        Ok(devices) => (
            devices
                .into_iter()
                .map(|d| NativeDevice {
                    name: d.display_name,
                    default: d.is_default,
                })
                .collect(),
            None,
        ),
        Err(e) => (vec![], Some(e.to_string())),
    }
}

#[cfg(not(feature = "native-capture"))]
fn list_devices() -> (Vec<NativeDevice>, Option<String>) {
    (vec![], None)
}
