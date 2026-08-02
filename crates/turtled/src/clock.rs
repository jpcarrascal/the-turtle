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
//! # Why the pair needs a seqlock
//!
//! The two values are only meaningful **together**: `sample_pos` is the position
//! *as of* `anchor_ns`. Published as two independent atomic stores, a reader can
//! land between them and get a new `sample_pos` with a stale `anchor_ns` — and
//! then `now_ns - anchor_ns` covers one period too much, so the interpolated
//! position is off by a whole audio period. At 1024 frames / 48 kHz that is
//! **±21 ms**, against §3.1's design tolerance of low single-digit milliseconds.
//! It is not a small error in the right answer; it is the wrong answer.
//!
//! So the pair is published under a **seqlock**: an odd sequence number marks a
//! write in progress, and a reader that sees the counter change across its read
//! (or catches it mid-write) retries. [`TransportClock::snapshot`] is therefore
//! the only way to read the pair — there is deliberately no accessor for one half
//! alone, because half of this pair is meaningless.
//!
//! ## Why a seqlock rather than a mutex
//!
//! The writer is the audio RT thread, and it **must never wait**. A seqlock is
//! wait-free for the writer: three relaxed stores and a fence, no blocking, no
//! syscall, no allocation. Only the *reader* can be made to retry, and the reader
//! is the MIDI thread, which has a millisecond of slack. A mutex would invert
//! that — exactly the priority inversion avoided for the status snapshot in
//! [`crate::socket`].

use std::sync::atomic::{fence, AtomicU64, Ordering};

pub struct TransportClock {
    /// Seqlock counter. **Even** = the pair below is stable; **odd** = a write is
    /// in progress. Incremented twice per publish, so it is even at rest.
    seq: AtomicU64,
    sample_pos: AtomicU64,
    /// Frames rendered since the transport started — the same instant as
    /// `sample_pos`, but **monotonic**: it never wraps at a loop point (§5.1).
    ///
    /// `sample_pos` answers "where in the song are we", which is what MIDI cues
    /// need. Musical time needs "how much music has played", and reconstructing
    /// that by watching `sample_pos` jump backwards cannot be made exact — the
    /// position is interpolated, so it overshoots the loop end before a wrap is
    /// noticed, and the overshoot is lost. Publishing both costs one more atomic
    /// store per period and removes the reconstruction entirely.
    rendered: AtomicU64,
    anchor_ns: AtomicU64,
    sample_rate: u32,
}

impl TransportClock {
    pub fn new(sample_rate: u32) -> Self {
        TransportClock {
            seq: AtomicU64::new(0),
            sample_pos: AtomicU64::new(0),
            rendered: AtomicU64::new(0),
            anchor_ns: AtomicU64::new(0),
            sample_rate,
        }
    }

    /// Publish the latest anchor. Called from the audio RT thread each period.
    ///
    /// Wait-free: it never spins, blocks, allocates, or makes a syscall, which is
    /// what makes it safe on the RT path. Single-writer by design — the audio
    /// thread is the only caller, and two concurrent writers would corrupt the
    /// counter's parity.
    pub fn publish(&self, sample_pos: u64, rendered: u64, monotonic_ns: u64) {
        // Relaxed is enough to read our own counter: this thread is the only
        // writer, so no other thread can have changed it.
        let s = self.seq.load(Ordering::Relaxed);
        debug_assert!(s.is_multiple_of(2), "publish called concurrently (seq was odd)");

        // Go odd: readers that arrive from here on will retry rather than trust
        // what they read.
        self.seq.store(s.wrapping_add(1), Ordering::Relaxed);
        // Keep the "seq is odd" store above from being reordered after the data
        // stores below — without this a reader could see old-seq with new-data.
        fence(Ordering::Release);

        self.sample_pos.store(sample_pos, Ordering::Relaxed);
        self.rendered.store(rendered, Ordering::Relaxed);
        self.anchor_ns.store(monotonic_ns, Ordering::Relaxed);

        // Release: everything above is visible to any thread that observes this
        // (even) counter value. `wrapping_add` so the counter cannot overflow-panic
        // in debug; 2^64 is even, so parity survives the wrap.
        self.seq.store(s.wrapping_add(2), Ordering::Release);
    }

    /// Read the `(sample_pos, anchor_ns)` pair, retrying until it is consistent.
    ///
    /// The retry loop is unbounded, which is safe here because the writer is the
    /// *highest-priority* thread in the process and holds the odd state for two
    /// stores: it cannot be waiting on this reader, so the state it is waiting for
    /// always arrives. There is deliberately no bounded-retry fallback, because
    /// any value it could return would be either torn or stale — reintroducing
    /// the bug this exists to prevent.
    pub fn snapshot(&self) -> (u64, u64) {
        let (pos, _, anchor) = self.snapshot_all();
        (pos, anchor)
    }

    /// The full anchor: `(sample_pos, rendered, anchor_ns)`.
    pub fn snapshot_all(&self) -> (u64, u64, u64) {
        loop {
            let s1 = self.seq.load(Ordering::Acquire);
            // Odd: a write is in flight. Don't even read the data.
            if !s1.is_multiple_of(2) {
                std::hint::spin_loop();
                continue;
            }

            let pos = self.sample_pos.load(Ordering::Relaxed);
            let rendered = self.rendered.load(Ordering::Relaxed);
            let anchor = self.anchor_ns.load(Ordering::Relaxed);

            // Order the data loads *before* re-reading the counter, so a write
            // that landed during them cannot be missed.
            fence(Ordering::Acquire);
            if self.seq.load(Ordering::Relaxed) == s1 {
                return (pos, rendered, anchor);
            }
            // The counter moved: a publish overlapped this read. Try again.
            std::hint::spin_loop();
        }
    }

