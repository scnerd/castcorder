//! The live transcript (cast-7hm.2): what each participant says while a take
//! records, as text, for everyone in the call and in the call's folder.
//!
//! The call thread hands every participant's live Opus frames (guests' and
//! the host machine's own) to [`Transcripts`], which runs on its own thread:
//! per participant it decodes them (libopus), takes them to 16 kHz
//! ([`Downsampler`]), cuts them at pauses ([`Segmenter`]) and transcribes
//! each utterance with the engine. The call thread collects the lines with
//! [`Transcripts::lines`], appends them to `<call>/transcript.jsonl` and
//! sends them to everyone ([`castcorder_protocol::Event::Transcript`]).
//!
//! Only audio sent while a take records is given to it, so nothing said
//! between takes is transcribed (guests are told when a take records).

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::mpsc;
use std::thread;
use std::time::Duration;

use castcorder_core::transcript::{Downsampler, Segmenter, SegmenterConfig, Utterance};
use castcorder_port_transcribe::{SAMPLE_RATE, Transcriber};
use castcorder_protocol::{ParticipantId, TakeId, TranscriptLine};
use tracing::{info, warn};

/// The engine the host transcribes with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Engine {
    /// whisper.cpp with the ggml model at this path (a build with the
    /// `whisper` feature).
    Whisper(PathBuf),
    /// The behavioural simulation (`castcorder_port_transcribe::sim`): it
    /// hears only its tone code. For tests.
    Simulated,
}

impl Engine {
    /// Make the transcriber (loading a model takes a while: not on the call
    /// thread).
    fn open(&self) -> Result<Box<dyn Transcriber>, String> {
        match self {
            Engine::Simulated => Ok(Box::new(castcorder_port_transcribe::sim::Simulated::new())),
            #[cfg(feature = "whisper")]
            Engine::Whisper(path) => Ok(Box::new(castcorder_transcribe::Whisper::load(
                path,
                castcorder_transcribe::Device::Auto,
            )?)),
            #[cfg(not(feature = "whisper"))]
            Engine::Whisper(_) => Err("this build has no transcription engine".into()),
        }
    }
}

/// The transcript file in a call's folder.
pub const FILE: &str = "transcript.jsonl";

/// What the call thread sends the transcript thread.
enum In {
    /// One Opus frame of a participant, while `take` records, received at
    /// `host_us` on the host timeline.
    Audio {
        who: u64,
        name: String,
        take: TakeId,
        rtp_time: u64,
        opus: Vec<u8>,
        host_us: i64,
    },
    /// The participant left: finish what they were saying.
    Gone(u64),
    /// The take stopped: finish what everyone was saying.
    Flush,
}

/// A gap longer than this (48 kHz ticks) ends the utterance: the
/// participant's stream stopped, rather than a packet or two going missing.
const MAX_GAP: u64 = 48_000;

/// One participant's audio on its way to text.
struct Speaker {
    name: String,
    take: TakeId,
    decoder: opus::Decoder,
    down: Downsampler,
    seg: Segmenter,
    /// The RTP time of the segmenter's first sample, and its host time.
    anchor: (u64, i64),
    /// The RTP time the next frame should have.
    next_rtp: u64,
    /// Decoded 48 kHz audio, reused.
    pcm48: Vec<f32>,
    /// 16 kHz audio, reused.
    pcm16: Vec<f32>,
}

impl Speaker {
    fn new(name: String, take: TakeId, rtp_time: u64, host_us: i64) -> Result<Self, String> {
        Ok(Speaker {
            name,
            take,
            decoder: opus::Decoder::new(48_000, opus::Channels::Mono)
                .map_err(|e| format!("opus decoder: {e}"))?,
            down: Downsampler::new(),
            seg: Segmenter::new(SegmenterConfig::default()),
            anchor: (rtp_time, host_us),
            next_rtp: rtp_time,
            pcm48: vec![0.0; 5_760],
            pcm16: Vec::new(),
        })
    }

    /// When the utterance starting at 16 kHz sample `start` began.
    fn host_us(&self, start: u64) -> i64 {
        self.anchor.1 + (start * 1_000_000 / u64::from(SAMPLE_RATE)) as i64
    }

