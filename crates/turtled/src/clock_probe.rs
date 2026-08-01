//! Measure MIDI-clock pulse jitter without sending anything (§5).
//!
//! # Why this exists before the feature does
//!
//! Clock pulses would be emitted from the ~1 ms control loop, the same one that
//! dispatches cued MIDI. Pulses are 20.8 ms apart at 120 BPM, so each can be up to
//! one tick late — about 5%. Whether that matters is not a question about the code:
//! a device that averages tempo over a window sees the exact mean, while one that
//! retimes on every pulse sees the full spread. A delay pedal locking its time to
//! clock is the harshest realistic case, and 5% of a 500 ms delay is 24 ms.
//!
//! So this measures first. If the spread is small the simple design stands; if it
//! is not, clock has to come off a dedicated thread sleeping to each pulse's
//! deadline, which is a bigger change and much better decided from a log than from
//! a merged PR.
//!
//! # What "lateness" means here
//!
//! For each pulse, `pos - pulse_sample(index)`: how far the transport had already
//! moved past the pulse's ideal time when the loop got round to it. Measured in
//! transport samples against the audio clock — no wall clock involved, because the
//! audio clock is what everything downstream is being synchronised *to*.
//!
//! Lateness is not itself audible; a constant lateness is just a fixed offset, which
//! `offset_ms` already exists to trim. What a device hears is the *variation*, so
//! peak-to-peak is the number that decides the design.

use std::time::Duration;

use turtle_core::timing::MidiClock;

/// How many pulses to gather before printing a line. 480 is 20 beats — ten seconds
/// at 120 BPM, often enough to watch a trend and rare enough not to flood a log.
const REPORT_EVERY: usize = 480;

/// Accumulates pulse lateness and reports it periodically.
///
/// Lives on the control thread, which runs at `SCHED_FIFO` — so this deliberately
/// does no I/O per pulse, only once per [`REPORT_EVERY`]. It is a diagnostic behind
/// `--clock-probe`, not something the daemon does in a show.
pub struct ClockProbe {
    clock: MidiClock,
    rate: u32,
    /// Lateness of each pulse in the current window, in samples. Preallocated, so
    /// the reporting path never grows it mid-set.
    lateness: Vec<u64>,
    /// Pulses seen since the probe started, across all windows.
    total: u64,
}

impl ClockProbe {
    pub fn new(bpm: f64, rate: u32) -> Self {
        ClockProbe {
            clock: MidiClock::new(bpm, rate),
            rate,
            lateness: Vec::with_capacity(REPORT_EVERY),
            total: 0,
        }
    }

    /// Point the probe at a newly armed song's tempo, discarding the part-gathered
    /// window — mixing two tempos into one report would describe neither.
    pub fn retempo(&mut self, bpm: f64) {
        self.clock = MidiClock::new(bpm, self.rate);
        self.lateness.clear();
    }

    /// A seek or a loop wrap: rebase without counting the pulses jumped over.
    pub fn reset_to(&mut self, pos: u64) {
        self.clock.reset_to(pos);
    }

    /// Call once per dispatch tick with the interpolated transport position.
    /// Returns a report line when a window completes.
    pub fn tick(&mut self, pos: u64) -> Option<String> {
        for index in self.clock.drain_due(pos) {
            // Saturating because a rebase can leave a pulse nominally ahead of the
            // position for one tick; that is a 0, not a negative.
            self.lateness.push(pos.saturating_sub(self.clock.pulse_sample(index)));
            self.total += 1;
        }
        (self.lateness.len() >= REPORT_EVERY).then(|| self.report())
    }

