//! Sample-time arithmetic.
//!
//! The Turtle's internal time base is *samples at the show's playback rate*
//! (spec §7). These helpers convert between musical time (beats) and samples,
//! and compute tempo-synced delay times from the song's nominal BPM (§6).

/// Convert a duration in beats to samples at `rate`, given `bpm`.
///
/// `samples = beats * (60 / bpm) * rate`, rounded to nearest.
pub fn beats_to_samples(beats: f64, bpm: f64, rate: u32) -> u64 {
    let seconds = beats * 60.0 / bpm;
    (seconds * rate as f64).round().max(0.0) as u64
}

/// Inverse of [`beats_to_samples`].
pub fn samples_to_beats(samples: u64, bpm: f64, rate: u32) -> f64 {
    let seconds = samples as f64 / rate as f64;
    seconds * bpm / 60.0
}

/// Tempo-synced delay time in samples for a note division expressed in beats
/// (e.g. a 1/8 note = `0.5`, a dotted 1/4 = `1.5`) at the song's nominal `bpm`.
pub fn division_to_samples(beats_per_division: f64, bpm: f64, rate: u32) -> u64 {
    beats_to_samples(beats_per_division, bpm, rate)
}

/// Convert a signed millisecond offset (per-destination latency alignment, §5)
/// to a sample offset at `rate`.
pub fn ms_to_samples(ms: f64, rate: u32) -> i64 {
    (ms / 1000.0 * rate as f64).round() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn one_beat_at_120bpm_is_half_second() {
        // 120 BPM -> 0.5 s/beat -> 24000 samples at 48k.
        assert_eq!(beats_to_samples(1.0, 120.0, 48_000), 24_000);
    }

    #[test]
    fn beats_samples_roundtrip() {
        let s = beats_to_samples(4.0, 122.0, 48_000);
        let b = samples_to_beats(s, 122.0, 48_000);
        assert!((b - 4.0).abs() < 1e-3, "got {b}");
    }

    #[test]
    fn eighth_note_delay_at_120bpm() {
        // 1/8 note = 0.25 s at 120 BPM -> 12000 samples at 48k.
        assert_eq!(division_to_samples(0.5, 120.0, 48_000), 12_000);
    }

    #[test]
    fn ms_offset_signed() {
        assert_eq!(ms_to_samples(-8.0, 48_000), -384);
        assert_eq!(ms_to_samples(0.0, 48_000), 0);
    }
}
