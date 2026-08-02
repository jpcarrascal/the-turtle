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
///
/// Deserialised from its own label (`"1/8"`, `"1/4."`) rather than a number,
/// because unlike the continuous controls this is a discrete *musical* value: a
/// show file saying `time = "1/8"` is self-documenting where `time = 40` would
/// require knowing the CC band layout.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(try_from = "String", into = "String")]
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

    /// Parse a division from its [`label`](DelayDivision::label).
    pub fn from_label(label: &str) -> Option<DelayDivision> {
        DelayDivision::ALL
            .iter()
            .copied()
            .find(|d| d.label() == label)
    }
}

impl TryFrom<String> for DelayDivision {
    type Error = String;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        DelayDivision::from_label(&value).ok_or_else(|| {
            let names: Vec<&str> = DelayDivision::ALL.iter().map(|d| d.label()).collect();
            format!("unknown note division {value:?}; expected one of {}", names.join(", "))
        })
    }
}

impl From<DelayDivision> for String {
    fn from(d: DelayDivision) -> String {
        d.label().to_string()
    }
}

/// Convert a signed millisecond offset (per-destination latency alignment, §5)
/// to a sample offset at `rate`.
pub fn ms_to_samples(ms: f64, rate: u32) -> i64 {
    (ms / 1000.0 * rate as f64).round() as i64
}

/// MIDI clock pulses per quarter note, fixed by the MIDI spec.
pub const PPQN: u64 = 24;

/// Generates MIDI clock pulse times from the transport position (§5).
///
/// # Why pulses are derived, not accumulated
///
/// Each pulse's sample time is computed from its **index** —
/// `round(index * samples_per_pulse)` — rather than by adding an interval to the
/// previous pulse. Accumulating would compound the rounding error of every pulse:
/// at 120 BPM and 48 kHz a pulse is 1000 samples exactly, but at 122 BPM it is
/// 983.6, and adding a rounded 984 each time drifts by about a quarter of a second
/// per hour against the audio it is supposed to be synchronised with. Deriving is
/// exact forever: pulse 100,000 lands where the arithmetic says it should.
///
/// What this does *not* fix is when the pulse is actually written to the wire —
/// that depends on how often the dispatch loop runs, and is bounded by one tick.
/// The distinction matters: the clock has **no drift** but does have **phase
/// jitter**, and gear that averages tempo over a window sees the exact tempo.
///
/// # Tempo
///
/// One nominal BPM per song (§14 lists tempo-map-following as future). A song
/// whose project has tempo changes will hold this tempo while its stems do not.
#[derive(Debug, Clone)]
pub struct MidiClock {
    /// Samples between pulses. Fractional deliberately — at most tempos this is
    /// not a whole number of samples, and rounding it here is what would drift.
    samples_per_pulse: f64,
    /// The next pulse index not yet emitted.
    next_index: u64,
    /// Position at the previous [`advance`](MidiClock::advance), for spotting a
    /// discontinuity.
    last_pos: Option<u64>,
    /// A position jump larger than this reads as a seek rather than a stall.
    discontinuity: u64,
}

/// What [`MidiClock::advance`] found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Advance {
    /// Pulse indices that came due since the previous call.
    Pulses(std::ops::Range<u64>),
    /// The transport jumped — a loop wrap, a rewind, a seek. The clock has
    /// rebased and emitted nothing, because no time passed: those pulses were
    /// never late, they simply never came due.
    Discontinuity,
}

impl MidiClock {
    /// For a song at `bpm`, played at `rate` Hz.
    ///
    /// `Song::validate` already rejects a tempo that is not `> 0`, so the guards
    /// below are purely defensive — but they defend the *control thread*, which
    /// must not panic or hang mid-set whatever reaches it.
    ///
    /// Two separate hazards, both real:
    ///
    /// * A non-positive or non-finite tempo makes the interval infinite or
    ///   negative. Clamped to 1 BPM rather than to a plausible 120, because a
    ///   clock crawling at 1 BPM is obviously broken to whoever is listening,
    ///   whereas a fabricated 120 looks correct and hides the bad song file.
    /// * An absurdly *high* tempo makes the interval round to zero samples, and
    ///   [`drain_due`](Self::drain_due) would then never advance past the current
    ///   position — a hang on the thread that dispatches MIDI. The interval is
    ///   floored at one sample, which is nonsense musically but terminates.
    pub fn new(bpm: f64, rate: u32) -> Self {
        let bpm = if bpm.is_finite() && bpm > 0.0 { bpm } else { 1.0 };
        let samples_per_pulse = (60.0 * rate as f64 / (bpm * PPQN as f64)).max(1.0);
        MidiClock {
            samples_per_pulse,
            next_index: 0,
            last_pos: None,
            // 100 ms, about a hundred dispatch ticks. Set well above any stall
            // worth noticing — a 100 ms gap in the dispatch loop would be an
            // audible dropout with much larger problems than clock timing — so a
            // loop merely running late still emits its pulses.
            discontinuity: rate as u64 / 10,
        }
    }

