//! Writing aligned tracks (broadcast-wave WAV with a `bext` chunk, or FLAC
//! with the same time reference as Vorbis comments) and marker exports
//! (Audacity labels, Adobe Audition marker CSV).

use std::io;

use castcorder_port_capture::SampleFormat;
use castcorder_port_disk::{Storage, StorageFile};
use castcorder_recorder::encode_flac;

/// Facts written into an aligned file's metadata.
pub(crate) struct Stamp<'a> {
    /// Host time (us since the Unix epoch) of the file's first frame.
    pub start_us: i64,
    pub rate: u32,
    pub description: &'a str,
    pub reference: &'a str,
}

impl Stamp<'_> {
    /// Samples since midnight UTC of the first frame's day (the BWF time
    /// reference).
    pub fn time_reference(&self) -> u64 {
        let day = self.start_us.rem_euclid(86_400_000_000) as u128;
        (day * u128::from(self.rate) / 1_000_000) as u64
    }

    /// (`yyyy-mm-dd`, `hh:mm:ss`) of the first frame, UTC.
    pub fn date_time(&self) -> (String, String) {
        let secs = self.start_us.div_euclid(1_000_000);
        let days = secs.div_euclid(86_400);
        let sod = secs.rem_euclid(86_400);
        // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z.rem_euclid(146_097);
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let d = doy - (153 * mp + 2) / 5 + 1;
        let m = if mp < 10 { mp + 3 } else { mp - 9 };
        let y = yoe + era * 400 + i64::from(m <= 2);
        (
            format!("{y:04}-{m:02}-{d:02}"),
            format!("{:02}:{:02}:{:02}", sod / 3600, sod / 60 % 60, sod % 60),
        )
    }
}

fn fixed(s: &str, n: usize) -> Vec<u8> {
    let mut v: Vec<u8> = s.bytes().filter(u8::is_ascii).take(n).collect();
    v.resize(n, 0);
    v
}

fn bits(f: SampleFormat) -> u16 {
    (f.bytes() * 8) as u16
}

/// Integer sample of a float, with rounding and clipping.
fn quantise(v: f32, f: SampleFormat) -> i32 {
    let scale = match f {
        SampleFormat::I16 => 32_768.0,
        SampleFormat::I24 => 8_388_608.0,
        SampleFormat::F32 => return 0,
    };
    (f64::from(v) * scale).round().clamp(-scale, scale - 1.0) as i32
}

/// A broadcast-wave file in memory: RIFF, fmt, bext (version 1), data.
pub(crate) fn wav_bytes(
    planes: &[Vec<f32>],
    format: SampleFormat,
    stamp: &Stamp<'_>,
) -> io::Result<Vec<u8>> {
    let channels = planes.len() as u16;
    let frames = planes.first().map_or(0, Vec::len);
    let bps = format.bytes();
    let data_len = frames * usize::from(channels) * bps;
    let (date, time) = stamp.date_time();
    let mut bext = Vec::new();
    bext.extend(fixed(stamp.description, 256));
    bext.extend(fixed("castcorder", 32));
    bext.extend(fixed(stamp.reference, 32));
    bext.extend(fixed(&date, 10));
    bext.extend(fixed(&time, 8));
    let tr = stamp.time_reference();
    bext.extend_from_slice(&(tr as u32).to_le_bytes());
    bext.extend_from_slice(&((tr >> 32) as u32).to_le_bytes());
    bext.extend_from_slice(&1u16.to_le_bytes());
    bext.extend([0u8; 64 + 190]);
    let mode = if channels == 1 { "mono" } else { "stereo" };
    bext.extend_from_slice(
        format!(
            "A=PCM,F={},W={},M={mode},T=castcorder-align\r\n",
            stamp.rate,
            bits(format)
        )
        .as_bytes(),
    );
    if bext.len() % 2 == 1 {
        bext.push(0);
    }
    let riff_len = 4 + (8 + 16) + (8 + bext.len()) + (8 + data_len);
    if riff_len > u32::MAX as usize {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "aligned track over 4 GiB; use FLAC",
        ));
    }
    let mut b = Vec::with_capacity(riff_len + 8);
    b.extend_from_slice(b"RIFF");
    b.extend_from_slice(&(riff_len as u32).to_le_bytes());
    b.extend_from_slice(b"WAVE");
    b.extend_from_slice(b"fmt ");
    b.extend_from_slice(&16u32.to_le_bytes());
    let tag: u16 = if format == SampleFormat::F32 { 3 } else { 1 };
    b.extend_from_slice(&tag.to_le_bytes());
    b.extend_from_slice(&channels.to_le_bytes());
    b.extend_from_slice(&stamp.rate.to_le_bytes());
    let frame_bytes = u32::from(channels) * bps as u32;
    b.extend_from_slice(&(stamp.rate * frame_bytes).to_le_bytes());
    b.extend_from_slice(&(frame_bytes as u16).to_le_bytes());
    b.extend_from_slice(&bits(format).to_le_bytes());
    b.extend_from_slice(b"bext");
    b.extend_from_slice(&(bext.len() as u32).to_le_bytes());
    b.extend_from_slice(&bext);
    b.extend_from_slice(b"data");
    b.extend_from_slice(&(data_len as u32).to_le_bytes());
    for i in 0..frames {
        for p in planes {
            let v = p[i];
            match format {
                SampleFormat::F32 => b.extend_from_slice(&v.to_le_bytes()),
                SampleFormat::I16 => {
                    b.extend_from_slice(&(quantise(v, format) as i16).to_le_bytes())
                }
                SampleFormat::I24 => b.extend_from_slice(&quantise(v, format).to_le_bytes()[..3]),
            }
        }
    }
    Ok(b)
}

