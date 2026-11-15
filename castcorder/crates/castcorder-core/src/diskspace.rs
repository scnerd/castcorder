//! How long a device's free disk lasts at the recording profile's data rate
//! (design.md: each device reports free disk; producers are warned under about
//! an hour). One rule for the host's warning and its tests; the warning never
//! stops a take.

/// A warning is due when less than this much recording fits (seconds).
pub const WARN_BELOW_SECS: u64 = 3600;

/// Audio, per second of a take: 48 kHz stereo 32-bit float WAV, the largest
/// the devices write (a 44.1 kHz or mono device writes less).
pub const AUDIO_BYTES_PER_SEC: u64 = 48_000 * 2 * 4;

/// A webcam recording, per second, on top of the audio: 8 Mbit/s, what a
/// 720p30 camera is asked for (the encoder's floor is 4, its ceiling 60).
pub const VIDEO_BYTES_PER_SEC: u64 = 1_000_000;

/// What one second of a take writes, for a recording profile with or without
/// video.
pub fn bytes_per_sec(video: bool) -> u64 {
    AUDIO_BYTES_PER_SEC + if video { VIDEO_BYTES_PER_SEC } else { 0 }
}

/// How many seconds of a take `free_bytes` holds.
pub fn seconds_left(free_bytes: u64, video: bool) -> u64 {
    free_bytes / bytes_per_sec(video)
}

/// Whether `free_bytes` holds less than [`WARN_BELOW_SECS`] of a take.
pub fn is_low(free_bytes: u64, video: bool) -> bool {
    seconds_left(free_bytes, video) < WARN_BELOW_SECS
}

/// `free_bytes` rounded down to 100 MB: free space moves with every write, and
/// a producer is told only when it moves by something worth showing.
pub fn rounded_free(free_bytes: u64) -> u64 {
    free_bytes / 100_000_000 * 100_000_000
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn about_an_hour_depends_on_the_profile() {
        // Audio only: 384 kB/s is 1.38 GB an hour.
        assert!(is_low(1_000_000_000, false));
        assert!(!is_low(1_500_000_000, false));
        // With video the same room lasts a lot less.
        assert!(is_low(1_500_000_000, true));
        assert!(!is_low(5_000_000_000, true));
        assert_eq!(seconds_left(bytes_per_sec(false) * 90, false), 90);
        assert!(is_low(0, false));
    }

    #[test]
    fn free_space_is_rounded_down_to_a_tenth_of_a_gigabyte() {
        assert_eq!(rounded_free(1_299_999_999), 1_200_000_000);
        assert_eq!(rounded_free(99_999_999), 0);
    }
}
