//! A feedback delay with a **tape-style** variable delay time (§6).
//!
//! Transparent by default (mix 0, feedback 0) so the chain is inaudible until a
//! knob is grabbed.
//!
//! # Why the read tap glides instead of jumping
//!
//! Setting the delay time used to move the read tap instantly. That is a
//! discontinuity in the signal — an audible click — and with the shared delay bus
//! (§6) *everything* routed to the delay clicks at once, so it stops being a
//! papercut and becomes the loudest thing in the room.
//!
//! Instead the tap **glides** toward its target over [`GLIDE_MS`], reading at a
//! fractional position with linear interpolation. That is how a tape echo behaves
//! when you change the head spacing: the pitch bends during the move and settles
//! at the new time. It is a musical effect people deliberately reach for, and it
//! is also the *cheaper* of the two ways to avoid the click.
//!
//! # Why this is cheaper than crossfading
//!
//! The obvious alternative is to keep two taps and crossfade between them. That
//! needs a second read at the **old** tap position, which is far away in the ring
//! buffer — a different cache line, quite possibly a miss. On a Pi 4, where this
//! buffer already dominates the delay's cost, that is the expensive part.
//!
//! Interpolation reads `buf[i]` and `buf[i + 1]`: **adjacent**, so the second read
//! is almost always in the cache line the first one just pulled in. One nearly-free
//! read and about three extra flops, with no fade state and no branching.
//!
//! The output lowpass (§6) lives here too — see [`Delay::set_output_filter`]
//! for why it is on the output rather than in the loop.
//!
//! # Why the glide is linear, not exponential
//!
//! The rest of this codebase smooths parameters with a one-pole (`Gain`, the live
//! filter cutoff). That is wrong here, for a reason that only showed up in a test:
//! a one-pole *asymptotes*. Its "150 ms" is a time constant, so it is still ~6
//! samples short after a full second and needs about **two seconds** to land
//! exactly.
//!
//! For a tempo-synced delay that is both sluggish and wrong: a delay time that
//! settles a fraction short is permanently a fraction out of time with the song.
//! A linear ramp arrives **exactly**, after exactly [`GLIDE_MS`], whatever the
//! distance — and a constant-rate tap movement is also the more faithful tape
//! behaviour, since it gives a steady detune during the move rather than a swoop
//! that eases out.

use crate::biquad::{Biquad, FilterType};

/// How long the read tap takes to glide to a new delay time.
///
/// Sets the character of the pitch bend: much faster and a time change is a
/// chirp rather than a slide; much slower and it feels sluggish under a foot
/// controller. ~150 ms is in the range analogue tape units actually move.
pub const GLIDE_MS: f32 = 150.0;

/// Below this magnitude, what is written back into the buffer is flushed to zero.
///
/// A decaying tail multiplies by `feedback` on every pass, so it heads toward zero
/// asymptotically and eventually reaches **denormal** floats, which are
/// dramatically slow to compute on some CPUs — on the RT thread, of all places.
/// This matters more now that the tail keeps running while stopped (§6): with
/// nothing to terminate it, a quiet delay would sit in denormal territory
/// indefinitely rather than for a moment.
///
/// 1e-20 is ~-400 dBFS — utterly inaudible, and eighteen orders of magnitude above
/// where f32 denormals begin (~1.2e-38), so the buffer drains to true zeros well
/// before the slow path is ever reached.
const DENORMAL_FLOOR: f32 = 1e-20;