/// A FLAC file in memory (integer formats only), the time reference and the
/// broadcast-wave facts as Vorbis comments.
pub(crate) fn flac_bytes(
    planes: &[Vec<f32>],
    format: SampleFormat,
    stamp: &Stamp<'_>,
) -> io::Result<Vec<u8>> {
    let frames = planes.first().map_or(0, Vec::len);
    let mut samples = Vec::with_capacity(frames * planes.len());
    for i in 0..frames {
        for p in planes {
            samples.push(quantise(p[i], format));
        }
    }
    let (date, time) = stamp.date_time();
    let tags = vec![
        ("TIME_REFERENCE".into(), stamp.time_reference().to_string()),
        ("DESCRIPTION".into(), stamp.description.to_string()),
        ("ORIGINATOR".into(), "castcorder".into()),
        ("ORIGINATOR_REFERENCE".into(), stamp.reference.to_string()),
        ("ORIGINATION_DATE".into(), date),
        ("ORIGINATION_TIME".into(), time),
        (
            "CASTCORDER_HOST_START_US".into(),
            stamp.start_us.to_string(),
        ),
    ];
    encode_flac(
        &samples,
        planes.len(),
        usize::from(bits(format)),
        stamp.rate,
        &tags,
    )
    .map_err(|e| io::Error::other(e.to_string()))
}

/// Replace `name` in `dir` with `bytes`, durably.
pub(crate) fn write_file<S: Storage>(dir: &S, name: &str, bytes: &[u8]) -> io::Result<()> {
    let mut f = dir.open(name)?;
    f.set_len(0)?;
    f.write_at(0, bytes)?;
    f.sync()
}

/// A marker on the take's timeline.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Marker {
    /// Seconds after the take's start.
    pub start_s: f64,
    /// Length in seconds (0: a point).
    pub duration_s: f64,
    /// Short name.
    pub name: String,
    /// More detail.
    pub description: String,
}

/// Audacity label track text: `start<TAB>end<TAB>label`, seconds.
pub(crate) fn audacity_labels(markers: &[Marker]) -> String {
    markers
        .iter()
        .map(|m| {
            format!(
                "{:.6}\t{:.6}\t{}\n",
                m.start_s,
                m.start_s + m.duration_s,
                clean(&m.name)
            )
        })
        .collect()
}

fn clean(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c == '\t' || c == '\n' || c == '\r' {
                ' '
            } else {
                c
            }
        })
        .collect()
}

/// Seconds as Audition's decimal time: `m:ss.mmm`, or `h:mm:ss.mmm` from an hour.
fn audition_time(s: f64) -> String {
    let ms = (s.max(0.0) * 1000.0).round() as u64;
    let (h, m, sec, milli) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if h > 0 {
        format!("{h}:{m:02}:{sec:02}.{milli:03}")
    } else {
        format!("{m}:{sec:02}.{milli:03}")
    }
}

/// Adobe Audition marker list (its "CSV" is tab-separated): `Name`, `Start`,
/// `Duration`, `Time Format` (decimal), `Type` (Cue), `Description`.
pub(crate) fn audition_csv(markers: &[Marker]) -> String {
    let mut s = String::from("Name\tStart\tDuration\tTime Format\tType\tDescription\n");
    for m in markers {
        s.push_str(&format!(
            "{}\t{}\t{}\tdecimal\tCue\t{}\n",
            clean(&m.name),
            audition_time(m.start_s),
            audition_time(m.duration_s),
            clean(&m.description)
        ));
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_and_time_references() {
        let s = Stamp {
            start_us: 1_700_000_000_500_000,
            rate: 48_000,
            description: "",
            reference: "",
        };
        assert_eq!(
            s.date_time(),
            ("2023-11-14".to_string(), "22:13:20".to_string())
        );
        assert_eq!(s.time_reference(), (80_000 * 48_000) + 24_000);
        assert_eq!(audition_time(61.2345), "1:01.235");
        assert_eq!(audition_time(3725.0), "1:02:05.000");
    }
}
