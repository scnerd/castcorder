//! The recorder on the real code paths: simulated disk for crashes, real files for the
//! rest. No mocks.

use std::fs;
use std::path::PathBuf;

use castcorder_port_capture::{
    AudioFormat, CaptureSource, Generator, SampleFormat, WavSource, encode_wav,
};
use castcorder_port_disk::{FsStorage, SimDisk, Storage, StorageFile};
use castcorder_recorder::{
    DATA_OFFSET, FlacError, WavWriter, WriterOptions, pump, read_data, read_info, record, recover,
    recover_with, wav_to_flac,
};

fn fmt(sample_format: SampleFormat, channels: u16) -> AudioFormat {
    AudioFormat {
        sample_rate: 48_000,
        channels,
        sample_format,
    }
}

/// Everything a generator yields, as one byte vector.
fn generated(format: AudioFormat, frames: u64) -> Vec<u8> {
    let mut g = Generator::new(format, 700, Some(frames));
    let mut out = Vec::new();
    while let Some(b) = g.next_buffer().unwrap() {
        out.extend_from_slice(&b.data);
    }
    out
}

fn read_all<F: StorageFile>(f: &mut F) -> Vec<u8> {
    let mut v = vec![0u8; f.len().unwrap() as usize];
    let mut done = 0;
    while done < v.len() {
        let n = f.read_at(done as u64, &mut v[done..]).unwrap();
        assert!(n > 0);
        done += n;
    }
    v
}

fn temp_dir(name: &str) -> PathBuf {
    let dir =
        std::env::temp_dir().join(format!("castcorder-recorder-{}-{name}", std::process::id()));
    let _ = fs::remove_dir_all(&dir);
    dir
}

fn u32_at(b: &[u8], o: usize) -> u32 {
    u32::from_le_bytes([b[o], b[o + 1], b[o + 2], b[o + 3]])
}

/// The header is self-consistent with the file: RIFF and data sizes match its length.
fn assert_valid_plain_header(bytes: &[u8]) {
    assert_eq!(&bytes[0..4], b"RIFF");
    assert_eq!(u32_at(bytes, 4) as usize, bytes.len() - 8);
    assert_eq!(&bytes[72..76], b"data");
    assert_eq!(u32_at(bytes, 76) as usize, bytes.len() - 80);
}

#[test]
fn generator_data_chunk_is_byte_identical_for_every_format() {
    for sf in [SampleFormat::I16, SampleFormat::I24, SampleFormat::F32] {
        let format = fmt(sf, 2);
        let disk = SimDisk::new(1);
        let mut source = Generator::new(format, 500, Some(2345));
        let info = record(
            &mut source,
            disk.open("t.wav").unwrap(),
            WriterOptions::default(),
        )
        .unwrap();
        let mut f = disk.open("t.wav").unwrap();
        let (read_info, data) = read_data(&mut f).unwrap();
        assert_eq!(read_info, info);
        assert_eq!(info.format, format);
        assert_eq!(info.data_offset, DATA_OFFSET);
        assert_eq!(data, generated(format, 2345), "{sf:?}");
        assert_valid_plain_header(&read_all(&mut f));
    }
}

#[test]
fn wav_source_data_chunk_is_byte_identical() {
    for (sf, ch) in [
        (SampleFormat::I16, 2),
        (SampleFormat::I24, 1),
        (SampleFormat::F32, 2),
    ] {
        let format = fmt(sf, ch);
        let original = generated(format, 3001);
        let mut source = WavSource::from_bytes(encode_wav(format, &original), 333).unwrap();
        let disk = SimDisk::new(2);
        record(
            &mut source,
            disk.open("t.wav").unwrap(),
            WriterOptions::default(),
        )
        .unwrap();
        let (_, data) = read_data(&mut disk.open("t.wav").unwrap()).unwrap();
        assert_eq!(data, original, "{sf:?}");
    }
}