    /// The sample time of pulse `index`, from the start of the song.
    pub fn pulse_sample(&self, index: u64) -> u64 {
        (index as f64 * self.samples_per_pulse).round() as u64
    }

    /// Nominal interval between pulses, in samples. For a probe reporting what the
    /// spacing *should* have been.
    pub fn samples_per_pulse(&self) -> f64 {
        self.samples_per_pulse
    }

    /// The pulses due at `pos`, or a report that the transport jumped.
    ///
    /// This is the only way to drive the clock, deliberately. An earlier version
    /// exposed the draining and the rebasing separately and left callers to notice
    /// discontinuities from transport *events* — which was wrong, because
    /// `RtEvent::Looped` reaches the control thread before the clock has published
    /// a post-wrap anchor, so a rebase to 0 was followed by a tick at the old
    /// position and emitted an entire song of pulses at once. Detecting the jump
    /// from the position itself needs no assumption about event ordering, and
    /// keeping it in here means no caller can get it wrong again.
    pub fn advance(&mut self, pos: u64) -> Advance {
        if let Some(prev) = self.last_pos.replace(pos) {
            let backwards = prev.saturating_sub(pos);
            // A small step backwards is not a reposition. Even a monotonic frame
            // count is *interpolated* between anchors, so the value can dip by a few
            // samples when a fresh anchor lands. Rebasing on that would move the next
            // pulse index behind a pulse already sent, and send it a second time —
            // one duplicate every few minutes, which is a slow gain against the
            // audio. Holding phase costs nothing: the pulses are still owed at their
            // own times, and the next forward tick emits them.
            if backwards > 0 && backwards <= self.discontinuity {
                self.last_pos = Some(prev);
                return Advance::Pulses(self.next_index..self.next_index);
            }
            if pos < prev || pos - prev > self.discontinuity {
                self.reset_to(pos);
                return Advance::Discontinuity;
            }
        }
        Advance::Pulses(self.drain_due(pos))
    }

    /// Pulse indices due at or before `pos`, advancing past them.
    fn drain_due(&mut self, pos: u64) -> std::ops::Range<u64> {
        let start = self.next_index;
        while self.pulse_sample(self.next_index) <= pos {
            self.next_index += 1;
        }
        start..self.next_index
    }

    /// Rebase to `pos`, emitting nothing for the pulses skipped over.
    fn reset_to(&mut self, pos: u64) {
        self.next_index = 0;
        while self.pulse_sample(self.next_index) < pos {
            self.next_index += 1;
        }
    }
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

    /// Round-trips through its label, so a show file written by `gen-tone` (or by
    /// hand) parses back to the same division.
    #[test]
    fn divisions_round_trip_through_their_labels() {
        for d in DelayDivision::ALL {
            assert_eq!(DelayDivision::from_label(d.label()), Some(d), "{}", d.label());
        }
        // And a typo is rejected with the valid names listed, rather than silently
        // falling back to some default.
        let err = DelayDivision::try_from("1/3".to_string()).unwrap_err();
        assert!(err.contains("unknown note division"), "{err}");
        assert!(err.contains("1/4"), "the message should list valid names: {err}");
    }
    // ---- MIDI clock (§5) ----

    /// 24 ppqn at 120 BPM and 48 kHz is exactly 1000 samples — the one tempo where
    /// the arithmetic is checkable by hand.
    #[test]
    fn pulses_land_on_the_expected_samples() {
        let c = MidiClock::new(120.0, 48_000);
        assert_eq!(c.samples_per_pulse(), 1000.0);
        assert_eq!(c.pulse_sample(0), 0);
        assert_eq!(c.pulse_sample(24), 24_000, "24 pulses = one beat = half a second");
        assert_eq!(c.pulse_sample(48), 48_000, "48 pulses = two beats = one second");
    }

    /// The property the whole design rests on: pulse times are DERIVED from the
    /// index, so they never drift, even at a tempo whose pulse is not a whole
    /// number of samples. Accumulating a rounded interval instead would drift by
    /// roughly a quarter-second per hour here — inaudible per pulse and fatal over
    /// a set.
    #[test]
    fn pulse_times_do_not_drift_at_an_awkward_tempo() {
        let rate = 48_000u32;
        let bpm = 122.0; // 983.6... samples per pulse
        let c = MidiClock::new(bpm, rate);

        // One hour of pulses.
        let pulses = (bpm / 60.0 * PPQN as f64 * 3600.0) as u64;
        let last = c.pulse_sample(pulses);
        let ideal = pulses as f64 * c.samples_per_pulse();
        assert!(
            (last as f64 - ideal).abs() <= 1.0,
            "pulse {pulses} at {last}, ideal {ideal}: derived times must not drift"
        );

        // And what accumulation would have done, for the record.
        let accumulated = c.samples_per_pulse().round() as u64 * pulses;
        assert!(
            (accumulated as f64 - ideal).abs() > rate as f64 * 0.1,
            "the accumulating version should be >100ms out, or this test proves nothing"
        );
    }

