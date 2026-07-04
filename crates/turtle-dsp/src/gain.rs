//! Per-pair gain + mute (§6), with a one-pole smoother so live CC moves don't
//! click. Alloc-free and `Copy`.

use crate::util::one_pole_coeff;

/// A smoothed linear gain with a hard mute. `target` is set from CC; `current`
/// ramps toward it one sample at a time.
#[derive(Debug, Clone, Copy)]
pub struct Gain {
    current: f32,
    target: f32,
    /// Per-sample smoothing coefficient in (0, 1]; 1.0 = instant.
    coeff: f32,
    muted: bool,
}

impl Gain {
    /// Unity gain, unmuted, with a smoothing time constant of `smooth_ms`.
    pub fn new(sample_rate: f32, smooth_ms: f32) -> Self {
        Gain {
            current: 1.0,
            target: 1.0,
            coeff: one_pole_coeff(smooth_ms, sample_rate),
            muted: false,
        }
    }

    /// Set the target linear gain (e.g. from a CC 0..=127 mapping).
    pub fn set_target(&mut self, gain: f32) {
        self.target = gain.max(0.0);
    }

    pub fn set_muted(&mut self, muted: bool) {
        self.muted = muted;
    }

    pub fn toggle_mute(&mut self) {
        self.muted = !self.muted;
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let goal = if self.muted { 0.0 } else { self.target };
        self.current += (goal - self.current) * self.coeff;
        x * self.current
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unity_by_default() {
        let mut g = Gain::new(48_000.0, 0.0); // instant
        assert_eq!(g.process(0.5), 0.5);
    }

    #[test]
    fn mute_ramps_to_silence() {
        let mut g = Gain::new(48_000.0, 5.0);
        g.set_muted(true);
        let mut y = 1.0;
        for _ in 0..48_000 {
            y = g.process(1.0);
        }
        assert!(y.abs() < 1e-3, "muted output should approach 0, got {y}");
    }
}