/// A feedback delay with a dry/wet mix, a gliding (tape-style) delay time, and an
/// optional lowpass **on its output**.
///
/// The filter's placement is a musical choice, and this is the one JP asked for.
/// On the output, every repeat is filtered identically — a tone control for the
/// wet signal. In the *feedback loop* instead, each repeat would pass through
/// again and darken cumulatively (tape/BBD character). The loop version was tried
/// first and rejected on taste.
///
/// One consequence worth knowing: out of the loop, resonance can no longer
/// multiply the loop gain, so the delay cannot be driven unstable by the filter
/// knobs. That was a real hazard in the feedback version, and it is why there is no
/// gain compensation here — see [`Delay::set_output_filter`].
#[derive(Debug, Clone)]
pub struct Delay {
    buf: Vec<f32>,
    write: usize,
    /// Where the tap is *heading*, in samples. Set by [`Delay::set_delay_samples`].
    target_samples: f32,
    /// Where the tap actually is right now, in samples — fractional, because it
    /// is mid-glide most of the time it is changing.
    current_samples: f32,
    /// How far the tap moves per sample while gliding, signed. Recomputed when
    /// the target changes so the glide always takes [`GLIDE_MS`] regardless of
    /// distance; `0.0` means settled.
    step_per_sample: f32,
    /// Glide duration in samples, kept to recompute `step_per_sample`.
    glide_samples: f32,
    /// Whether a delay time has ever been set.
    ///
    /// The first one **jumps**: there is no previous time to glide from, so
    /// gliding would swoop up from zero the first time the knob is touched, which
    /// is not tape behaviour — it is just wrong. Every later change glides.
    time_set: bool,
    feedback: f32,
    mix: f32,
    /// Applied to the delayed signal on its way out, so every repeat is filtered
    /// the same. Not in the feedback path — see the type docs.
    output_filter: Biquad,
    /// Whether the filter does anything. Transparent by default, and skipped
    /// entirely when so: an identity biquad still costs 5 multiplies and 4 adds
    /// per sample per channel, which is not nothing on an RT path.
    filter_live: bool,
    sample_rate: f32,
}

impl Delay {
    /// Allocate a delay with headroom for up to `max_delay_samples` of delay.
    ///
    /// `sample_rate` is needed for the glide time constant; it does not change
    /// the buffer size, which the caller sizes for the longest delay it will ask
    /// for (§6: a whole note at the song's tempo).
    pub fn new(max_delay_samples: usize, sample_rate: f32) -> Self {
        Delay {
            buf: vec![0.0; max_delay_samples.max(2)],
            write: 0,
            target_samples: 0.0,
            current_samples: 0.0,
            step_per_sample: 0.0,
            glide_samples: (GLIDE_MS * 0.001 * sample_rate).max(1.0),
            time_set: false,
            feedback: 0.0,
            mix: 0.0,
            output_filter: Biquad::identity(),
            filter_live: false,
            sample_rate,
        }
    }

    /// Longest delay this instance can produce (its buffer length).
    pub fn capacity(&self) -> usize {
        self.buf.len()
    }

    /// Set the delay time in samples, clamped to the allocated capacity.
    ///
    /// The tap *glides* here rather than jumping — see the module docs. Use
    /// [`Delay::jump_to_delay_samples`] when an instant move is what you want.
    pub fn set_delay_samples(&mut self, samples: usize) {
        // The very first time, jump: there is nothing to glide *from*.
        if !self.time_set {
            self.jump_to_delay_samples(samples);
            return;
        }
        self.target_samples = self.clamp_samples(samples as f32);
        // One division per parameter change, not per sample.
        self.step_per_sample = (self.target_samples - self.current_samples) / self.glide_samples;
    }

    /// Move the tap immediately, with no glide.
    ///
    /// For setting an initial time at load, where there is no previous value to
    /// glide from and a swoop from zero on the first note would be wrong.
    pub fn jump_to_delay_samples(&mut self, samples: usize) {
        self.target_samples = self.clamp_samples(samples as f32);
        self.current_samples = self.target_samples;
        self.step_per_sample = 0.0;
        self.time_set = true;
    }

    /// One less than the buffer length: interpolation reads `i` and `i + 1`, so
    /// the tap must leave room for that second sample.
    fn clamp_samples(&self, samples: f32) -> f32 {
        samples.clamp(0.0, (self.buf.len() - 2) as f32)
    }

    /// The tap's current position in samples (fractional while gliding).
    pub fn current_delay_samples(&self) -> f32 {
        self.current_samples
    }

    /// Set the feedback amount, clamped to `[0, 0.99]` to stay stable.
    pub fn set_feedback(&mut self, feedback: f32) {
        self.feedback = feedback.clamp(0.0, 0.99);
    }

    /// Set the dry/wet mix in `[0, 1]` (0 = dry only, transparent).
    pub fn set_mix(&mut self, mix: f32) {
        self.mix = mix.clamp(0.0, 1.0);
    }