    /// Decode one frame (and conceal a short gap before it); the utterances
    /// it completes.
    fn frame(&mut self, rtp_time: u64, opus: &[u8]) -> Vec<Utterance> {
        if rtp_time < self.next_rtp {
            // Late or repeated: its time is already filled.
            return Vec::new();
        }
        let gap = rtp_time - self.next_rtp;
        self.pcm16.clear();
        if gap > 0 {
            let silence = vec![0.0; gap as usize];
            self.down.push(&silence, &mut self.pcm16);
        }
        let n = match self.decoder.decode_float(opus, &mut self.pcm48, false) {
            Ok(n) => n,
            Err(e) => {
                warn!(error = %e, "an opus frame did not decode; heard as silence");
                960
            }
        };
        self.pcm48[..n]
            .iter_mut()
            .for_each(|s| *s = s.clamp(-1.0, 1.0));
        self.down.push(&self.pcm48[..n], &mut self.pcm16);
        self.next_rtp = rtp_time + n as u64;
        self.seg.push(&self.pcm16)
    }
}

/// The transcript thread, as the call thread sees it.
pub struct Transcripts {
    tx: mpsc::Sender<In>,
    lines: mpsc::Receiver<TranscriptLine>,
}

impl Transcripts {
    /// Start the transcript thread with `engine` (opened and warmed up there).
    pub fn spawn(engine: Engine) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let (line_tx, lines) = mpsc::channel();
        thread::Builder::new()
            .name("transcript".into())
            .spawn(move || run(&engine, &rx, &line_tx))
            .map_err(|e| format!("transcript thread: {e}"))?;
        Ok(Transcripts { tx, lines })
    }

    /// One Opus frame of participant `who` (named `name`) while `take`
    /// records.
    pub fn audio(
        &self,
        who: u64,
        name: &str,
        take: TakeId,
        rtp_time: u64,
        opus: &[u8],
        host_us: i64,
    ) {
        let _ = self.tx.send(In::Audio {
            who,
            name: name.to_string(),
            take,
            rtp_time,
            opus: opus.to_vec(),
            host_us,
        });
    }

    /// Participant `who` left.
    pub fn gone(&self, who: u64) {
        let _ = self.tx.send(In::Gone(who));
    }

    /// The take stopped.
    pub fn flush(&self) {
        let _ = self.tx.send(In::Flush);
    }

    /// The lines transcribed since the last call.
    pub fn lines(&self) -> Vec<TranscriptLine> {
        self.lines.try_iter().collect()
    }
}

/// An utterance waiting for the engine.
struct Pending {
    who: u64,
    name: String,
    take: TakeId,
    host_us: i64,
    pcm: Vec<f32>,
}

fn run(engine: &Engine, rx: &mpsc::Receiver<In>, out: &mpsc::Sender<TranscriptLine>) {
    let mut t = match engine.open() {
        Ok(t) => t,
        Err(e) => {
            warn!(error = %e, "no live transcript");
            return;
        }
    };
    if let Err(e) = t.warm_up() {
        warn!(error = %e, "the transcription engine did not warm up");
    }
    info!(?engine, "live transcript ready");
    let mut speakers: BTreeMap<u64, Speaker> = BTreeMap::new();
    let mut queue: std::collections::VecDeque<Pending> = Default::default();
    loop {
        // Take everything waiting; block only when there is nothing to do.
        let first = if queue.is_empty() {
            match rx.recv() {
                Ok(m) => Some(m),
                Err(_) => return,
            }
        } else {
            match rx.recv_timeout(Duration::ZERO) {
                Ok(m) => Some(m),
                Err(mpsc::RecvTimeoutError::Timeout) => None,
                Err(mpsc::RecvTimeoutError::Disconnected) => return,
            }
        };
        for m in first.into_iter().chain(rx.try_iter()) {
            take_in(m, &mut speakers, &mut queue);
        }
        if let Some(p) = queue.pop_front() {
            match t.transcribe(&p.pcm) {
                Ok(segs) => {
                    for s in segs {
                        let line = TranscriptLine {
                            participant: ParticipantId(p.who),
                            name: p.name.clone(),
                            take: p.take,
                            host_us: p.host_us + s.start_ms as i64 * 1000,
                            text: s.text,
                        };
                        if out.send(line).is_err() {
                            return;
                        }
                    }
                }
                Err(e) => warn!(error = %e, "an utterance was not transcribed"),
            }
        }
    }
}

