//! The live transcript (cast-7hm.2): what each participant says while a take
//! records, as text, for everyone in the call and in the call's folder.
//!
//! The call thread hands every participant's live Opus frames (guests' and
//! the host machine's own) to [`Transcripts`], which runs on its own thread:
//! per participant it decodes them (libopus), takes them to 16 kHz
//! ([`Downsampler`]), cuts them at pauses ([`Segmenter`]) and transcribes
//! each utterance with the engine. The call thread collects the lines with
//! [`Transcripts::heard`], appends the lines to `<call>/transcript.jsonl`
//! and sends them to everyone ([`castcorder_protocol::Event::Transcript`]).
//!
//! Near-realtime (cast-veeq): while someone is still talking, and the engine
//! has nothing finished to do, it transcribes what they have said so far
//! and sends that as a draft ([`castcorder_protocol::Event::TranscriptDraft`])
//! that their finished line replaces. Drafts come about once a second of new
//! speech, less often when the engine is slower than that (audio waiting to
//! be read means it is behind), and finished lines always go first.
//!
//! Only audio sent while a take records is given to it, so nothing said
//! between takes is transcribed (guests are told when a take records).

use std::collections::{BTreeMap, VecDeque};
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
    /// `whisper` feature), or, for a folder of bundled models, the one that
    /// suits this machine (`castcorder_transcribe::load_best`: the GPU model
    /// when it runs on a GPU).
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
            Engine::Whisper(path) if path.is_dir() => {
                info!(models = %path.display(), "loading the transcription model");
                Ok(Box::new(castcorder_transcribe::load_best(path)?))
            }
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

/// A draft is made once this much more of an utterance (16 kHz samples)
/// has been said since the last one.
const DRAFT_STEP: usize = SAMPLE_RATE as usize;

/// A finished line with the same words as the speaker's previous one, this
/// soon after it (microseconds) and at least [`REPEAT_WORDS`] long, is the
/// engine repeating itself, not the speaker: it is dropped.
const REPEAT_WITHIN_US: i64 = 30_000_000;
/// See [`REPEAT_WITHIN_US`]; a short "yeah" said twice is real.
const REPEAT_WORDS: usize = 3;

/// What the transcript thread heard.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Heard {
    /// A finished line: file it and send it.
    Line(TranscriptLine),
    /// A draft of what someone is still saying (empty text: none any more):
    /// send it, never file it.
    Draft(TranscriptLine),
}

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
    /// How much of the open utterance the last draft covered.
    drafted: usize,
    /// A draft of theirs is showing (until their next finished line).
    shown: bool,
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
            drafted: 0,
            shown: false,
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
    heard: mpsc::Receiver<Heard>,
}

impl Transcripts {
    /// Start the transcript thread with `engine` (opened and warmed up there).
    pub fn spawn(engine: Engine) -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let (heard_tx, heard) = mpsc::channel();
        thread::Builder::new()
            .name("transcript".into())
            .spawn(move || run(&engine, &rx, &heard_tx))
            .map_err(|e| format!("transcript thread: {e}"))?;
        Ok(Transcripts { tx, heard })
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