    /// Set the lowpass on the delay's output: every repeat is filtered the same (§6).
    ///
    /// Coefficients are recomputed here, off the per-sample path — a CC move costs
    /// one recompute, not one per sample. **`Q` is free** to run: it only changes
    /// the coefficients, so an adjustable-resonance filter is exactly as cheap as
    /// a fixed one. There is no cheaper "fixed frequency" version worth having.
    ///
    /// # There is deliberately no resonant-gain compensation
    ///
    /// An earlier version scaled the output by `1/Q`, on the theory that a resonant
    /// lowpass peaks at about `Q` and would otherwise make the resonance knob shift
    /// the delay's level. That was wrong twice over, and both showed up in use.
    ///
    /// It over-corrected by an order of magnitude. A lowpass's **passband** gain is
    /// 1.0 whatever `Q` is — resonance boosts only a narrow band near the cutoff —
    /// so dividing the *whole* signal by `Q` attenuated everything by up to 20 dB to
    /// offset a peak that affects a small slice of the spectrum. For broadband
    /// material the real level rise from resonance is a few dB.
    ///
    /// And it made the top of the cutoff sweep discontinuous. CC 127 maps exactly
    /// onto the maximum cutoff, which bypasses the filter — so the compensation
    /// vanished in one step and the level jumped by exactly `Q`, up to +20 dB. That
    /// is what a "large jump in volume, like the filter had suddenly turned off"
    /// turns out to be: the filter had suddenly turned off.
    pub fn set_output_filter(&mut self, cutoff_hz: f32, q: f32) {
        self.output_filter
            .set(FilterType::Lowpass, cutoff_hz, q, self.sample_rate);
        self.filter_live = true;
    }

    /// Remove the output filter, restoring bit-exact passthrough.
    pub fn clear_output_filter(&mut self) {
        self.output_filter = Biquad::identity();
        self.filter_live = false;
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

        // Glide the tap one step toward its target. Settled is the common case and
        // costs one compare against zero.
        if self.step_per_sample != 0.0 {
            self.current_samples += self.step_per_sample;
            // Stop exactly on the target rather than stepping past it — a linear
            // ramp overshoots on its last step otherwise, and "exactly" is the
            // whole reason this is not a one-pole.
            let overshot = (self.target_samples - self.current_samples).signum()
                != self.step_per_sample.signum();
            if overshot {
                self.current_samples = self.target_samples;
                self.step_per_sample = 0.0;
            }
        }

        // Split the fractional tap into an integer sample and a fraction.
        let whole = self.current_samples as usize;
        let frac = self.current_samples - whole as f32;

        // Two adjacent taps, `whole` and `whole + 1` samples back. Computed by
        // subtraction with a conditional wrap rather than `%`: an integer division
        // per sample is tens of cycles on ARM, and this runs 48000 times a second
        // per channel.
        let i0 = wrap_back(self.write, whole, len);
        let i1 = wrap_back(self.write, whole + 1, len);
        let a = self.buf[i0];
        let b = self.buf[i1];
        // `i1` is one sample *further back*, so it is the older of the two: the
        // fraction interpolates from `a` toward `b` as the tap moves back.
        let delayed = a + (b - a) * frac;

        // Feedback is taken *before* the filter, so the loop stays clean and each
        // repeat is a faithful copy — the filter colours what comes out, not what
        // circulates.
        let write_back = x + delayed * self.feedback;
        // Flush a vanishing tail to true zero rather than letting it decay into
        // denormals — see `DENORMAL_FLOOR`.
        self.buf[self.write] = if write_back.abs() < DENORMAL_FLOOR {
            0.0
        } else {
            write_back
        };
        // Same reasoning as above: a compare beats a division.
        self.write += 1;
        if self.write >= len {
            self.write = 0;
        }

        // Filter on the way out, so every repeat is coloured identically. Skipped
        // when transparent rather than running an identity biquad, which still
        // costs 5 multiplies and 4 adds per sample per channel.
        let wet = if self.filter_live {
            self.output_filter.process(delayed)
        } else {
            delayed
        };

        x * (1.0 - self.mix) + wet * self.mix
    }
}

