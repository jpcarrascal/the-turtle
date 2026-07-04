//! One biquad per pair (§6), transposed direct-form II. RBJ cookbook coefficients.

use std::f32::consts::PI;

/// Filter response type. One per pair, fixed at song load; `cutoff`/`resonance`
/// are then driven live (§6).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FilterType {
    Lowpass,
    Highpass,
    Bandpass,
}

/// A single biquad section. `state` is the only mutable per-sample data, so the
/// struct is `Copy` and lives inline in the RT chain — no allocation.
#[derive(Debug, Clone, Copy)]
pub struct Biquad {
    // Normalized coefficients (a0 folded to 1).
    b0: f32,
    b1: f32,
    b2: f32,
    a1: f32,
    a2: f32,
    // Transposed direct-form II state.
    z1: f32,
    z2: f32,
}

impl Biquad {
    /// A flat (unity, no-op) biquad — the transparent default before a knob is
    /// grabbed (§6).
    pub fn identity() -> Self {
        Biquad {
            b0: 1.0,
            b1: 0.0,
            b2: 0.0,
            a1: 0.0,
            a2: 0.0,
            z1: 0.0,
            z2: 0.0,
        }
    }

    /// Build from an RBJ design and reset state.
    pub fn new(kind: FilterType, cutoff_hz: f32, q: f32, sample_rate: f32) -> Self {
        let mut b = Biquad::identity();
        b.set(kind, cutoff_hz, q, sample_rate);
        b
    }

    /// Recompute coefficients in place (state preserved) — safe to call from the
    /// RT thread when a CC moves the cutoff/resonance. No allocation.
    pub fn set(&mut self, kind: FilterType, cutoff_hz: f32, q: f32, sample_rate: f32) {
        // Clamp to a sane, stable range.
        let q = q.max(1e-4);
        let nyquist = sample_rate * 0.5;
        let f0 = cutoff_hz.clamp(1.0, nyquist - 1.0);

        let w0 = 2.0 * PI * f0 / sample_rate;
        let (sin_w0, cos_w0) = w0.sin_cos();
        let alpha = sin_w0 / (2.0 * q);

        let (b0, b1, b2) = match kind {
            FilterType::Lowpass => {
                let x = (1.0 - cos_w0) * 0.5;
                (x, 1.0 - cos_w0, x)
            }
            FilterType::Highpass => {
                let x = (1.0 + cos_w0) * 0.5;
                (x, -(1.0 + cos_w0), x)
            }
            FilterType::Bandpass => (alpha, 0.0, -alpha), // constant 0 dB peak gain
        };
        let a0 = 1.0 + alpha;
        let a1 = -2.0 * cos_w0;
        let a2 = 1.0 - alpha;

        self.b0 = b0 / a0;
        self.b1 = b1 / a0;
        self.b2 = b2 / a0;
        self.a1 = a1 / a0;
        self.a2 = a2 / a0;
    }

    /// Clear the filter state (e.g. between songs).
    pub fn reset(&mut self) {
        self.z1 = 0.0;
        self.z2 = 0.0;
    }

    /// Process one sample.
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let y = self.b0 * x + self.z1;
        self.z1 = self.b1 * x - self.a1 * y + self.z2;
        self.z2 = self.b2 * x - self.a2 * y;
        y
    }

    /// Process a block in place.
    pub fn process_block(&mut self, buf: &mut [f32]) {
        for x in buf.iter_mut() {
            *x = self.process(*x);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identity_passes_signal_unchanged() {
        let mut b = Biquad::identity();
        for &x in &[0.0, 1.0, -0.5, 0.25] {
            assert_eq!(b.process(x), x);
        }
    }

    #[test]
    fn lowpass_passes_dc() {
        // A lowpass has unity gain at DC: a constant input converges to itself.
        let mut b = Biquad::new(FilterType::Lowpass, 1_000.0, 0.707, 48_000.0);
        let mut y = 0.0;
        for _ in 0..2_000 {
            y = b.process(1.0);
        }
        assert!((y - 1.0).abs() < 1e-3, "DC gain should be ~1, got {y}");
    }

    #[test]
    fn highpass_blocks_dc() {
        // A highpass has zero gain at DC: a constant input decays to ~0.
        let mut b = Biquad::new(FilterType::Highpass, 1_000.0, 0.707, 48_000.0);
        let mut y = 0.0;
        for _ in 0..2_000 {
            y = b.process(1.0);
        }
        assert!(y.abs() < 1e-3, "DC should be blocked, got {y}");
    }

    #[test]
    fn stays_finite_under_sweep() {
        // Sweep the cutoff live (as CC would) and ensure no NaN/inf.
        let mut b = Biquad::new(FilterType::Lowpass, 200.0, 4.0, 48_000.0);
        for i in 0..48_000 {
            let cutoff = 100.0 + (i as f32 * 0.4);
            b.set(FilterType::Lowpass, cutoff, 4.0, 48_000.0);
            let y = b.process((i as f32 * 0.01).sin());
            assert!(y.is_finite(), "output went non-finite at i={i}");
        }
    }
}
