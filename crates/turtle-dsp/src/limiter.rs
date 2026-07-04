//! Master brickwall limiter (§4/§6): keeps the summed output below a linear
//! ceiling. A feed-forward peak follower with fast attack / slow release, plus
//! a hard clamp so the ceiling is never exceeded even on the first transient.
//!
//! Alloc-free and `Copy`.

use crate::util::one_pole_coeff;

/// A peak limiter with a hard ceiling.
#[derive(Debug, Clone, Copy)]
pub struct Limiter {
    threshold: f32,
    gain: f32,
    attack: f32,
    release: f32,
}

impl Limiter {
    /// `threshold` is the linear ceiling (e.g. `0.98`). Attack is the gain-
    /// reduction time; release is the recovery time.
    pub fn new(threshold: f32, sample_rate: f32, attack_ms: f32, release_ms: f32) -> Self {
        Limiter {
            threshold: threshold.max(1e-4),
            gain: 1.0,
            attack: one_pole_coeff(attack_ms, sample_rate),
            release: one_pole_coeff(release_ms, sample_rate),
        }
    }

    /// A transparent default: ceiling just below full scale, quick musical
    /// attack/release at the given sample rate.
    pub fn default_master(sample_rate: f32) -> Self {
        Limiter::new(0.98, sample_rate, 1.0, 100.0)
    }

    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let mag = x.abs();
        let target = if mag > self.threshold {
            self.threshold / mag
        } else {
            1.0
        };
        // Attack (gain falling) is fast; release (gain rising) is slow.
        let coeff = if target < self.gain {
            self.attack
        } else {
            self.release
        };
        self.gain += (target - self.gain) * coeff;
        let y = x * self.gain;
        // Brickwall safety: never exceed the ceiling, even mid-attack.
        y.clamp(-self.threshold, self.threshold)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn passes_signal_below_threshold() {
        let mut l = Limiter::new(0.98, 48_000.0, 1.0, 100.0);
        // A quiet signal is untouched (gain stays ~1).
        for _ in 0..100 {
            let y = l.process(0.5);
            assert!((y - 0.5).abs() < 1e-3);
        }
    }

    #[test]
    fn never_exceeds_ceiling() {
        let mut l = Limiter::new(0.9, 48_000.0, 1.0, 100.0);
        // Even the very first loud sample is clamped to the ceiling.
        for i in 0..1_000 {
            let x = if i % 2 == 0 { 5.0 } else { -5.0 };
            let y = l.process(x);
            assert!(y.abs() <= 0.9 + 1e-6, "overshoot at i={i}: {y}");
        }
    }

    #[test]
    fn settles_near_ceiling_on_sustained_overload() {
        let mut l = Limiter::new(0.8, 48_000.0, 1.0, 100.0);
        let mut y = 0.0;
        for _ in 0..48_000 {
            y = l.process(2.0);
        }
        // Sustained 2.0 in -> gain reduction pins output at the ceiling.
        assert!((y - 0.8).abs() < 1e-3, "expected ~0.8, got {y}");
    }
}