    /// Interpolate the sample position at `now_ns`. Called from the scheduler.
    pub fn interpolate(&self, now_ns: u64) -> u64 {
        let (pos, anchor) = self.snapshot();
        let dt_ns = now_ns.saturating_sub(anchor) as u128;
        pos + (dt_ns * self.sample_rate as u128 / 1_000_000_000) as u64
    }

    /// Musical time since the transport started, in frames, interpolated to
    /// `now_ns`. Monotonic across loop wraps — this is what MIDI clock runs on.
    pub fn elapsed(&self, now_ns: u64) -> u64 {
        let (_, rendered, anchor) = self.snapshot_all();
        let dt_ns = now_ns.saturating_sub(anchor) as u128;
        rendered + (dt_ns * self.sample_rate as u128 / 1_000_000_000) as u64
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
        clk.publish(1_000, 1_000, 5_000_000_000);
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
        clk.publish(1_000, 1_000, 5_000_000_000);
        // A now_ns before the anchor must not underflow.
        assert_eq!(clk.interpolate(4_000_000_000), 1_000);
    }

    /// The counter must be even at rest, or every reader would spin forever.
    #[test]
    fn the_sequence_counter_returns_to_even_after_a_publish() {
        let clk = TransportClock::new(48_000);
        assert!(clk.seq.load(Ordering::Relaxed).is_multiple_of(2), "starts even");
        clk.publish(1, 1, 2);
        assert!(
            clk.seq.load(Ordering::Relaxed).is_multiple_of(2),
            "even after publish"
        );
        clk.publish(3, 3, 4);
        assert_eq!(clk.seq.load(Ordering::Relaxed), 4, "two increments per publish");
    }

    /// A snapshot of an untouched clock must still be readable rather than
    /// spinning — the scheduler reads the clock before the first period lands.
    #[test]
    fn a_fresh_clock_snapshots_without_spinning() {
        assert_eq!(TransportClock::new(48_000).snapshot(), (0, 0));
    }

    /// The regression test for the bug this module's seqlock exists to prevent.
    ///
    /// A writer publishes pairs satisfying a known invariant (`anchor == pos *
    /// 1000`) as fast as it can while a reader checks it. Any violation means the
    /// reader mixed halves of two different publishes.
    ///
    /// Against the previous two-independent-stores implementation this fails
    /// immediately and hard — measured at **~42% of reads torn** on an 8-core dev
    /// machine. That rate is an artefact of a writer that does nothing but
    /// publish; in production the writer publishes once per ~21 ms period, so the
    /// real-world rate is far lower (order of one tear per hour of playback). The
    /// point of the stress loop is to make a rare race *reliably* detectable, not
    /// to estimate its frequency.
    #[test]
    fn concurrent_reads_never_tear() {
        use std::sync::atomic::AtomicBool;
        use std::sync::Arc;

        let clock = Arc::new(TransportClock::new(48_000));
        let stop = Arc::new(AtomicBool::new(false));

        let writer = {
            let clock = Arc::clone(&clock);
            let stop = Arc::clone(&stop);
            std::thread::spawn(move || {
                let mut i = 1u64;
                while !stop.load(Ordering::Relaxed) {
                    clock.publish(i, i, i * 1000);
                    i = i.wrapping_add(1);
                }
            })
        };

        // Read on this thread so a failure's panic is attributed to the test.
        let deadline = std::time::Instant::now() + std::time::Duration::from_millis(300);
        let mut reads = 0u64;
        while std::time::Instant::now() < deadline {
            let (pos, anchor) = clock.snapshot();
            reads += 1;
            assert_eq!(
                anchor,
                pos * 1000,
                "torn read after {reads} reads: pos={pos} anchor={anchor}"
            );
        }

        stop.store(true, Ordering::Relaxed);
        writer.join().unwrap();
        // Guard against the test silently passing by never actually racing.
        assert!(reads > 10_000, "only {reads} reads; the probe was too weak to prove anything");
    }
    /// Musical time must not wrap when the song does — that is the whole reason it
    /// is published separately from `sample_pos`.
    #[test]
    fn elapsed_is_monotonic_where_the_position_wraps() {
        let clk = TransportClock::new(48_000);
        // End of a loop, then the wrap: the position goes back to near zero while
        // musical time carries on.
        clk.publish(767_000, 767_000, 1_000_000_000);
        assert_eq!(clk.interpolate(1_000_000_000), 767_000);
        assert_eq!(clk.elapsed(1_000_000_000), 767_000);

        clk.publish(1_000, 769_000, 2_000_000_000);
        assert_eq!(clk.interpolate(2_000_000_000), 1_000, "position wrapped");
        assert_eq!(clk.elapsed(2_000_000_000), 769_000, "musical time did not");

        // And it interpolates from the monotonic anchor, not the wrapped one.
        assert_eq!(clk.elapsed(2_000_500_000), 769_024);
    }

}