    /// The lines and drafts transcribed since the last call, in the order
    /// they were made (a draft's line follows it).
    pub fn heard(&self) -> Vec<Heard> {
        self.heard.try_iter().collect()
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

fn run(engine: &Engine, rx: &mpsc::Receiver<In>, out: &mpsc::Sender<Heard>) {
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
    let mut queue: VecDeque<Pending> = Default::default();
    let mut last: BTreeMap<u64, (TakeId, i64, Vec<String>)> = BTreeMap::new();
    loop {
        // Take everything waiting; block only when there is nothing to do.
        let idle = queue.is_empty() && draft_due(&speakers).is_none();
        let first = if idle {
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
        let sent = if let Some(p) = queue.pop_front() {
            finished(&mut *t, p, &mut speakers, &mut last, out)
        } else if let Some(who) = draft_due(&speakers) {
            draft(&mut *t, who, &mut speakers, out)
        } else {
            true
        };
        if !sent {
            return;
        }
    }
}

/// Transcribe a finished utterance and send its lines; false: nobody is
/// listening any more.
fn finished(
    t: &mut dyn Transcriber,
    p: Pending,
    speakers: &mut BTreeMap<u64, Speaker>,
    last: &mut BTreeMap<u64, (TakeId, i64, Vec<String>)>,
    out: &mpsc::Sender<Heard>,
) -> bool {
    let segs = t.transcribe(&p.pcm).unwrap_or_else(|e| {
        warn!(error = %e, "an utterance was not transcribed");
        Vec::new()
    });
    let mut said = false;
    for s in segs {
        let host_us = p.host_us + s.start_ms as i64 * 1000;
        let words = castcorder_port_transcribe::words(&s.text);
        if let Some((take, at, prev)) = last.get(&p.who)
            && *take == p.take
            && host_us - at < REPEAT_WITHIN_US
            && words.len() >= REPEAT_WORDS
            && *prev == words
        {
            info!(participant = p.who, "a repeated line dropped");
            continue;
        }
        last.insert(p.who, (p.take, host_us, words));
        let line = TranscriptLine {
            participant: ParticipantId(p.who),
            name: p.name.clone(),
            take: p.take,
            host_us,
            text: s.text,
        };
        said = true;
        if out.send(Heard::Line(line)).is_err() {
            return false;
        }
    }
    // Any draft shown was of this utterance (drafts wait for finished
    // work): a line replaces it, and with no line it must be taken back.
    let drafted = speakers
        .get_mut(&p.who)
        .map(|s| std::mem::take(&mut s.shown))
        .unwrap_or(true);
    if !said && drafted {
        let none = TranscriptLine {
            participant: ParticipantId(p.who),
            name: p.name,
            take: p.take,
            host_us: p.host_us,
            text: String::new(),
        };
        return out.send(Heard::Draft(none)).is_ok();
    }
    true
}

/// Whose utterance in progress has grown the most since its last draft, if
/// any has grown by [`DRAFT_STEP`].
fn draft_due(speakers: &BTreeMap<u64, Speaker>) -> Option<u64> {
    speakers
        .iter()
        .filter_map(|(who, s)| {
            let (_, pcm) = s.seg.open()?;
            let grown = pcm.len().saturating_sub(s.drafted);
            (grown >= DRAFT_STEP).then_some((grown, *who))
        })
        .max()
        .map(|(_, who)| who)
}

/// Transcribe what `who` has said so far and send it as a draft; false:
/// nobody is listening any more.
fn draft(
    t: &mut dyn Transcriber,
    who: u64,
    speakers: &mut BTreeMap<u64, Speaker>,
    out: &mpsc::Sender<Heard>,
) -> bool {
    let Some(s) = speakers.get_mut(&who) else {
        return true;
    };
    let Some((start, pcm)) = s.seg.open() else {
        return true;
    };
    let len = pcm.len();
    let segs = match t.transcribe(pcm) {
        Ok(segs) => segs,
        Err(e) => {
            warn!(error = %e, "a draft was not transcribed");
            Vec::new()
        }
    };
    let host_us = s.host_us(start);
    s.drafted = len;
    let text = segs
        .iter()
        .map(|g| g.text.as_str())
        .collect::<Vec<_>>()
        .join(" ");
    if text.is_empty() {
        return true;
    }
    s.shown = true;
    let line = TranscriptLine {
        participant: ParticipantId(who),
        name: s.name.clone(),
        take: s.take,
        host_us,
        text,
    };
    out.send(Heard::Draft(line)).is_ok()
}

fn finish(who: u64, s: &mut Speaker, queue: &mut VecDeque<Pending>) {
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

fn take_in(m: In, speakers: &mut BTreeMap<u64, Speaker>, queue: &mut VecDeque<Pending>) {
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
                // The next utterance's drafts start afresh.
                s.drafted = 0;
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

    /// The finished lines, until there are `n`.
    fn wait_lines(t: &Transcripts, n: usize) -> Vec<TranscriptLine> {
        let mut got = Vec::new();
        let end = std::time::Instant::now() + Duration::from_secs(20);
        while got.len() < n {
            assert!(std::time::Instant::now() < end, "only {got:?}");
            got.extend(lines(t.heard()));
            thread::sleep(Duration::from_millis(5));
        }
        got
    }

    fn lines(heard: Vec<Heard>) -> Vec<TranscriptLine> {
        heard
            .into_iter()
            .filter_map(|h| match h {
                Heard::Line(l) => Some(l),
                Heard::Draft(_) => None,
            })
            .collect()
    }

    /// Everything heard until the first finished line, that line included.
    fn until_a_line(t: &Transcripts) -> Vec<Heard> {
        let mut got = Vec::new();
        let end = std::time::Instant::now() + Duration::from_secs(20);
        while !got.iter().any(|h| matches!(h, Heard::Line(_))) {
            assert!(std::time::Instant::now() < end, "only {got:?}");
            got.extend(t.heard());
            thread::sleep(Duration::from_millis(5));
        }
        got
    }

    fn send(t: &Transcripts, who: u64, frames: &[Vec<u8>], host_us: i64) {
        for (i, f) in frames.iter().enumerate() {
            t.audio(
                who,
                "Al",
                TakeId(1),
                i as u64 * 960,
                f,
                host_us + i as i64 * 20_000,
            );
        }
    }

    /// The next draft (a line before it fails the test).
    fn next_draft(t: &Transcripts) -> TranscriptLine {
        let end = std::time::Instant::now() + Duration::from_secs(20);
        loop {
            assert!(std::time::Instant::now() < end, "no draft");
            if let Some(h) = t.heard().into_iter().next() {
                match h {
                    Heard::Draft(l) => return l,
                    Heard::Line(l) => panic!("a line before the pause: {l:?}"),
                }
            }
            thread::sleep(Duration::from_millis(5));
        }
    }

    #[test]
    fn drafts_show_what_is_being_said_before_the_pause() {
        let t = Transcripts::spawn(Engine::Simulated).unwrap();
        let said = "the quick brown fox jumps";
        let f = frames(said);
        // The speech in two parts, as it is said, and none of the pause
        // after it: no line yet, only drafts of what was said so far, timed
        // from its start.
        let speech = f.len() - 45;
        let half = speech / 2;
        let mut sent = 0;
        let mut drafts = Vec::new();
        for upto in [half, speech] {
            for (i, fr) in f.iter().enumerate().take(upto).skip(sent) {
                t.audio(
                    1,
                    "Al",
                    TakeId(1),
                    i as u64 * 960,
                    fr,
                    5_000_000 + i as i64 * 20_000,
                );
            }
            sent = upto;
            drafts.push(next_draft(&t));
        }
        assert!(drafts[0].text.starts_with("the qu"), "{drafts:?}");
        assert!(drafts[1].text.starts_with("the quick brown"), "{drafts:?}");
        assert!(
            drafts
                .iter()
                .all(|d| d.host_us.abs_diff(5_000_000) < 30_000)
        );
        // The pause finishes the line; no draft comes after it.
        for (i, fr) in f.iter().enumerate().skip(sent) {
            t.audio(
                1,
                "Al",
                TakeId(1),
                i as u64 * 960,
                fr,
                5_000_000 + i as i64 * 20_000,
            );
        }
        let rest = until_a_line(&t);
        assert!(
            matches!(rest.last(), Some(Heard::Line(l)) if l.text == said),
            "{rest:?}"
        );
        thread::sleep(Duration::from_millis(100));
        assert!(t.heard().is_empty());
    }

    #[test]
    fn a_line_the_engine_repeats_is_dropped_and_its_draft_taken_back() {
        let t = Transcripts::spawn(Engine::Simulated).unwrap();
        let f = frames("one two three");
        send(&t, 1, &f, 0);
        assert_eq!(wait_lines(&t, 1)[0].text, "one two three");
        // The same words again, word for word, 4 s later: drafted while
        // said, then dropped as a repeat, and the draft taken back.
        let speech = f.len() - 45;
        send(&t, 1, &f[..speech], 4_000_000);
        assert_eq!(next_draft(&t).text, "one two three");
        t.flush();
        let end = std::time::Instant::now() + Duration::from_secs(20);
        let mut heard = Vec::new();
        while heard.is_empty() {
            assert!(std::time::Instant::now() < end, "nothing after the flush");
            heard.extend(t.heard());
            thread::sleep(Duration::from_millis(5));
        }
        assert!(
            matches!(heard.as_slice(), [Heard::Draft(l)] if l.text.is_empty()),
            "{heard:?}"
        );
        // A short line said twice is real, though.
        let yes = frames("yes");
        send(&t, 1, &yes, 8_000_000);
        send(&t, 1, &yes, 10_000_000);
        let got = wait_lines(&t, 2);
        assert_eq!(
            got.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(),
            ["yes", "yes"]
        );
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
        assert!(lines(t.heard()).is_empty());
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
        assert!(t.heard().is_empty());
    }
}
