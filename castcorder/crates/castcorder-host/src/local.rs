//! The host's own microphone tracks as items of the take manifest, and what
//! happens to them after a take: FLAC conversion, the WAV policy, and
//! recovery of a take the host never finalised (Ctrl+C, a crash).
//!
//! A host track is recorded like a guest's item, under the host's own device
//! id: each WAV segment `takeNN_<who>_<source>[-segN].wav` is an item named
//! after its file (format `wav`), the source being `mic` or a backup
//! microphone's `mic-N` (as a browser guest's backups), and each source's
//! timing file `takeNN_<who>_<source>.timing.jsonl` is the item
//! `takeNN_<who>_<source>-timing`
//! (format `timing.jsonl`), so the take checklist and alignment see every
//! track alike. The verifying of a host item is reading the whole file at
//! finalize (its size and BLAKE3 root become the entry's). The FLAC beside a
//! WAV is derived and not an item.
//!
//! WAV policy: both files are kept by default. With `keep_wav` off, a WAV is
//! deleted only after its FLAC was read back from disk and matched sample
//! for sample, and only after its manifest item is recorded; alignment then
//! reads the FLAC (castcorder-recorder `read_track`).

use std::io;

use castcorder_core::library::{Library, LocalItem, hash_local_item};
use castcorder_core::timing::{self, SUFFIX, TimingLine};
use castcorder_port_disk::{Storage, StorageFile};
use castcorder_protocol::TakeId;
use castcorder_recorder::{FlacError, flac_name, recover, verify_flac_of_wav, wav_to_flac};
use tracing::{error, info, warn};

/// The source id of the host's microphone items (a backup's is `mic-N`).
pub const SOURCE: &str = "mic";

/// Whether `s` is a microphone source id: `mic`, or a backup's `mic-N`.
fn is_mic_source(s: &str) -> bool {
    s == SOURCE
        || s.strip_prefix("mic-")
            .is_some_and(|n| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit()))
}

/// `(who, source)` of a track's base name `takeNN_<who>_<source>`.
fn who_and_source(base: &str) -> Option<(&str, &str)> {
    let (front, source) = base.rsplit_once('_')?;
    let (_, who) = front.split_once('_')?;
    is_mic_source(source).then_some((who, source))
}

fn stem(file: &str) -> &str {
    file.trim_end_matches(".wav")
}

/// `(base, segment)` of a stem: `x_mic-seg3` is `("x_mic", 3)`, `x_mic` is
/// `("x_mic", 1)`.
fn split_segment(stem: &str) -> (&str, u32) {
    if let Some((base, n)) = stem.rsplit_once("-seg")
        && let Ok(n) = n.parse::<u32>()
        && !base.is_empty()
    {
        return (base, n);
    }
    (stem, 1)
}

/// Whether `name` looks like a host microphone segment of take `take`.
fn is_host_mic_wav(name: &str, take: u32) -> bool {
    let Some(s) = name.strip_suffix(".wav") else {
        return false;
    };
    let (base, _) = split_segment(s);
    base.starts_with(&format!("take{take:02}_")) && who_and_source(base).is_some()
}

/// Manifest items for a take's finished host files: each WAV, and the timing
/// file (named after the first WAV's track). Hashes every file.
pub fn items<S: Storage>(
    dir: &S,
    wavs: &[String],
    timing_file: Option<&str>,
) -> io::Result<Vec<LocalItem>> {
    let mut out = Vec::new();
    for w in wavs {
        out.push(hash_local_item(dir, stem(w), w, "wav")?);
    }
    if let (Some(t), Some(first)) = (timing_file, wavs.first()) {
        let base = split_segment(stem(first)).0;
        out.push(hash_local_item(
            dir,
            &format!("{base}-timing"),
            t,
            "timing.jsonl",
        )?);
    }
    Ok(out)
}

/// The participant name and source in a host track's file name:
/// `takeNN_<who>_<source>...`.
fn who_of(wav: &str) -> (String, String) {
    let (base, _) = split_segment(stem(wav));
    let (who, source) = who_and_source(base).unwrap_or(("host", SOURCE));
    (who.to_string(), source.to_string())
}