/// `write - back` in a ring of `len`, without a division.
///
/// `back` is always `<= len - 1` because the tap is clamped to `len - 2`, so a
/// single conditional add is enough to bring it back into range.
#[inline]
fn wrap_back(write: usize, back: usize, len: usize) -> usize {
    if write >= back {
        write - back
    } else {
        write + len - back
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A delay whose tap is already settled, for tests about steady-state
    /// behaviour rather than the glide.
    fn settled(max: usize, delay: usize) -> Delay {
        let mut d = Delay::new(max, 48_000.0);
        d.jump_to_delay_samples(delay);
        d
    }

    #[test]
    fn transparent_by_default() {
        let mut d = Delay::new(16, 48_000.0);
        // mix 0, feedback 0: output is the input, untouched.
        for x in [0.5, -0.25, 1.0] {
            assert_eq!(d.process(x), x);
        }
    }

    /// The core behaviour: a sample comes back out `delay_samples` later.
    #[test]
    fn a_settled_tap_delays_by_exactly_its_time() {
        let mut d = settled(16, 4);
        d.set_mix(1.0); // wet only, so the output *is* the delayed signal
        let out: Vec<f32> = [1.0, 0.0, 0.0, 0.0, 0.0, 0.0]
            .iter()
            .map(|&x| d.process(x))
            .collect();
        // Impulse in at t=0 reappears at t=4.
        assert_eq!(out[4], 1.0, "{out:?}");
        assert!(out[..4].iter().all(|&v| v == 0.0), "{out:?}");
    }

    /// The whole point of the rewrite: changing the time must not snap the tap.
    /// A jump would be a discontinuity — the click this exists to remove.
    #[test]
    fn changing_the_time_glides_rather_than_jumping() {
        // `settled` jumps to an initial time, so the next set is a real change.
        let mut d = settled(48_000, 100);
        d.set_delay_samples(4_800);

        // One sample later the tap has barely moved: it is gliding, not jumping.
        d.process(0.0);
        let after_one = d.current_delay_samples();
        assert!(
            after_one > 100.0 && after_one < 200.0,
            "expected a small step, got {after_one}"
        );

        // And it does arrive: ~150 ms at 48 kHz is ~7200 samples, so well within
        // 48000 the tap should be essentially at target.
        for _ in 0..48_000 {
            d.process(0.0);
        }
        // Exactly on target, not merely close: a tempo-synced delay that settles
        // a fraction short is permanently a fraction out of time.
        assert_eq!(
            d.current_delay_samples(),
            4_800.0,
            "the tap must snap onto its target rather than asymptote"
        );
    }

    /// The property a one-pole could not give: the tap lands **exactly** on the
    /// target, and does so within the stated glide time whatever the distance.
    /// A delay that settles a fraction short is permanently a fraction out of
    /// time with the song, which is the whole point of tempo-syncing it.
    #[test]
    fn the_glide_arrives_exactly_and_on_schedule() {
        let sr = 48_000.0;
        let glide = (GLIDE_MS * 0.001 * sr) as usize; // samples in one glide

        for (from, to) in [(100usize, 4_800usize), (4_800, 100), (0, 24_000)] {
            let mut d = Delay::new(48_000, sr);
            d.jump_to_delay_samples(from);
            d.set_delay_samples(to);

            // Still travelling just before the glide completes.
            for _ in 0..glide - 2 {
                d.process(0.0);
            }
            assert_ne!(
                d.current_delay_samples(),
                to as f32,
                "{from}->{to}: should not have arrived early"
            );

            // Arrived exactly, a few samples later.
            for _ in 0..4 {
                d.process(0.0);
            }
            assert_eq!(
                d.current_delay_samples(),
                to as f32,
                "{from}->{to}: must land exactly on target"
            );
        }
    }

    /// Once settled the tap must stay put — a ramp that keeps stepping would
    /// drift the delay time away under it.
    #[test]
    fn a_settled_tap_does_not_drift() {
        let mut d = Delay::new(48_000, 48_000.0);
        d.jump_to_delay_samples(2_400);
        for _ in 0..100_000 {
            d.process(0.1);
        }
        assert_eq!(d.current_delay_samples(), 2_400.0, "settled tap drifted");
    }

    /// The first delay time must land immediately: with nothing to glide from,
    /// a glide would swoop up from zero the first time the knob is touched.
    #[test]
    fn the_first_time_set_jumps_and_later_ones_glide() {
        let mut d = Delay::new(48_000, 48_000.0);
        d.set_delay_samples(2_400);
        assert_eq!(
            d.current_delay_samples(),
            2_400.0,
            "the first set should land immediately"
        );

        d.set_delay_samples(4_800);
        d.process(0.0);
        let after = d.current_delay_samples();
        assert!(
            after > 2_400.0 && after < 4_800.0,
            "a later set should glide, got {after}"
        );
    }

    /// `jump_to_delay_samples` is the escape hatch for setting an initial time:
    /// gliding up from zero on the first note would be wrong.
    #[test]
    fn jumping_moves_the_tap_immediately() {
        let mut d = Delay::new(48_000, 48_000.0);
        d.jump_to_delay_samples(2_400);
        assert_eq!(d.current_delay_samples(), 2_400.0);
    }

    /// Fractional taps must interpolate between neighbours, not truncate — that
    /// truncation is exactly the quantisation a glide is meant to avoid.
    #[test]
    fn a_fractional_tap_interpolates_between_neighbours() {
        let mut d = Delay::new(16, 48_000.0);
        d.set_mix(1.0);
        // Land the tap exactly half a sample between 1 and 2.
        d.jump_to_delay_samples(1);
        d.target_samples = 1.5;
        d.current_samples = 1.5;

        // Write a ramp so the two neighbours differ and the midpoint is checkable.
        d.process(1.0); // buffer: [1.0, ...]
        d.process(3.0);
        let out = d.process(0.0);
        // Tap 1.5 back sits between the 3.0 and the 1.0 -> 2.0.
        assert!((out - 2.0).abs() < 1e-5, "expected ~2.0, got {out}");
    }

    #[test]
    fn feedback_and_mix_are_clamped_to_safe_ranges() {
        let mut d = settled(16, 2);
        d.set_feedback(5.0);
        d.set_mix(5.0);
        // Feedback above 1.0 would grow without bound; mix above 1.0 is meaningless.
        for _ in 0..1000 {
            let y = d.process(0.5);
            assert!(y.is_finite() && y.abs() < 100.0, "runaway: {y}");
        }
    }

    /// The tap can never index past the end of the buffer, including the `+1`
    /// interpolation neighbour — an out-of-range read here would panic on the RT
    /// thread, which is the worst place for it.
    #[test]
    fn the_tap_cannot_read_past_the_buffer() {
        let mut d = Delay::new(8, 48_000.0);
        d.jump_to_delay_samples(usize::MAX);
        assert!(d.current_delay_samples() <= 6.0, "clamped to len - 2");
        for _ in 0..100 {
            d.process(0.5); // must not panic
        }
    }

    /// The ring wraps correctly without `%`: run well past the buffer length and
    /// the delay still reads the right sample.
    #[test]
    fn the_ring_wraps_correctly_over_many_passes() {
        let mut d = settled(8, 3);
        d.set_mix(1.0);
        // Feed a repeating marker and check it comes back 3 samples later, long
        // after the write head has wrapped several times.
        let mut outs = Vec::new();
        for i in 0..40 {
            let x = if i % 8 == 0 { 1.0 } else { 0.0 };
            outs.push(d.process(x));
        }
        for i in (0..40).filter(|i| i % 8 == 0).skip(1) {
            assert_eq!(outs[i + 3], 1.0, "marker at {i} should return at {}", i + 3);
        }
    }

    /// On the output, every repeat is filtered *equally* — that is the whole point
    /// of this placement, and the observable difference from the feedback version.
    ///
    /// The feedback version darkened each repeat more than the last (the loss
    /// compounded per pass). Here the ratio between filtered and unfiltered energy
    /// should be roughly constant across repeats instead.
    #[test]
    fn the_output_filter_colours_every_repeat_equally() {
        let mut filtered = settled(48_000, 100);
        filtered.set_mix(1.0);
        filtered.set_feedback(0.9);
        filtered.set_output_filter(1_000.0, 0.707);

        let mut plain = settled(48_000, 100);
        plain.set_mix(1.0);
        plain.set_feedback(0.9);

        let energy = |d: &mut Delay| -> Vec<f32> {
            let mut windows = vec![0.0f32; 5];
            for i in 0..500 {
                let x = if i == 0 { 1.0 } else { 0.0 };
                let y = d.process(x);
                windows[i / 100] += y * y;
            }
            windows
        };
        let f = energy(&mut filtered);
        let p = energy(&mut plain);

        // Unlike the feedback version, the FIRST echo is already filtered: it
        // passes the filter on its way out, not on its way back.
        assert!(f[1] < p[1], "the first echo should be coloured too: {f:?} vs {p:?}");

        // And the ratio stays roughly constant rather than compounding: each repeat
        // is filtered once, not once per pass.
        let ratios: Vec<f32> = (1..5).map(|w| f[w] / p[w]).collect();
        let first = ratios[0];
        for (i, r) in ratios.iter().enumerate() {
            assert!(
                (r / first) > 0.5 && (r / first) < 2.0,
                "repeat {} should be filtered like the others, not compounding: {ratios:?}",
                i + 1
            );
        }
    }

    /// Transparent until asked for: an identity biquad is not free, so the
    /// untouched case must skip it entirely and stay bit-exact.
    #[test]
    fn the_output_filter_is_bit_exact_passthrough_until_set() {
        let mut plain = settled(64, 4);
        plain.set_mix(1.0);
        plain.set_feedback(0.5);

        let mut cleared = settled(64, 4);
        cleared.set_mix(1.0);
        cleared.set_feedback(0.5);
        cleared.set_output_filter(1_000.0, 0.707);
        cleared.clear_output_filter();

        for i in 0..200 {
            let x = if i == 0 { 0.7 } else { 0.0 };
            assert_eq!(plain.process(x), cleared.process(x), "diverged at {i}");
        }
    }

    /// Out of the loop, resonance cannot destabilise the delay — the hazard the
    /// feedback version needed compensation for. High Q with high feedback must
    /// stay bounded, and now does so structurally rather than by scaling.
    #[test]
    fn a_resonant_output_filter_cannot_destabilise_the_loop() {
        let mut d = settled(4_800, 480);
        d.set_mix(1.0);
        d.set_feedback(0.95);
        d.set_output_filter(800.0, 10.0); // maximum resonance
        for i in 0..48_000 {
            let x = if i % 4_800 == 0 { 0.5 } else { 0.0 };
            let y = d.process(x);
            assert!(y.is_finite() && y.abs() < 10.0, "runaway at {i}: {y}");
        }
    }

    /// Feedback must be taken *before* the filter, so what circulates is a faithful
    /// copy. If the filter had crept back into the loop, repeats would compound
    /// their colouring — checked here by the tail lasting as long as plain feedback
    /// predicts rather than dying early.
    #[test]
    fn the_filter_does_not_colour_what_circulates() {
        let mut d = settled(200, 100);
        d.set_mix(1.0);
        d.set_feedback(0.9);
        // A very dark filter: if this were in the loop it would strangle the tail
        // within a couple of passes.
        d.set_output_filter(200.0, 0.707);

        let mut last_window_energy = 0.0f32;
        for i in 0..1_000 {
            let x = if i == 0 { 1.0 } else { 0.0 };
            let y = d.process(x);
            if i >= 900 {
                last_window_energy += y * y;
            }
        }
        // After nine passes at 0.9 feedback there should still be measurable
        // signal; a filtered loop would have decayed far faster.
        assert!(
            last_window_energy > 1e-9,
            "the tail died too fast — is the filter back in the loop? {last_window_energy}"
        );
    }

    /// A decaying tail must reach true zero, not sit forever in denormal floats.
    ///
    /// This matters because the tail now keeps running while the transport is
    /// stopped (§6) with nothing to terminate it — so a quiet delay would compute
    /// with denormals indefinitely on the RT thread rather than momentarily.
    #[test]
    fn a_decaying_tail_flushes_to_true_zero() {
        let mut d = settled(64, 8);
        d.set_mix(1.0);
        d.set_feedback(0.5); // halves each pass, so it decays quickly

        // One impulse, then silence for a long time.
        d.process(1.0);
        for _ in 0..20_000 {
            d.process(0.0);
        }

        // Exactly zero, and therefore never denormal. `!= 0.0` values below the
        // f32 normal minimum would be the failure this guards against.
        for _ in 0..64 {
            let y = d.process(0.0);
            assert_eq!(y, 0.0, "the tail should have flushed to true zero");
        }
    }
}
