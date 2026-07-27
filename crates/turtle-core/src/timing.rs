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

/// The note divisions a tempo-synced delay can be set to (§6), longest first.
///
/// A stepped list rather than a continuous time, because that is what makes a
/// delay sit *in* the song: any value between a dotted eighth and a quarter is
/// simply wrong against the beat. A CC selects one of these rather than sweeping
/// a millisecond value.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DelayDivision {
    Whole,
    DottedHalf,
    Half,
    DottedQuarter,
    Quarter,
    DottedEighth,
    Eighth,
    DottedSixteenth,
    Sixteenth,
}

impl DelayDivision {
    /// Every division, **shortest to longest** — the order a CC sweeps through
    /// them, so turning the knob up lengthens the delay.
    pub const ALL: [DelayDivision; 9] = [
        DelayDivision::Sixteenth,
        DelayDivision::DottedSixteenth,
        DelayDivision::Eighth,
        DelayDivision::DottedEighth,
        DelayDivision::Quarter,
        DelayDivision::DottedQuarter,
        DelayDivision::Half,
        DelayDivision::DottedHalf,
        DelayDivision::Whole,
    ];

    /// Length in beats. A dot adds half again, as in notation.
    pub fn beats(self) -> f64 {
        match self {
            DelayDivision::Whole => 4.0,
            DelayDivision::DottedHalf => 3.0,
            DelayDivision::Half => 2.0,
            DelayDivision::DottedQuarter => 1.5,
            DelayDivision::Quarter => 1.0,
            DelayDivision::DottedEighth => 0.75,
            DelayDivision::Eighth => 0.5,
            DelayDivision::DottedSixteenth => 0.375,
            DelayDivision::Sixteenth => 0.25,
        }
    }

    /// How this division reads in a log line or `turtle status`.
    pub fn label(self) -> &'static str {
        match self {
            DelayDivision::Whole => "1/1",
            DelayDivision::DottedHalf => "1/2.",
            DelayDivision::Half => "1/2",
            DelayDivision::DottedQuarter => "1/4.",
            DelayDivision::Quarter => "1/4",
            DelayDivision::DottedEighth => "1/8.",
            DelayDivision::Eighth => "1/8",
            DelayDivision::DottedSixteenth => "1/16.",
            DelayDivision::Sixteenth => "1/16",
        }
    }

    /// Pick a division from a 0..=127 CC value.
    ///
    /// The range is split into equal bands so each division is equally easy to
    /// land on with a pedal. Saturating at both ends rather than wrapping, so
    /// slamming the pedal to either stop gives the shortest or longest.
    pub fn from_cc(value: u8) -> DelayDivision {
        let n = DelayDivision::ALL.len();
        // 128 CC values across n bands; the last band absorbs the remainder.
        let idx = (value as usize * n / 128).min(n - 1);
        DelayDivision::ALL[idx]
    }

    /// This division's delay time in samples at the song's tempo.
    pub fn to_samples(self, bpm: f64, rate: u32) -> u64 {
        division_to_samples(self.beats(), bpm, rate)
    }

    /// The longest division, which is what a delay buffer must be sized for.
    pub fn longest() -> DelayDivision {
        DelayDivision::Whole
    }
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

    /// A CC must reach every division — one that could not be selected would be
    /// dead weight in the list, and the operator would never know which.
    #[test]
    fn every_division_is_reachable_from_some_cc_value() {
        let mut seen = std::collections::BTreeSet::new();
        for cc in 0..=127u8 {
            seen.insert(DelayDivision::from_cc(cc).label());
        }
        assert_eq!(
            seen.len(),
            DelayDivision::ALL.len(),
            "unreachable divisions: got {seen:?}"
        );
    }

    /// Turning the knob up must lengthen the delay — the opposite would be a
    /// genuinely confusing pedal.
    #[test]
    fn higher_cc_values_give_longer_delays() {
        let mut last = 0.0;
        for cc in 0..=127u8 {
            let beats = DelayDivision::from_cc(cc).beats();
            assert!(beats >= last, "cc {cc} went backwards: {beats} after {last}");
            last = beats;
        }
        assert_eq!(DelayDivision::from_cc(0), DelayDivision::Sixteenth);
        assert_eq!(DelayDivision::from_cc(127), DelayDivision::Whole);
    }

    /// The point of tempo sync: the delay lands on the beat. At 120 BPM a quarter
    /// note is exactly 0.5 s, so a quarter-note delay at 48 kHz is 24000 samples.
    #[test]
    fn divisions_land_on_the_beat_at_a_known_tempo() {
        assert_eq!(DelayDivision::Quarter.to_samples(120.0, 48_000), 24_000);
        assert_eq!(DelayDivision::Eighth.to_samples(120.0, 48_000), 12_000);
        assert_eq!(DelayDivision::Half.to_samples(120.0, 48_000), 48_000);
        assert_eq!(DelayDivision::Whole.to_samples(120.0, 48_000), 96_000);
        // A dot is half again: a dotted eighth is three sixteenths.
        assert_eq!(DelayDivision::DottedEighth.to_samples(120.0, 48_000), 18_000);
    }

    /// The buffer is sized from the longest division at the song's tempo; a slow
    /// song needs a bigger one, and getting this wrong clamps the longest delays.
    #[test]
    fn the_longest_division_sizes_the_buffer() {
        // A whole note at 60 BPM is 4 seconds — twice the old fixed 2 s cap.
        assert_eq!(DelayDivision::longest().to_samples(60.0, 48_000), 192_000);
        assert_eq!(DelayDivision::longest(), DelayDivision::Whole);
    }
}