fn finish(who: u64, s: &mut Speaker, queue: &mut std::collections::VecDeque<Pending>) {
    if let Some(u) = s.seg.finish() {
        queue.push_back(pending(who, s, u));
    }
}

fn pending(who: u64, s: &Speaker, u: Utterance) -> Pending {
    Pending {
        who,
        name: s.name.clone(),
        take: s.take,
        host_us: s.host_us(u.start),
        pcm: u.pcm,
    }
}

fn take_in(
    m: In,
    speakers: &mut BTreeMap<u64, Speaker>,
    queue: &mut std::collections::VecDeque<Pending>,
) {
    match m {
        In::Audio {
            who,
            name,
            take,
            rtp_time,
            opus,
            host_us,
        } => {
            // A new take, a long gap or a time going back starts afresh.
            let fresh = speakers.get(&who).is_none_or(|s| {
                s.take != take || rtp_time > s.next_rtp + MAX_GAP || rtp_time + MAX_GAP < s.next_rtp
            });
            if fresh {
                if let Some(mut old) = speakers.remove(&who) {
                    finish(who, &mut old, queue);
                }
                match Speaker::new(name, take, rtp_time, host_us) {
                    Ok(s) => {
                        speakers.insert(who, s);
                    }
                    Err(e) => {
                        warn!(error = %e, "no transcript for a participant");
                        return;
                    }
                }
            }
            let s = speakers.get_mut(&who).expect("just made");
            for u in s.frame(rtp_time, &opus) {
                queue.push_back(pending(who, s, u));
            }
        }
        In::Gone(who) => {
            if let Some(mut s) = speakers.remove(&who) {
                finish(who, &mut s, queue);
            }
        }
        In::Flush => {
            for (who, mut s) in std::mem::take(speakers) {
                finish(who, &mut s, queue);
            }
        }
    }
}

/// The transcript saved in the call folder `dir`, oldest first; lines that
/// do not parse are skipped.
pub fn load(dir: &Path) -> Vec<TranscriptLine> {
    let Ok(text) = std::fs::read_to_string(dir.join(FILE)) else {
        return Vec::new();
    };
    text.lines().filter_map(from_json).collect()
}

/// Append `lines` to the transcript in the call folder `dir`.
pub fn append(dir: &Path, lines: &[TranscriptLine]) -> std::io::Result<()> {
    let mut f = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(dir.join(FILE))?;
    let mut text = String::new();
    for l in lines {
        text.push_str(&to_json(l));
        text.push('\n');
    }
    f.write_all(text.as_bytes())
}

fn to_json(l: &TranscriptLine) -> String {
    serde_json::json!({
        "participant": l.participant.0,
        "name": l.name,
        "take": l.take.0,
        "host_us": l.host_us,
        "text": l.text,
    })
    .to_string()
}

