//! The decoupled transport clock (spec §3.1).
//!
//! Each audio period the RT thread publishes `(sample_pos, monotonic_ns)`. The
//! MIDI scheduler reads that anchor and interpolates the current sample
//! position between periods, so MIDI timing granularity (~1 ms) is decoupled
//! from the audio buffer size:
//!
//! ```text
//! pos = last_sample_pos + (now_ns - last_ns) * Fs / 1e9
//! ```
//!
//! NOTE: the skeleton publishes two atomics independently. A production build
//! must make the pair tear-proof (a seqlock / versioned publish) so the reader
//! never mixes a new `sample_pos` with an old `anchor_ns`.

use std::sync::atomic::{AtomicU64, Ordering};

pub struct TransportClock {
    sample_pos: AtomicU64,
    anchor_ns: AtomicU64,
    sample_rate: u32,
}

impl TransportClock {
    pub fn new(sample_rate: u32) -> Self {
        TransportClock {
            sample_pos: AtomicU64::new(0),
            anchor_ns: AtomicU64::new(0),
            sample_rate,
        }
    }

    /// Publish the latest anchor. Called from the audio RT thread each period.
    pub fn publish(&self, sample_pos: u64, monotonic_ns: u64) {
        self.anchor_ns.store(monotonic_ns, Ordering::Release);
        self.sample_pos.store(sample_pos, Ordering::Release);
    }

    /// Interpolate the sample position at `now_ns`. Called from the scheduler.
    pub fn interpolate(&self, now_ns: u64) -> u64 {
        let pos = self.sample_pos.load(Ordering::Acquire);
        let anchor = self.anchor_ns.load(Ordering::Acquire);
        let dt_ns = now_ns.saturating_sub(anchor) as u128;
        pos + (dt_ns * self.sample_rate as u128 / 1_000_000_000) as u64
    }

    pub fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_forward_from_anchor() {
        let clk = TransportClock::new(48_000);
        clk.publish(1_000, 5_000_000_000);
        // Exactly at the anchor: the published position.
        assert_eq!(clk.interpolate(5_000_000_000), 1_000);
        // One second later: +48000 samples.
        assert_eq!(clk.interpolate(6_000_000_000), 49_000);
        // Half a millisecond later: +24 samples.
        assert_eq!(clk.interpolate(5_000_500_000), 1_024);
    }

    #[test]
    fn clamps_time_before_anchor() {
        let clk = TransportClock::new(48_000);
        clk.publish(1_000, 5_000_000_000);
        // A now_ns before the anchor must not underflow.
        assert_eq!(clk.interpolate(4_000_000_000), 1_000);
    }
}