/// Convert each WAV to FLAC beside it and read the FLAC back from disk to
/// verify it. Returns the WAVs whose FLAC is verified (float tracks and
/// failures stay WAV-only, logged).
pub fn convert_verified<S: Storage>(dir: &S, wavs: &[String]) -> Vec<String> {
    let mut verified = Vec::new();
    for wav in wavs {
        let flac = flac_name(wav);
        match wav_to_flac(dir, wav, &flac) {
            Ok(r) => match verify_flac_of_wav(dir, wav, &flac) {
                Ok(()) => {
                    info!(wav = %wav, flac = %flac, frames = r.frames, bytes = r.flac_bytes, "host microphone track converted to FLAC and verified");
                    verified.push(wav.clone());
                }
                Err(e) => {
                    error!(wav = %wav, flac = %flac, error = %e, "FLAC did not verify; the WAV is kept")
                }
            },
            Err(FlacError::NotApplicable(m)) => info!(wav = %wav, reason = %m, "kept as WAV"),
            Err(e) => error!(wav = %wav, error = %e, "FLAC conversion failed; the WAV is kept"),
        }
    }
    verified
}

/// Delete WAVs whose FLAC is verified (`--keep-wav false`), logging each.
pub fn delete_wavs<S: Storage>(dir: &S, verified: &[String]) {
    for wav in verified {
        match dir.remove(wav).and_then(|()| dir.sync_dir()) {
            Ok(()) => {
                info!(wav = %wav, flac = %flac_name(wav), "WAV deleted: its FLAC is verified and --keep-wav is false")
            }
            Err(e) => warn!(wav = %wav, error = %e, "WAV not deleted"),
        }
    }
}

/// What startup recovery did for one take.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Recovered {
    /// The take.
    pub take: TakeId,
    /// The WAVs adopted into the manifest.
    pub wavs: Vec<String>,
}

/// Make the timing file of a recovered take whole: cut a torn last line, and
/// end it (`end` line) if the recorder never did. `Ok(false)`: unusable.
fn finish_timing<S: Storage>(dir: &S, file: &str, last_frames: u64) -> io::Result<bool> {
    let mut f = dir.open(file)?;
    let mut buf = vec![0u8; f.len()? as usize];
    read_whole(&mut f, &mut buf)?;
    let whole = buf.iter().rposition(|b| *b == b'\n').map_or(0, |i| i + 1);
    let text = String::from_utf8_lossy(&buf[..whole]).into_owned();
    let Ok(lines) = timing::parse(&text) else {
        return Ok(false);
    };
    if !lines.iter().any(|l| matches!(l, TimingLine::Header(_))) {
        return Ok(false);
    }
    let mut len = whole as u64;
    if whole != buf.len() {
        f.set_len(len)?;
    }
    if !lines.iter().any(|l| matches!(l, TimingLine::End { .. })) {
        let line = TimingLine::End {
            frames: last_frames,
        }
        .to_line();
        f.write_at(len, line.as_bytes())?;
        len += line.len() as u64;
        let _ = len;
    }
    f.sync()?;
    Ok(true)
}

fn read_whole<F: StorageFile>(f: &mut F, buf: &mut [u8]) -> io::Result<()> {
    let mut done = 0;
    while done < buf.len() {
        let n = f.read_at(done as u64, &mut buf[done..])?;
        if n == 0 {
            return Err(io::ErrorKind::UnexpectedEof.into());
        }
        done += n;
    }
    Ok(())
}

/// On host startup: for each take in `takes`, find host microphone WAVs no
/// manifest item claims (left unfinalised by Ctrl+C or a crash, or never
/// recorded), repair them with `recover()`, finish their timing file, record
/// their manifest items under `device`, and convert to FLAC (and apply the
/// WAV policy). Every recovery is logged.
pub fn recover_takes<S: Storage + Clone>(
    lib: &mut Library<S>,
    takes: &[u32],
    device: &str,
    keep_wav: bool,
) -> Vec<Recovered> {
    let mut out = Vec::new();
    for &n in takes {
        let take = TakeId(n);
        let (dir, claimed) = match lib.take_dir(take).cloned().and_then(|d| {
            lib.entries(take)
                .map(|e| (d, e.into_iter().map(|e| e.file).collect::<Vec<_>>()))
        }) {
            Ok(x) => x,
            Err(e) => {
                warn!(take = n, error = %e, "take not scanned for host microphone tracks");
                continue;
            }
        };
        let mut wavs: Vec<String> = dir
            .list()
            .unwrap_or_default()
            .into_iter()
            .filter(|f| is_host_mic_wav(f, n) && !claimed.contains(f))
            .collect();
        wavs.sort_by_key(|w| {
            let (b, s) = split_segment(stem(w));
            (b.to_string(), s)
        });
        // One track per base name (segments share it).
        let mut i = 0;
        while i < wavs.len() {
            let base = split_segment(stem(&wavs[i])).0.to_string();
            let group: Vec<String> = wavs[i..]
                .iter()
                .take_while(|w| split_segment(stem(w)).0 == base)
                .cloned()
                .collect();
            i += group.len();
            if let Some(r) = recover_track(lib, &dir, take, device, keep_wav, &base, &group) {
                out.push(r);
            }
        }
    }
    out
}