fn from_json(text: &str) -> Option<TranscriptLine> {
    let v: serde_json::Value = serde_json::from_str(text).ok()?;
    Some(TranscriptLine {
        participant: ParticipantId(v["participant"].as_u64()?),
        name: v["name"].as_str()?.to_string(),
        take: TakeId(u32::try_from(v["take"].as_u64()?).ok()?),
        host_us: v["host_us"].as_i64()?,
        text: v["text"].as_str()?.to_string(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_port_transcribe::sim::speak;

    /// `text` in the simulation's tone code, at 48 kHz, as 20 ms Opus frames.
    fn frames(text: &str) -> Vec<Vec<u8>> {
        let mut pcm16 = speak(text);
        pcm16.extend(vec![0.0; 16_000]);
        let pcm48: Vec<f32> = pcm16
            .windows(2)
            .flat_map(|w| {
                let (a, b) = (w[0], w[1]);
                [a, a + (b - a) / 3.0, a + 2.0 * (b - a) / 3.0]
            })
            .collect();
        let mut enc =
            opus::Encoder::new(48_000, opus::Channels::Mono, opus::Application::Audio).unwrap();
        enc.set_bitrate(opus::Bitrate::Bits(96_000)).unwrap();
        pcm48
            .as_chunks::<960>()
            .0
            .iter()
            .map(|f| enc.encode_vec_float(f, 4000).unwrap())
            .collect()
    }

    fn wait_lines(t: &Transcripts, n: usize) -> Vec<TranscriptLine> {
        let mut got = Vec::new();
        let end = std::time::Instant::now() + Duration::from_secs(20);
        while got.len() < n {
            assert!(std::time::Instant::now() < end, "only {got:?}");
            got.extend(t.lines());
            thread::sleep(Duration::from_millis(5));
        }
        got
    }

    #[test]
    fn two_speakers_are_heard_through_opus_with_their_times() {
        let t = Transcripts::spawn(Engine::Simulated).unwrap();
        let take = TakeId(2);
        // Bea starts at host time 10 s with RTP 5000; a lost frame is
        // concealed. Cy speaks from 11 s.
        for (i, f) in frames("hello there").iter().enumerate() {
            if i == 20 {
                continue;
            }
            t.audio(
                7,
                "Bea",
                take,
                5_000 + i as u64 * 960,
                f,
                10_000_000 + i as i64 * 20_000,
            );
        }
        for (i, f) in frames("zebra").iter().enumerate() {
            t.audio(
                9,
                "Cy",
                take,
                77 + i as u64 * 960,
                f,
                11_000_000 + i as i64 * 20_000,
            );
        }
        let mut got = wait_lines(&t, 2);
        got.sort_by_key(|l| l.participant);
        assert_eq!(got[0].text, "hello there");
        assert_eq!((got[0].participant.0, got[0].name.as_str()), (7, "Bea"));
        assert_eq!(got[0].take, take);
        assert!(got[0].host_us.abs_diff(10_000_000) < 30_000, "{got:?}");
        assert_eq!(got[1].text, "zebra");
        assert!(got[1].host_us.abs_diff(11_000_000) < 30_000, "{got:?}");
    }

    #[test]
    fn a_stopped_take_or_a_leaver_finishes_what_was_being_said() {
        let t = Transcripts::spawn(Engine::Simulated).unwrap();
        let f = frames("hey you");
        // Only the speech, no pause after it: nothing until the flush.
        let speech = f.len() - 45;
        for (i, fr) in f.iter().take(speech).enumerate() {
            t.audio(1, "Al", TakeId(1), i as u64 * 960, fr, 0);
        }
        thread::sleep(Duration::from_millis(100));
        assert!(t.lines().is_empty());
        t.flush();
        assert_eq!(wait_lines(&t, 1)[0].text, "hey you");
        for (i, fr) in f.iter().take(speech).enumerate() {
            t.audio(2, "Bo", TakeId(1), i as u64 * 960, fr, 0);
        }
        t.gone(2);
        assert_eq!(wait_lines(&t, 1)[0].name, "Bo");
    }

    #[test]
    fn the_transcript_file_round_trips_and_skips_bad_lines() {
        let dir =
            std::env::temp_dir().join(format!("castcorder-transcript-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let line = |n: u64| TranscriptLine {
            participant: ParticipantId(n),
            name: format!("P\"{n}"),
            take: TakeId(1),
            host_us: -5,
            text: "a \"quoted\"\nline".into(),
        };
        append(&dir, &[line(1)]).unwrap();
        std::fs::OpenOptions::new()
            .append(true)
            .open(dir.join(FILE))
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        append(&dir, &[line(2)]).unwrap();
        assert_eq!(load(&dir), vec![line(1), line(2)]);
        assert!(load(&dir.join("none")).is_empty());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_missing_engine_leaves_the_call_untranscribed() {
        let t = Transcripts::spawn(Engine::Whisper(PathBuf::from("/no/such/model.bin"))).unwrap();
        for (i, f) in frames("hi").iter().enumerate() {
            t.audio(1, "Al", TakeId(1), i as u64 * 960, f, 0);
        }
        t.flush();
        thread::sleep(Duration::from_millis(200));
        assert!(t.lines().is_empty());
    }
}
