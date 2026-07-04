//! One delay line per pair (§6): time, feedback, mix. Transparent by default
//! (mix 0, feedback 0) so the chain is inaudible until a knob is grabbed.
//!
//! The ring buffer is preallocated to a maximum delay at construction (off the
//! RT path); `process` only indexes and never allocates. Integer-sample delay
//! taps — free-ms and tempo-synced times are rounded to samples by the caller.

/// A feedback delay with a dry/wet mix.
#[derive(Debug, Clone)]
pub struct Delay {
    buf: Vec<f32>,
    write: usize,
    delay_samples: usize,
    feedback: f32,
    mix: f32,
}

impl Delay {
    /// Allocate a delay with headroom for up to `max_delay_samples` of delay.
    pub fn new(max_delay_samples: usize) -> Self {
        Delay {
            buf: vec![0.0; max_delay_samples.max(1)],
            write: 0,
            delay_samples: 0,
            feedback: 0.0,
            mix: 0.0,
        }
    }

    /// Longest delay this instance can produce (its buffer length).
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Set the delay time in samples, clamped to the allocated capacity.
    pub fn set_delay_samples(&mut self, samples: usize) {
        self.delay_samples = samples.min(self.buf.len() - 1);
    }

    /// Set the feedback amount, clamped to `[0, 0.99]` to stay stable.
    pub fn set_feedback(&mut self, feedback: f32) {
        self.feedback = feedback.clamp(0.0, 0.99);
    }

    /// Set the dry/wet mix in `[0, 1]` (0 = dry only, transparent).
    pub fn set_mix(&mut self, mix: f32) {
        self.mix = mix.clamp(0.0, 1.0);
    }

    /// Clear the delay buffer.
    pub fn reset(&mut self) {
        self.buf.iter_mut().for_each(|s| *s = 0.0);
        self.write = 0;
    }

    /// Process one sample.
    #[inline]
    pub fn process(&mut self, x: f32) -> f32 {
        let len = self.buf.len();
        let read = (self.write + len - self.delay_samples) % len;
        let delayed = self.buf[read];

        self.buf[self.write] = x + delayed * self.feedback;
        self.write = (self.write + 1) % len;

        x * (1.0 - self.mix) + delayed * self.mix
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transparent_when_mix_zero() {
        let mut d = Delay::new(1_000);
        d.set_delay_samples(100);
        d.set_feedback(0.5);
        // mix defaults to 0 -> output is exactly the dry input.
        for &x in &[0.0, 1.0, -0.3, 0.7] {
            assert_eq!(d.process(x), x);
        }
    }

    #[test]
    fn wet_tap_is_delayed() {
        let mut d = Delay::new(1_000);
        d.set_delay_samples(3);
        d.set_mix(1.0); // fully wet
        d.set_feedback(0.0);

        let out: Vec<f32> = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0]
            .iter()
            .map(|&x| d.process(x))
            .collect();
        // Impulse re-appears 3 samples later on the wet path.
        assert_eq!(out, vec![0.0, 0.0, 0.0, 1.0, 0.0, 0.0]);
    }

    #[test]
    fn feedback_decays() {
        let mut d = Delay::new(1_000);
        d.set_delay_samples(2);
        d.set_mix(1.0);
        d.set_feedback(0.5);

        let mut out = Vec::new();
        out.push(d.process(1.0));
        for _ in 0..7 {
            out.push(d.process(0.0));
        }
        // Echoes at t=2 (1.0), t=4 (0.5), t=6 (0.25), each half the last.
        assert_eq!(out[2], 1.0);
        assert!((out[4] - 0.5).abs() < 1e-6);
        assert!((out[6] - 0.25).abs() < 1e-6);
    }

    #[test]
    fn delay_time_is_clamped_to_capacity() {
        let mut d = Delay::new(16);
        d.set_delay_samples(9_999);
        assert!(d.delay_samples < d.capacity());
    }
}
