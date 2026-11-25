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
    /// Muted in the call (the live stream only; its track keeps recording).
    pub muted: bool,
    /// The backup microphones recording beside it (their labels: a device's
    /// display name), while joined.
    pub backups: Vec<String>,
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
    /// The main input (what a backup may not be).
    input: NativeInput,
    takes: mpsc::Sender<TakeCtl>,
    threads: Vec<JoinHandle<()>>,
    /// Backup microphones: (label, input, source id `mic-N`).
    backups: Vec<(String, NativeInput, String)>,
}

/// The next free backup source id: `mic-2`, `mic-3`, ... (as a browser
/// guest's backups).
fn next_source(taken: &[(String, NativeInput, String)]) -> String {
    (2..)
        .map(|n| format!("mic-{n}"))
        .find(|s| !taken.iter().any(|(_, _, t)| t == s))
        .expect("a free source id")
}

/// Whether `a` and `b` are the same input; `default` names the default
/// input's device, if known.
fn same_input(a: &NativeInput, b: &NativeInput, default: Option<&str>) -> bool {
    let named = |i: &NativeInput| match i {
        NativeInput::Device(None) => default.map(|d| NativeInput::Device(Some(d.to_string()))),
        other => Some(other.clone()),
    };
    a == b || named(a).is_some_and(|x| Some(x) == named(b))
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
    /// Muted in the call (call-only: the live stream; the track records).
    muted: Arc<std::sync::atomic::AtomicBool>,
    /// A device to switch to, taken by the recorder.
    switch: Arc<Mutex<Option<NativeInput>>>,
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
            muted: Default::default(),
            switch: Default::default(),
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

    /// Join the call with `input`: the live room's call, else the default
    /// room's. Errors are a sentence for the person at the host.
    pub fn join(&self, input: NativeInput) -> Result<(), String> {
        self.join_room(input, None)
    }

    /// Join `room`'s current call (`None`: as [`NativeCtl::join`]) with `input`.
    pub fn join_room(&self, input: NativeInput, room: Option<String>) -> Result<(), String> {
        #[cfg(not(feature = "native-capture"))]
        {
            let _ = (input, room);
            Err(crate::NO_NATIVE.into())
        }
        #[cfg(feature = "native-capture")]
        {
            let mut joined = self.joined.lock().map_err(|_| "native lock poisoned")?;
            if joined.is_some() {
                return Err("the host microphone is already in the call".into());
            }
            if let Ok(mut s) = self.switch.lock() {
                *s = None;
            }
            // Every join starts unmuted.
            self.muted
                .store(false, std::sync::atomic::Ordering::Relaxed);
            let label = input.label();
            let input_kept = input.clone();
            let (participant, prepared) = crate::native::prepare(crate::native::Config {
                input,
                name: self.base.name.clone(),
                call: self.base.call.clone(),
                timeline: self.base.timeline.clone(),
                device_id: self.base.device_id.clone(),
                keep_wav: self.base.keep_wav,
                muted: self.muted.clone(),
                switch: self.switch.clone(),
            });
            let takes = participant.takes.clone();
            let tx = self.tx.lock().map_err(|_| "host lock poisoned")?.clone();
            let (reply, answer) = mpsc::channel();
            tx.send(Command::JoinNative {
                room,
                participant,
                reply,
            })
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
                input: input_kept,
                takes,
                threads,
                backups: vec![],
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

    /// Switch the joined microphone to another input (`None`: the default)
    /// mid-call: the take keeps recording, with a new segment from the new
    /// device. Errors if the host microphone is not in the call.
    pub fn switch(&self, device: Option<String>) -> Result<(), String> {
        let mut joined = self.joined.lock().map_err(|_| "native lock poisoned")?;
        let Some(j) = joined.as_mut() else {
            return Err("the host microphone is not in the call".into());
        };
        let input = NativeInput::Device(device);
        let default = default_device();
        if j.backups
            .iter()
            .any(|(_, b, _)| same_input(b, &input, default.as_deref()))
        {
            return Err(format!(
                "{} is recording as a backup microphone; remove it as a backup first",
                input.label()
            ));
        }
        j.label = input.label();
        j.input = input.clone();
        *self.switch.lock().map_err(|_| "native lock poisoned")? = Some(input);
        info!(input = %j.label, "host microphone switch requested");
        Ok(())
    }

    /// Also record `input` as a backup microphone: a track of its own (source
    /// `mic-N`) in each take, from the one recording now. Only while the host
    /// microphone is in the call; not its own input, nor one already a backup.
    pub fn add_backup(&self, input: NativeInput) -> Result<String, String> {
        let mut joined = self.joined.lock().map_err(|_| "native lock poisoned")?;
        let Some(j) = joined.as_mut() else {
            return Err("the host microphone is not in the call".into());
        };
        let default = default_device();
        if same_input(&j.input, &input, default.as_deref()) {
            return Err(format!(
                "{} is the host microphone itself; choose another input as a backup",
                input.label()
            ));
        }
        if j.backups
            .iter()
            .any(|(_, b, _)| same_input(b, &input, default.as_deref()))
        {
            return Err(format!("{} is already a backup microphone", input.label()));
        }
        let source = next_source(&j.backups);
        j.takes
            .send(TakeCtl::AddBackup {
                input: input.clone(),
                source: source.clone(),
            })
            .map_err(|_| "the host microphone's recorder has ended")?;
        info!(input = %input.label(), source = %source, "backup microphone added");
        j.backups.push((input.label(), input, source.clone()));
        Ok(source)
    }

    /// Stop recording the backup microphone labelled `label` (its files of
    /// the take recording now are finished). False if it was not a backup.
    pub fn remove_backup(&self, label: &str) -> Result<bool, String> {
        let mut joined = self.joined.lock().map_err(|_| "native lock poisoned")?;
        let Some(j) = joined.as_mut() else {
            return Err("the host microphone is not in the call".into());
        };
        let Some(i) = j.backups.iter().position(|(l, _, _)| l == label) else {
            return Ok(false);
        };
        let (_, _, source) = j.backups.remove(i);
        let _ = j.takes.send(TakeCtl::RemoveBackup {
            source: source.clone(),
        });
        info!(input = %label, source = %source, "backup microphone removed");
        Ok(true)
    }

    /// Mute (true) or unmute the native microphone in the call: the live
    /// stream goes silent, its track keeps recording, and everyone sees it
    /// muted with its level moving. False if it is not in the call.
    pub fn set_muted(&self, muted: bool) -> bool {
        let joined = self.joined.lock().is_ok_and(|j| j.is_some());
        if joined {
            self.muted
                .store(muted, std::sync::atomic::Ordering::Relaxed);
            info!(muted, "host microphone mute");
        }
        joined
    }

    /// Whether it can join, with what, and whether it is joined.
    pub fn state(&self) -> NativeState {
        let (joined, backups) = self
            .joined
            .lock()
            .ok()
            .and_then(|j| {
                j.as_ref().map(|j| {
                    (
                        Some(j.label.clone()),
                        j.backups.iter().map(|(l, _, _)| l.clone()).collect(),
                    )
                })
            })
            .unwrap_or_default();
        let (devices, error) = list_devices();
        let joined_now = joined.is_some();
        NativeState {
            available: crate::NATIVE_CAPTURE,
            joined,
            devices,
            error,
            muted: joined_now && self.muted.load(std::sync::atomic::Ordering::Relaxed),
            backups,
        }
    }
}

/// The default input's display name, if the inputs can be listed.
fn default_device() -> Option<String> {
    list_devices()
        .0
        .into_iter()
        .find(|d| d.default)
        .map(|d| d.name)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn switching_needs_the_microphone_in_the_call() {
        let (tx, _rx) = mpsc::channel();
        let ctl = NativeCtl::for_test(tx);
        let e = ctl.switch(Some("USB".into())).unwrap_err();
        assert!(e.contains("not in the call"), "{e}");
    }

    #[test]
    fn backups_need_the_microphone_in_the_call() {
        let (tx, _rx) = mpsc::channel();
        let ctl = NativeCtl::for_test(tx);
        let e = ctl
            .add_backup(NativeInput::Device(Some("USB".into())))
            .unwrap_err();
        assert!(e.contains("not in the call"), "{e}");
        assert!(ctl.remove_backup("USB").is_err());
        assert!(ctl.state().backups.is_empty());
    }

    #[test]
    fn backup_sources_are_the_next_free_mic_n() {
        let b = |s: &str| ("x".to_string(), NativeInput::Device(None), s.to_string());
        assert_eq!(next_source(&[]), "mic-2");
        assert_eq!(next_source(&[b("mic-2"), b("mic-4")]), "mic-3");
    }

    #[test]
    fn the_default_input_is_the_device_it_names() {
        let usb = NativeInput::Device(Some("USB".into()));
        let default = NativeInput::Device(None);
        assert!(same_input(&usb, &usb, None));
        assert!(same_input(&default, &usb, Some("USB")));
        assert!(same_input(&usb, &default, Some("USB")));
        assert!(!same_input(&default, &usb, Some("Built-in")));
        assert!(!same_input(&default, &usb, None));
        let file = NativeInput::File("a.wav".into());
        assert!(!same_input(&file, &NativeInput::File("b.wav".into()), None));
    }
}