    #[test]
    fn advancing_returns_each_pulse_exactly_once() {
        let mut c = MidiClock::new(120.0, 48_000);
        // Pulse 0 is at sample 0, so it is due immediately.
        assert_eq!(c.advance(0), Advance::Pulses(0..1));
        assert_eq!(c.advance(999), Advance::Pulses(1..1), "nothing until the next pulse time");
        assert_eq!(c.advance(1000), Advance::Pulses(1..2));
        // A gap yields every pulse in it, in order, none repeated.
        assert_eq!(c.advance(5000), Advance::Pulses(2..6));
        assert_eq!(c.advance(5000), Advance::Pulses(6..6));
    }

    /// A seek or a loop wrap moves the position without time passing. Firing every
    /// pulse in between would be heard downstream as a tempo spike, so a rebase
    /// emits nothing.
    /// The failure seen on hardware: a loop wrap made the probe report an entire
    /// song of pulses in one tick, at 43 seconds of "lateness". A backwards jump
    /// must rebase and emit nothing.
    #[test]
    fn a_backwards_jump_is_a_discontinuity_not_a_burst() {
        let mut c = MidiClock::new(120.0, 48_000);
        c.advance(10_000); // pulses 0..=10
        assert_eq!(c.advance(0), Advance::Discontinuity, "a wrap emits nothing");
        // ...and the train resumes from the top of the loop: pulse 0 sits exactly
        // at the new position, so it is still owed and fires on the next tick.
        assert_eq!(c.advance(1000), Advance::Pulses(0..2));
    }

    /// A forward seek is the same discontinuity in the other direction.
    #[test]
    fn a_large_forward_jump_is_a_discontinuity() {
        let mut c = MidiClock::new(120.0, 48_000);
        c.advance(0);
        assert_eq!(c.advance(48_000 * 30), Advance::Discontinuity, "a seek emits nothing");
        // Pulse 1440 lands exactly on the seek target, so it is owed, not skipped.
        assert_eq!(c.advance(48_000 * 30 + 1000), Advance::Pulses(1440..1442));
    }

    /// But a loop merely running late is NOT a discontinuity — those pulses are
    /// genuinely due, and swallowing them would hide the very thing the clock
    /// exists to get right.
    #[test]
    fn a_stall_short_of_the_threshold_still_emits_its_pulses() {
        let mut c = MidiClock::new(120.0, 48_000);
        c.advance(0);
        // 50ms later: under the 100ms threshold, so these pulses are late, not gone.
        match c.advance(48_000 / 20) {
            Advance::Pulses(r) => assert_eq!(r.end - r.start, 2, "a 50ms stall owes 2 pulses"),
            Advance::Discontinuity => panic!("a 50ms stall is not a discontinuity"),
        }
    }

    /// A song's tempo is validated `> 0`, but the control thread must not panic
    /// mid-set if a bad value ever reaches it.
    #[test]
    fn a_nonsense_tempo_does_not_panic_or_hang() {
        // The high tempos are the dangerous ones: a sub-sample pulse interval would
        // leave `drain_due` unable to advance past `pos`, hanging the thread that
        // dispatches MIDI. This test is a hang detector as much as an assertion.
        for bpm in [0.0, -120.0, f64::NAN, f64::INFINITY, 1e9, f64::MAX] {
            let mut c = MidiClock::new(bpm, 48_000);
            assert!(c.samples_per_pulse().is_finite(), "bpm {bpm} gave a non-finite interval");
            assert!(c.samples_per_pulse() >= 1.0, "bpm {bpm} gave a sub-sample interval");
            let _ = c.advance(48_000);
            let _ = c.advance(96_000);
        }
    }

    /// A pulse must never be sent twice. Interpolated time can dip by a few samples
    /// when a fresh anchor lands, and rebasing on that moved the next index behind a
    /// pulse already emitted — one duplicate every few minutes, seen on hardware as
    /// an occasional extra pulse in an otherwise exact loop audit.
    #[test]
    fn a_small_backwards_step_never_repeats_a_pulse() {
        let mut c = MidiClock::new(120.0, 48_000); // 1000 samples per pulse
        let mut emitted = Vec::new();
        let mut take = |a: Advance| {
            if let Advance::Pulses(r) = a {
                emitted.extend(r);
            }
        };

        take(c.advance(0));
        take(c.advance(1_000));
        take(c.advance(2_000));
        // Time dips back below the last pulse, then recovers.
        take(c.advance(1_980));
        take(c.advance(2_010));
        take(c.advance(3_000));

        let mut sorted = emitted.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(emitted, sorted, "pulses must be emitted once each, in order: {emitted:?}");
        assert_eq!(emitted, vec![0, 1, 2, 3], "and every pulse exactly once");
    }

}
