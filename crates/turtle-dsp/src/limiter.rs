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

/// A **linked** stereo limiter: one gain reduction, applied to both channels.
///
/// # Why linked
///
/// Two independent mono limiters reduce each channel by whatever that channel
/// needs, so a loud left and a quiet right get different gains — and the stereo
/// image shifts toward the quieter side exactly when the music is loudest. A linked
/// limiter detects on the louder channel and applies that one reduction to both, so
/// the balance between them is preserved and only the level changes.
///
/// This matters more since the delay became a shared bus (§6): four pairs summing
/// into one send bus pushes the master harder than four independent inserts did, so
/// heavy limiting is no longer a corner case.
///
/// # It is also cheaper
///
/// The expensive part of limiting is the `threshold / mag` divide and the envelope
/// update, and unlinked does both **twice** per sample. Linked does them once, on
/// `max(|L|, |R|)`, then applies the result with two multiplies. So the better
/// behaviour costs less rather than more.
#[derive(Debug, Clone, Copy)]
pub struct StereoLimiter {
    threshold: f32,
    gain: f32,
    attack: f32,
    release: f32,
}

impl StereoLimiter {
    pub fn new(threshold: f32, sample_rate: f32, attack_ms: f32, release_ms: f32) -> Self {
        StereoLimiter {
            threshold: threshold.max(1e-4),
            gain: 1.0,
            attack: one_pole_coeff(attack_ms, sample_rate),
            release: one_pole_coeff(release_ms, sample_rate),
        }
    }

    /// Same transparent default as [`Limiter::default_master`].
    pub fn default_master(sample_rate: f32) -> Self {
        StereoLimiter::new(0.98, sample_rate, 1.0, 100.0)
    }

    /// Limit one stereo frame, returning `(left, right)`.
    #[inline]
    pub fn process(&mut self, l: f32, r: f32) -> (f32, f32) {
        // Detect on whichever channel is louder: that is what decides how much
        // reduction the *pair* needs.
        let mag = l.abs().max(r.abs());
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
        // One gain, both channels — this is what keeps the image stable.
        let (yl, yr) = (l * self.gain, r * self.gain);
        // Brickwall safety: never exceed the ceiling, even mid-attack.
        (
            yl.clamp(-self.threshold, self.threshold),
            yr.clamp(-self.threshold, self.threshold),
        )
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

    /// The property that makes linking worth doing: heavy limiting must change the
    /// *level* without changing the balance between channels.
    ///
    /// Two independent mono limiters fail this — a loud left is reduced while a quiet
    /// right is not, so the image slides toward the right exactly when the music is
    /// loudest. Compared against that here rather than asserted in the abstract.
    #[test]
    fn linking_preserves_the_stereo_image_under_heavy_limiting() {
        let (loud, quiet) = (1.5f32, 0.15f32); // 10:1, and well over the ceiling
        let input_ratio = loud / quiet;

        let mut linked = StereoLimiter::new(0.9, 48_000.0, 1.0, 100.0);
        let mut mono_l = Limiter::new(0.9, 48_000.0, 1.0, 100.0);
        let mut mono_r = Limiter::new(0.9, 48_000.0, 1.0, 100.0);

        // Settle past the attack so both are in steady-state reduction.
        let mut linked_out = (0.0, 0.0);
        let mut unlinked_out = (0.0, 0.0);
        for _ in 0..5_000 {
            linked_out = linked.process(loud, quiet);
            unlinked_out = (mono_l.process(loud), mono_r.process(quiet));
        }

        let linked_ratio = linked_out.0 / linked_out.1;
        let unlinked_ratio = unlinked_out.0 / unlinked_out.1;

        // Linked: the balance survives.
        assert!(
            (linked_ratio - input_ratio).abs() / input_ratio < 0.02,
            "linked should preserve the 10:1 balance, got {linked_ratio:.2}"
        );
        // Unlinked: it does not — which is the bug being fixed, demonstrated rather
        // than assumed.
        assert!(
            (unlinked_ratio - input_ratio).abs() / input_ratio > 0.2,
            "unlinked should visibly distort the balance, got {unlinked_ratio:.2} \
             (if this fails, the two are behaving alike and the test proves nothing)"
        );
    }

    /// Linking must not weaken the brickwall guarantee: neither channel may exceed
    /// the ceiling, including the channel that was not the one detected on.
    #[test]
    fn linked_limiting_never_exceeds_the_ceiling_on_either_channel() {
        let mut l = StereoLimiter::new(0.9, 48_000.0, 1.0, 100.0);
        for i in 0..5_000 {
            // Alternate which channel is the loud one, so the detector has to track
            // whichever is currently louder.
            let (a, b) = if i % 2 == 0 { (3.0, 0.2) } else { (0.2, 3.0) };
            let (yl, yr) = l.process(a, b);
            assert!(yl.abs() <= 0.9 + 1e-6, "left exceeded the ceiling at {i}: {yl}");
            assert!(yr.abs() <= 0.9 + 1e-6, "right exceeded the ceiling at {i}: {yr}");
        }
    }

    /// Below the ceiling it must be transparent, like the mono version.
    #[test]
    fn linked_passes_quiet_signal_untouched() {
        let mut l = StereoLimiter::new(0.98, 48_000.0, 1.0, 100.0);
        for _ in 0..100 {
            let (yl, yr) = l.process(0.5, -0.25);
            assert!((yl - 0.5).abs() < 1e-3, "{yl}");
            assert!((yr + 0.25).abs() < 1e-3, "{yr}");
        }
    }
}