    /// Summarise and clear the window.
    fn report(&mut self) -> String {
        let ms = |samples: f64| samples / self.rate as f64 * 1000.0;
        self.lateness.sort_unstable();
        let n = self.lateness.len();
        let min = self.lateness[0] as f64;
        let max = self.lateness[n - 1] as f64;
        // Nearest-rank p99: the value below which 99% of pulses fall.
        let p99 = self.lateness[(n * 99 / 100).min(n - 1)] as f64;
        let mean = self.lateness.iter().sum::<u64>() as f64 / n as f64;
        let nominal = self.clock.samples_per_pulse();
        // Peak-to-peak is the number that decides the design: a constant lateness
        // is a fixed offset (trimmable with `offset_ms`), whereas the variation is
        // what a device retiming per pulse actually hears.
        let spread = max - min;
        let line = format!(
            "[clock] {} pulses  nominal {:.3}ms  late min {:.3} mean {:.3} p99 {:.3} max {:.3}ms  \
             peak-to-peak {:.3}ms ({:.1}% of a pulse)",
            self.total,
            ms(nominal),
            ms(min),
            ms(mean),
            ms(p99),
            ms(max),
            ms(spread),
            spread / nominal * 100.0,
        );
        self.lateness.clear();
        line
    }

    /// The dispatch interval this probe is measuring against, for the banner.
    pub fn tick_interval(&self) -> Duration {
        Duration::from_millis(1)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Ticking exactly on the pulse times must show zero lateness — if the ideal
    /// case does not read as 0, every real measurement is meaningless.
    #[test]
    fn a_perfectly_timed_tick_reports_no_lateness() {
        let mut p = ClockProbe::new(120.0, 48_000);
        let mut line = None;
        // 1000 samples per pulse at this tempo.
        for i in 0..=REPORT_EVERY {
            if let Some(l) = p.tick(i as u64 * 1000) {
                line = Some(l);
            }
        }
        let line = line.expect("a window should have completed");
        assert!(line.contains("peak-to-peak 0.000ms"), "{line}");
        assert!(line.contains("nominal 20.833ms"), "{line}");
    }

    /// The realistic case: a 1 ms tick against a 20.833 ms pulse. The probe must
    /// show a spread near one tick — that is the whole measurement.
    #[test]
    fn a_one_millisecond_tick_shows_about_one_tick_of_spread() {
        let rate = 48_000u64;
        let mut p = ClockProbe::new(120.0, rate as u32);
        let mut line = None;
        // 48 samples = 1 ms.
        for tick in 0..(REPORT_EVERY as u64 * 21 + 100) {
            if let Some(l) = p.tick(tick * 48) {
                line = Some(l);
            }
        }
        let line = line.expect("a window should have completed");
        // Parse the peak-to-peak back out rather than eyeballing the string.
        let pp: f64 = line
            .split("peak-to-peak ")
            .nth(1)
            .and_then(|s| s.split("ms").next())
            .and_then(|s| s.parse().ok())
            .expect("peak-to-peak should be parseable");
        assert!(
            (0.5..=1.1).contains(&pp),
            "a 1ms tick should spread pulses by about 1ms, got {pp}: {line}"
        );
    }

    /// A rebase must not bill the probe for pulses that were jumped over — a loop
    /// wrap would otherwise report a burst of enormous lateness that never happened.
    #[test]
    fn a_rebase_does_not_count_the_pulses_jumped_over() {
        let mut p = ClockProbe::new(120.0, 48_000);
        p.tick(1000);
        let before = p.total;
        p.reset_to(48_000);
        p.tick(48_000);
        assert_eq!(p.total, before + 1, "only the pulse at the new position counts");
    }

    /// Windows must not straddle a tempo change: the nominal interval differs, so
    /// the two halves are not comparable.
    #[test]
    fn a_tempo_change_discards_the_part_gathered_window() {
        let mut p = ClockProbe::new(120.0, 48_000);
        for i in 0..10u64 {
            p.tick(i * 1000);
        }
        assert!(!p.lateness.is_empty());
        p.retempo(140.0);
        assert!(p.lateness.is_empty(), "the old tempo's samples must be dropped");
    }
}