#[test]
fn header_is_rewritten_once_per_second_of_audio() {
    let format = fmt(SampleFormat::I16, 2);
    let disk = SimDisk::new(3);
    let mut w = WavWriter::create(
        disk.open("t.wav").unwrap(),
        format,
        WriterOptions::default(),
    )
    .unwrap();
    let mut source = Generator::new(format, 4_800, None);
    pump(&mut source, &mut w, Some(9)).unwrap(); // 0.9 s
    assert_eq!(w.header_writes(), 0);
    pump(&mut source, &mut w, Some(1)).unwrap(); // 1.0 s
    assert_eq!(w.header_writes(), 1);
    pump(&mut source, &mut w, Some(20)).unwrap(); // 3.0 s
    assert_eq!(w.header_writes(), 3);
    assert_eq!(w.frames(), 144_000);
    // The stale header is at most one interval behind.
    let info = read_info(&mut disk.open("t.wav").unwrap()).unwrap();
    assert_eq!(info.data_len_header, 144_000 * 4);
}

#[test]
fn crash_recovers_exactly_the_synced_data() {
    let format = fmt(SampleFormat::I24, 2);
    let reference = generated(format, 5_000);
    let fb = format.frame_bytes();
    let disk = SimDisk::new(7);
    let mut w = WavWriter::create(
        disk.open("t.wav").unwrap(),
        format,
        WriterOptions::default(),
    )
    .unwrap();
    // The track file must survive a crash: make its directory entry durable.
    disk.sync_dir().unwrap();
    let mut source = Generator::new(format, 1_000, Some(5_000));
    pump(&mut source, &mut w, Some(3)).unwrap();
    w.sync().unwrap();
    // Unsynced tail, then power loss.
    pump(&mut source, &mut w, Some(2)).unwrap();
    disk.crash();

    let mut f = disk.open("t.wav").unwrap();
    let report = recover(&mut f).unwrap();
    // The stale header claimed nothing; part of the unsynced tail may survive.
    assert_eq!(report.frames_header_before, 0);
    let (info, data) = read_data(&mut f).unwrap();
    assert_eq!(info.data_len_header as usize, data.len());
    assert_eq!(data.len() % fb, 0);
    assert!(data.len() >= 3_000 * fb && data.len() <= 5_000 * fb);
    // Only the synced prefix is guaranteed: a lost unsynced write can leave a zero gap
    // before a torn later one, so the unsynced tail is not compared.
    assert_eq!(data[..3_000 * fb], reference[..3_000 * fb]);
    assert_valid_plain_header(&read_all(&mut f));
}

#[test]
fn crash_right_after_sync_recovers_exactly_synced_length() {
    let format = fmt(SampleFormat::I16, 2);
    let reference = generated(format, 5_000);
    for seed in 0..10 {
        let disk = SimDisk::new(seed);
        let mut w = WavWriter::create(
            disk.open("t.wav").unwrap(),
            format,
            WriterOptions::default(),
        )
        .unwrap();
        disk.sync_dir().unwrap();
        let mut source = Generator::new(format, 1_000, Some(5_000));
        pump(&mut source, &mut w, Some(4)).unwrap();
        w.sync().unwrap();
        disk.crash();
        let mut f = disk.open("t.wav").unwrap();
        let report = recover(&mut f).unwrap();
        assert_eq!(report.frames_recovered, 4_000, "seed {seed}");
        let (info, data) = read_data(&mut f).unwrap();
        assert_eq!(info.data_len_header, 4_000 * 4);
        assert_eq!(data[..], reference[..data.len()]);
        assert_valid_plain_header(&read_all(&mut f));
        // Recovery is idempotent.
        assert!(!recover(&mut f).unwrap().repaired);
    }
}