fn recover_track<S: Storage + Clone>(
    lib: &mut Library<S>,
    dir: &S,
    take: TakeId,
    device: &str,
    keep_wav: bool,
    base: &str,
    group: &[String],
) -> Option<Recovered> {
    let mut last_frames = 0;
    for w in group {
        match dir.open(w).and_then(|mut f| recover(&mut f)) {
            Ok(r) => {
                last_frames = r.frames_recovered;
                if r.repaired {
                    warn!(take = take.0, wav = %w, header_frames = r.frames_header_before,
                        frames = r.frames_recovered, dropped_bytes = r.dropped_partial_bytes,
                        "host microphone track was not finalised; recovered");
                } else {
                    info!(take = take.0, wav = %w, frames = r.frames_recovered,
                        "host microphone track was whole but not in the manifest; adopted");
                }
            }
            Err(e) => {
                error!(take = take.0, wav = %w, error = %e, "host microphone track cannot be recovered; left as is");
                return None;
            }
        }
    }
    let timing_file = format!("{base}{SUFFIX}");
    let timing_file = match dir.exists(&timing_file) {
        Ok(true) => match finish_timing(dir, &timing_file, last_frames) {
            Ok(true) => Some(timing_file),
            Ok(false) => {
                warn!(take = take.0, file = %timing_file, "timing metadata unusable; track recorded without it (it will not be aligned)");
                None
            }
            Err(e) => {
                warn!(take = take.0, file = %timing_file, error = %e, "timing metadata not finished");
                None
            }
        },
        _ => None,
    };
    let (who, source) = who_of(&group[0]);
    let recorded = items(dir, group, timing_file.as_deref()).and_then(|items| {
        for item in &items {
            lib.record_local(device, &who, take, &source, item)?;
        }
        Ok(())
    });
    if let Err(e) = recorded {
        error!(take = take.0, track = %base, error = %e, "recovered host track not recorded in the manifest");
        return None;
    }
    let verified = convert_verified(dir, group);
    if !keep_wav {
        delete_wavs(dir, &verified);
    }
    Some(Recovered {
        take,
        wavs: group.to_vec(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use castcorder_core::library::Library;
    use castcorder_core::sync::SyncConfig;
    use castcorder_port_capture::{AudioFormat, SampleFormat};
    use castcorder_port_disk::FsStorage;
    use castcorder_recorder::{WavWriter, WriterOptions};

    const DEVICE: &str = "host-test";

    fn temp(name: &str) -> FsStorage {
        let root =
            std::env::temp_dir().join(format!("castcorder-local-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        FsStorage::new(root).unwrap()
    }

    fn fmt(sample_format: SampleFormat) -> AudioFormat {
        AudioFormat {
            sample_rate: 48_000,
            channels: 1,
            sample_format,
        }
    }

    /// A WAV of `frames` frames (a ramp), finalised, in `dir`.
    fn wav(dir: &FsStorage, name: &str, f: AudioFormat, frames: u64) {
        let mut w =
            WavWriter::create(dir.open(name).unwrap(), f, WriterOptions::default()).unwrap();
        let bytes = f.sample_format.bytes();
        let mut data = Vec::new();
        for i in 0..frames {
            let s = ((i % 200) as i32 - 100) * 100;
            match f.sample_format {
                SampleFormat::I16 => data.extend_from_slice(&(s as i16).to_le_bytes()),
                SampleFormat::I24 => data.extend_from_slice(&(s << 8).to_le_bytes()[1..4]),
                SampleFormat::F32 => data.extend_from_slice(&(s as f32 / 32768.0).to_le_bytes()),
            }
        }
        assert_eq!(data.len() as u64, frames * bytes as u64);
        w.write(&data).unwrap();
        w.finalize().unwrap();
    }

    /// The timing file a recorder leaves: header and segment, refs, no end.
    fn timing_file(dir: &FsStorage, name: &str, wavs: &[&str], end: Option<u64>) {
        let mut text = String::new();
        let mut push = |l: TimingLine| text.push_str(&l.to_line());
        push(TimingLine::Header(timing::Header {
            version: timing::VERSION,
            call: "call".into(),
            device: "host".into(),
            take: 1,
            track: wavs[0].into(),
            source: "mic".into(),
            sample_rate: 48_000,
            channels: 1,
            device_clock: "host".into(),
            host_clock: "host".into(),
            host_timeline: 1,
        }));
        for (i, w) in wavs.iter().enumerate() {
            push(TimingLine::Segment {
                segment: i as u32 + 1,
                file: (*w).into(),
            });
            push(TimingLine::Ref(timing::RefPoint::on_host(
                0,
                1_000_000 + i as i64 * 10_000_000,
            )));
        }
        if let Some(frames) = end {
            push(TimingLine::End { frames });
        }
        let mut f = dir.open(name).unwrap();
        f.write_at(0, text.as_bytes()).unwrap();
        f.sync().unwrap();
    }

    fn library(root: &FsStorage) -> Library<FsStorage> {
        Library::open(root, "call", SyncConfig::default()).unwrap()
    }

    #[test]
    fn host_items_are_verified_manifest_entries_that_align_sees() {
        let root = temp("items");
        let mut lib = library(&root);
        let dir = lib.take_dir(TakeId(1)).unwrap().clone();
        wav(&dir, "take01_host_mic.wav", fmt(SampleFormat::I16), 48_000);
        wav(
            &dir,
            "take01_host_mic-seg2.wav",
            fmt(SampleFormat::I16),
            24_000,
        );
        timing_file(
            &dir,
            "take01_host_mic.timing.jsonl",
            &["take01_host_mic.wav", "take01_host_mic-seg2.wav"],
            Some(24_000),
        );
        let wavs = vec![
            "take01_host_mic.wav".to_string(),
            "take01_host_mic-seg2.wav".to_string(),
        ];
        let its = items(&dir, &wavs, Some("take01_host_mic.timing.jsonl")).unwrap();
        for item in &its {
            lib.record_local(DEVICE, "host", TakeId(1), SOURCE, item)
                .unwrap();
        }
        let entries = lib.entries(TakeId(1)).unwrap();
        assert_eq!(entries.len(), 3, "{entries:?}");
        assert!(
            entries
                .iter()
                .all(|e| e.verified && e.offer.device == DEVICE)
        );
        let names: Vec<_> = entries.iter().map(|e| e.offer.name.as_str()).collect();
        assert!(names.contains(&"take01_host_mic-timing"), "{names:?}");
        assert!(
            castcorder_align::ready(&entries),
            "the take checklist is complete"
        );
        // Recording again changes nothing; a changed file is refused.
        lib.record_local(DEVICE, "host", TakeId(1), SOURCE, &its[0])
            .unwrap();
        assert_eq!(lib.entries(TakeId(1)).unwrap().len(), 3);
        let mut changed = its[0].clone();
        changed.root[0] ^= 1;
        assert!(
            lib.record_local(DEVICE, "host", TakeId(1), SOURCE, &changed)
                .is_err()
        );
        // Alignment finds both segments through the manifest.
        let summary = castcorder_align::align_take(&dir, &castcorder_align::AlignConfig::default())
            .expect("aligned");
        assert_eq!(summary.tracks.len(), 1, "{summary:?}");
        assert_eq!(summary.tracks[0].segments.len(), 2);
        let _ = std::fs::remove_dir_all(root.root());
    }

    #[test]
    fn startup_recovers_a_truncated_host_wav_and_adopts_it() {
        let root = temp("recover");
        let mut lib = library(&root);
        let dir = lib.take_dir(TakeId(1)).unwrap().clone();
        // A WAV the recorder never finalised: header says 0 frames, data
        // on disk, a torn final frame; the timing file has no end line and
        // a torn last line.
        wav(&dir, "take01_host_mic.wav", fmt(SampleFormat::I16), 48_000);
        {
            let mut f = dir.open("take01_host_mic.wav").unwrap();
            let mut hdr = vec![0u8; 80];
            f.read_at(0, &mut hdr).unwrap();
            // Stale sizes, as written by the first header write.
            let len = f.len().unwrap();
            f.set_len(len + 1).unwrap(); // a torn half frame
            f.write_at(len, &[7]).unwrap();
            f.write_at(0, &castcorder_stale_header(&hdr)).unwrap();
        }
        timing_file(
            &dir,
            "take01_host_mic.timing.jsonl",
            &["take01_host_mic.wav"],
            None,
        );
        {
            let mut f = dir.open("take01_host_mic.timing.jsonl").unwrap();
            let len = f.len().unwrap();
            f.write_at(len, b"{\"type\":\"ref\",\"fra").unwrap();
        }
        let got = recover_takes(&mut lib, &[1], DEVICE, true);
        assert_eq!(
            got,
            vec![Recovered {
                take: TakeId(1),
                wavs: vec!["take01_host_mic.wav".into()]
            }]
        );
        let mut f = dir.open("take01_host_mic.wav").unwrap();
        let info = castcorder_recorder::read_info(&mut f).unwrap();
        assert_eq!(info.data_len_header, info.data_len_on_disk());
        assert_eq!(
            info.data_len_header,
            48_000 * 2,
            "torn frame dropped, rest kept"
        );
        let entries = lib.entries(TakeId(1)).unwrap();
        assert_eq!(entries.len(), 2, "{entries:?}");
        assert!(entries.iter().all(|e| e.verified));
        assert!(castcorder_align::ready(&entries));
        assert!(dir.exists("take01_host_mic.flac").unwrap(), "converted");
        assert!(
            dir.exists("take01_host_mic.wav").unwrap(),
            "WAV kept by default"
        );
        let text =
            std::fs::read_to_string(root.root().join("call/take01/take01_host_mic.timing.jsonl"))
                .unwrap();
        let lines = timing::parse(&text).expect("timing parses again");
        assert!(
            matches!(lines.last(), Some(TimingLine::End { frames: 48_000 })),
            "{lines:?}"
        );
        // A second startup finds nothing to do.
        assert!(recover_takes(&mut lib, &[1], DEVICE, true).is_empty());
        let _ = std::fs::remove_dir_all(root.root());
    }

    #[test]
    fn track_names_give_who_and_a_microphone_source() {
        assert_eq!(who_of("take01_host_mic.wav"), ("host".into(), "mic".into()));
        assert_eq!(
            who_of("take01_Pat-2_mic-3-seg2.wav"),
            ("Pat-2".into(), "mic-3".into())
        );
        assert!(is_host_mic_wav("take01_host_mic-2.wav", 1));
        assert!(is_host_mic_wav("take01_host_mic-2-seg4.wav", 1));
        for no in [
            "take02_host_mic-2.wav",
            "take01_host_mic-.wav",
            "take01_host_mic-x.wav",
            "take01_host_camera.wav",
            "take01_host_mic-2.flac",
        ] {
            assert!(!is_host_mic_wav(no, 1), "{no}");
        }
    }

    /// A backup microphone's track left unfinalised is recovered as a track
    /// of its own (source `mic-2`), beside the main microphone's.
    #[test]
    fn startup_recovers_a_backup_track_under_its_own_source() {
        let root = temp("recover-backup");
        let mut lib = library(&root);
        let dir = lib.take_dir(TakeId(1)).unwrap().clone();
        wav(&dir, "take01_host_mic.wav", fmt(SampleFormat::I16), 4_800);
        timing_file(
            &dir,
            "take01_host_mic.timing.jsonl",
            &["take01_host_mic.wav"],
            Some(4_800),
        );
        wav(&dir, "take01_host_mic-2.wav", fmt(SampleFormat::I16), 4_800);
        wav(
            &dir,
            "take01_host_mic-2-seg2.wav",
            fmt(SampleFormat::I16),
            2_400,
        );
        timing_file(
            &dir,
            "take01_host_mic-2.timing.jsonl",
            &["take01_host_mic-2.wav", "take01_host_mic-2-seg2.wav"],
            None,
        );
        let got = recover_takes(&mut lib, &[1], DEVICE, true);
        assert_eq!(
            got,
            vec![
                Recovered {
                    take: TakeId(1),
                    wavs: vec!["take01_host_mic.wav".into()]
                },
                Recovered {
                    take: TakeId(1),
                    wavs: vec![
                        "take01_host_mic-2.wav".into(),
                        "take01_host_mic-2-seg2.wav".into()
                    ]
                },
            ]
        );
        let entries = lib.entries(TakeId(1)).unwrap();
        let mut got: Vec<(String, String)> = entries
            .iter()
            .map(|e| (e.offer.source.0.clone(), e.offer.name.clone()))
            .collect();
        got.sort();
        assert_eq!(
            got,
            [
                ("mic", "take01_host_mic"),
                ("mic", "take01_host_mic-timing"),
                ("mic-2", "take01_host_mic-2"),
                ("mic-2", "take01_host_mic-2-seg2"),
                ("mic-2", "take01_host_mic-2-timing"),
            ]
            .map(|(s, n)| (s.to_string(), n.to_string()))
        );
        assert!(entries.iter().all(|e| e.participant == "host"));
        assert!(castcorder_align::ready(&entries));
        assert!(dir.exists("take01_host_mic-2.flac").unwrap(), "converted");
        let _ = std::fs::remove_dir_all(root.root());
    }

    /// The header the writer leaves before its first rewrite: sizes of an
    /// empty data chunk (all other bytes as written).
    fn castcorder_stale_header(good: &[u8]) -> Vec<u8> {
        let mut h = good.to_vec();
        // data chunk size is the last 4 bytes of the 80-byte header.
        h[76..80].copy_from_slice(&0u32.to_le_bytes());
        h
    }

    #[test]
    fn keep_wav_false_deletes_only_after_a_verified_flac() {
        // Integer track: FLAC verified, WAV deleted, alignment still works.
        let root = temp("keep");
        let mut lib = library(&root);
        let dir = lib.take_dir(TakeId(1)).unwrap().clone();
        wav(&dir, "take01_host_mic.wav", fmt(SampleFormat::I24), 48_000);
        timing_file(
            &dir,
            "take01_host_mic.timing.jsonl",
            &["take01_host_mic.wav"],
            None,
        );
        let got = recover_takes(&mut lib, &[1], DEVICE, false);
        assert_eq!(got.len(), 1);
        assert!(dir.exists("take01_host_mic.flac").unwrap());
        assert!(
            !dir.exists("take01_host_mic.wav").unwrap(),
            "WAV deleted after verified FLAC"
        );
        let entries = lib.entries(TakeId(1)).unwrap();
        assert!(castcorder_align::ready(&entries));
        let s = castcorder_align::align_take(&dir, &castcorder_align::AlignConfig::default())
            .expect("aligned from the FLAC");
        assert_eq!(s.tracks.len(), 1);
        let _ = std::fs::remove_dir_all(root.root());

        // Float track: no FLAC is possible, so the WAV must stay.
        let root = temp("keep-f32");
        let mut lib = library(&root);
        let dir = lib.take_dir(TakeId(1)).unwrap().clone();
        wav(&dir, "take01_host_mic.wav", fmt(SampleFormat::F32), 48_000);
        timing_file(
            &dir,
            "take01_host_mic.timing.jsonl",
            &["take01_host_mic.wav"],
            None,
        );
        assert_eq!(recover_takes(&mut lib, &[1], DEVICE, false).len(), 1);
        assert!(!dir.exists("take01_host_mic.flac").unwrap());
        assert!(
            dir.exists("take01_host_mic.wav").unwrap(),
            "no verified FLAC, WAV kept"
        );
        let _ = std::fs::remove_dir_all(root.root());

        // A FLAC that does not match the WAV never authorises a delete.
        let root = temp("keep-bad");
        let dir = FsStorage::new(root.root().join("t")).unwrap();
        wav(&dir, "a.wav", fmt(SampleFormat::I16), 1000);
        wav(&dir, "b.wav", fmt(SampleFormat::I16), 1000);
        // b's samples differ from a's only if we alter them.
        {
            let mut f = dir.open("b.wav").unwrap();
            f.write_at(80 + 10, &[1, 2]).unwrap();
        }
        wav_to_flac(&dir, "a.wav", "x.flac").unwrap();
        assert!(verify_flac_of_wav(&dir, "b.wav", "x.flac").is_err());
        assert!(verify_flac_of_wav(&dir, "a.wav", "x.flac").is_ok());
        let _ = std::fs::remove_dir_all(root.root());
    }
}