#[test]
fn resume_after_crash_continues_the_same_track() {
    let format = fmt(SampleFormat::I16, 2);
    let reference = generated(format, 3_000);
    let disk = SimDisk::new(11);
    let mut w = WavWriter::create(
        disk.open("t.wav").unwrap(),
        format,
        WriterOptions::default(),
    )
    .unwrap();
    // The track file must survive a crash: make its directory entry durable.
    disk.sync_dir().unwrap();
    let mut source = Generator::new(format, 1_000, Some(3_000));
    pump(&mut source, &mut w, Some(1)).unwrap();
    w.sync().unwrap();
    disk.crash();

    let mut w = WavWriter::resume(disk.open("t.wav").unwrap(), WriterOptions::default()).unwrap();
    assert_eq!(w.frames(), 1_000);
    assert!(pump(&mut source, &mut w, None).unwrap());
    w.finalize().unwrap();
    let (_, data) = read_data(&mut disk.open("t.wav").unwrap()).unwrap();
    assert_eq!(data, reference);
}

#[test]
fn recovery_on_real_files_after_header_sizes_are_lost() {
    let dir = temp_dir("recover");
    let storage = FsStorage::new(&dir).unwrap();
    let format = fmt(SampleFormat::I24, 2);
    let reference = generated(format, 4_000);
    let mut w = WavWriter::create(
        storage.open("t.wav").unwrap(),
        format,
        WriterOptions::default(),
    )
    .unwrap();
    let mut source = Generator::new(format, 1_000, Some(4_000));
    pump(&mut source, &mut w, None).unwrap();
    drop(w); // killed: no finalize, header still says 0

    // Simulate a kill mid-write: zero the header sizes by hand and tear a frame.
    let path = dir.join("t.wav");
    let mut raw = fs::read(&path).unwrap();
    raw[4..8].copy_from_slice(&0u32.to_le_bytes());
    raw[80 - 4..80].copy_from_slice(&0u32.to_le_bytes());
    raw.extend_from_slice(&[9, 9, 9, 9, 9]); // 5 bytes: not a whole 6-byte frame
    fs::write(&path, &raw).unwrap();

    let mut f = storage.open("t.wav").unwrap();
    let report = recover(&mut f).unwrap();
    assert_eq!(report.frames_recovered, 4_000);
    assert_eq!(report.dropped_partial_bytes, 5);
    assert!(report.repaired);
    let (info, data) = read_data(&mut f).unwrap();
    assert_eq!(info.data_len_header as usize, reference.len());
    assert_eq!(data, reference);
    let on_disk = fs::read(&path).unwrap();
    assert_eq!(on_disk.len(), 80 + reference.len());
    assert_eq!(u32_at(&on_disk, 4) as usize, on_disk.len() - 8);
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn rf64_switch_with_lowered_threshold() {
    let format = fmt(SampleFormat::I16, 2);
    let opts = WriterOptions {
        header_interval_frames: Some(u64::MAX), // only the switch may rewrite the header
        rf64_threshold: 1_000,
        ..WriterOptions::default()
    };
    let disk = SimDisk::new(5);
    let mut w = WavWriter::create(disk.open("t.wav").unwrap(), format, opts).unwrap();
    let mut source = Generator::new(format, 100, Some(1_000)); // 400 bytes per buffer
    pump(&mut source, &mut w, Some(2)).unwrap(); // 800 bytes: still RIFF
    assert_eq!(w.header_writes(), 0);
    let mut f = disk.open("t.wav").unwrap();
    assert!(!read_info(&mut f).unwrap().rf64);
    pump(&mut source, &mut w, Some(1)).unwrap(); // 1200 bytes: past the threshold
    assert_eq!(w.header_writes(), 1);
    let info = read_info(&mut f).unwrap();
    assert!(info.rf64);
    assert_eq!(info.data_len_header, 1_200);
    assert_eq!(info.data_offset, DATA_OFFSET);
    pump(&mut source, &mut w, None).unwrap();
    let info = w.finalize().unwrap();
    assert!(info.rf64);
    assert_eq!(info.data_len_header, 4_000);

    let bytes = read_all(&mut f);
    assert_eq!(&bytes[0..4], b"RF64");
    assert_eq!(u32_at(&bytes, 4), 0xFFFF_FFFF);
    assert_eq!(&bytes[12..16], b"ds64");
    assert_eq!(u32_at(&bytes, 76), 0xFFFF_FFFF);
    let (_, data) = read_data(&mut f).unwrap();
    assert_eq!(data, generated(format, 1_000));

    // A stale RF64 file recovers as RF64 with the same threshold.
    let mut f2 = disk.open("t2.wav").unwrap();
    let mut w = WavWriter::create(
        disk.open("t2.wav").unwrap(),
        format,
        WriterOptions {
            header_interval_frames: Some(u64::MAX),
            ..opts
        },
    )
    .unwrap();
    let mut source = Generator::new(format, 100, Some(500));
    pump(&mut source, &mut w, None).unwrap();
    drop(w);
    let report = recover_with(&mut f2, 1_000).unwrap();
    assert_eq!(report.frames_recovered, 500);
    assert!(read_info(&mut f2).unwrap().rf64);
}

fn decode(data: &[u8], sf: SampleFormat) -> Vec<i32> {
    match sf {
        SampleFormat::I16 => data
            .as_chunks::<2>()
            .0
            .iter()
            .map(|c| i32::from(i16::from_le_bytes(*c)))
            .collect(),
        SampleFormat::I24 => data
            .as_chunks::<3>()
            .0
            .iter()
            .map(|c| i32::from_le_bytes([0, c[0], c[1], c[2]]) >> 8)
            .collect(),
        SampleFormat::F32 => unreachable!(),
    }
}

#[test]
fn flac_round_trip_is_bit_exact_on_real_files() {
    let dir = temp_dir("flac");
    let storage = FsStorage::new(&dir).unwrap();
    for (sf, ch) in [
        (SampleFormat::I16, 2),
        (SampleFormat::I24, 2),
        (SampleFormat::I24, 1),
    ] {
        let format = fmt(sf, ch);
        let name = format!("t-{sf:?}-{ch}");
        let mut source = Generator::new(format, 1_000, Some(20_000));
        record(
            &mut source,
            storage.open(&format!("{name}.wav")).unwrap(),
            WriterOptions::default(),
        )
        .unwrap();
        let report =
            wav_to_flac(&storage, &format!("{name}.wav"), &format!("{name}.flac")).unwrap();
        assert_eq!(report.frames, 20_000);

        let flac = fs::read(dir.join(format!("{name}.flac"))).unwrap();
        assert_eq!(flac.len() as u64, report.flac_bytes);
        assert_eq!(&flac[0..4], b"fLaC");
        let mut reader = claxon::FlacReader::new(std::io::Cursor::new(&flac)).unwrap();
        let si = reader.streaminfo();
        assert_eq!(si.sample_rate, 48_000);
        assert_eq!(si.channels, u32::from(ch));
        assert_eq!(
            si.bits_per_sample,
            if sf == SampleFormat::I16 { 16 } else { 24 }
        );
        let decoded: Vec<i32> = reader.samples().map(|s| s.unwrap()).collect();
        let (_, wav_data) = read_data(&mut storage.open(&format!("{name}.wav")).unwrap()).unwrap();
        assert_eq!(decoded, decode(&wav_data, sf), "{name}");
        assert_eq!(wav_data, generated(format, 20_000));
    }
    let _ = fs::remove_dir_all(&dir);
}

#[test]
fn flac_is_not_applicable_to_float_and_the_wav_is_left_alone() {
    let dir = temp_dir("flac-f32");
    let storage = FsStorage::new(&dir).unwrap();
    let format = fmt(SampleFormat::F32, 2);
    let mut source = Generator::new(format, 1_000, Some(2_000));
    record(
        &mut source,
        storage.open("f.wav").unwrap(),
        WriterOptions::default(),
    )
    .unwrap();
    let before = fs::read(dir.join("f.wav")).unwrap();
    match wav_to_flac(&storage, "f.wav", "f.flac") {
        Err(FlacError::NotApplicable(msg)) => assert!(msg.contains("float"), "{msg}"),
        other => panic!("expected NotApplicable, got {other:?}"),
    }
    assert!(!storage.exists("f.flac").unwrap());
    assert_eq!(fs::read(dir.join("f.wav")).unwrap(), before);
    let _ = fs::remove_dir_all(&dir);
}
