//! The RT mixer (spec §3/§4/§6): turn preloaded stems into one period of
//! interleaved stereo output for `AlsaAudio::write_period`.
//!
//! Signal flow per the architecture table (§3):
//!
//! ```text
//! read stems -> per-pair gain/mute -> biquad -> delay -> sum -> master limiter -> device
//! ```
//!
//! Everything here is **alloc-free** on the hot path: all buffers (delay lines,
//! per-channel DSP state) are allocated in [`Mixer::new`], off the RT thread;
//! [`Mixer::render`] only reads, does arithmetic, and writes. It is
//! host-independent, so it is unit-tested on the dev Mac like the stem loader.
//!
//! Stereo detail: each DSP primitive carries its own internal state (filter
//! delays, the delay ring, the smoother), so the left and right channels each
//! need their *own* instance — sharing one across both channels would bleed L
//! state into R. Hence a [`ChannelChain`] per channel and a [`PairChain`] per
//! pair. The chain's default parameters are transparent (§6), so until live CC
//! drives them the mixer is a straight passthrough of the summed stems.

use turtle_dsp::{Biquad, Delay, Gain, Limiter};

use crate::stems::PreloadedSong;

/// Smoothing time for the per-pair gain so mute/CC moves don't click (§6).
const GAIN_SMOOTH_MS: f32 = 5.0;
/// Headroom for the delay line: enough for any musical delay time (§6).
const DELAY_MAX_SECONDS: usize = 2;
/// Full-scale for the S32 device format: map f32 [-1.0, 1.0] onto the i32 range.
const I32_FULL_SCALE: f32 = i32::MAX as f32;

/// The fixed per-channel DSP chain (§6), in signal order.
struct ChannelChain {
    gain: Gain,
    biquad: Biquad,
    delay: Delay,
}

impl ChannelChain {
    fn new(sample_rate: f32) -> Self {
        ChannelChain {
            gain: Gain::new(sample_rate, GAIN_SMOOTH_MS),
            // Identity = transparent until a live CC picks a filter type/cutoff.
            biquad: Biquad::identity(),
            delay: Delay::new(DELAY_MAX_SECONDS * sample_rate as usize),
        }
    }

    /// One sample through gain -> biquad -> delay. `&mut self` because each
    /// stage advances its internal state.
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let g = self.gain.process(x);
        let f = self.biquad.process(g);
        self.delay.process(f)
    }

    /// Clear filter/delay tails so a seek doesn't bleed the old position's
    /// reverberant state into the new one.
    fn reset(&mut self) {
        self.biquad.reset();
        self.delay.reset();
    }
}

/// A stereo pair's two channel chains.
struct PairChain {
    left: ChannelChain,
    right: ChannelChain,
}

/// Reads preloaded stems at the transport position and mixes them down to a
/// stereo master. Owns the master sample counter (§3.1): the RT loop advances
/// it by rendering and publishes `(position, monotonic_ns)` to the clock.
pub struct Mixer {
    song: PreloadedSong,
    pairs: Vec<PairChain>,
    // The master limiter is per-channel here. NOTE: this makes the two channels
    // limit independently (unlinked) — under heavy limiting the stereo image can
    // shift. A linked stereo limiter (shared gain reduction) is a later refinement;
    // `turtle-dsp::Limiter` is mono today.
    limiter_l: Limiter,
    limiter_r: Limiter,
    sample_rate: u32,
    /// Current playback position in frames (samples per channel) from song start.
    pos: u64,
}

impl Mixer {
    /// Build the mixer for a preloaded song. Allocates all DSP state here, off
    /// the RT thread, so [`render`](Self::render) never allocates.
    pub fn new(song: PreloadedSong, sample_rate: u32) -> Self {
        let sr = sample_rate as f32;
        // One transparent chain per pair. `map` + `collect` builds the `Vec` in
        // one shot; `_` ignores each pair's data (we only need the count/layout).
        let pairs = song
            .pairs
            .iter()
            .map(|_| PairChain {
                left: ChannelChain::new(sr),
                right: ChannelChain::new(sr),
            })
            .collect();
        Mixer {
            song,
            pairs,
            limiter_l: Limiter::default_master(sr),
            limiter_r: Limiter::default_master(sr),
            sample_rate,
            pos: 0,
        }
    }

    pub fn position(&self) -> u64 {
        self.pos
    }

    /// True once the transport has run past the end of every stem (§8 ENDED).
    pub fn is_finished(&self) -> bool {
        self.pos >= self.song.frames as u64
    }

    /// Jump to `pos` (rewind / restart) and clear DSP tails.
    pub fn seek(&mut self, pos: u64) {
        self.pos = pos;
        for pair in &mut self.pairs {
            pair.left.reset();
            pair.right.reset();
        }
        // `Limiter` is `Copy`, so reassigning a fresh one is the cheapest reset.
        let sr = self.sample_rate as f32;
        self.limiter_l = Limiter::default_master(sr);
        self.limiter_r = Limiter::default_master(sr);
    }

    /// Render one period into `out`, an interleaved `L, R, L, R, …` buffer whose
    /// length is `frames * 2`. Advances the transport by `frames`.
    pub fn render(&mut self, out: &mut [i32]) {
        let frames = out.len() / 2;
        for f in 0..frames {
            let frame_idx = self.pos + f as u64;
            // Sum every pair's contribution for this frame.
            let mut acc_l = 0.0f32;
            let mut acc_r = 0.0f32;
            // `zip` walks the stem data and its matching DSP chain together;
            // `iter_mut` on the chains because `process` mutates their state.
            for (stem, chain) in self.song.pairs.iter().zip(self.pairs.iter_mut()) {
                // Past the end of a (possibly shorter) stem, read silence.
                let (l, r) = if (frame_idx as usize) < stem.frames {
                    let i = frame_idx as usize * 2;
                    (stem.samples[i], stem.samples[i + 1])
                } else {
                    (0.0, 0.0)
                };
                acc_l += chain.left.process(l);
                acc_r += chain.right.process(r);
            }
            // Master limiter, then map to the device's i32 sample format.
            out[2 * f] = to_i32(self.limiter_l.process(acc_l));
            out[2 * f + 1] = to_i32(self.limiter_r.process(acc_r));
        }
        self.pos += frames as u64;
    }
}

/// Convert a float sample in ~[-1.0, 1.0] to a full-scale `i32`. Rust's
/// float-to-int cast *saturates* (values beyond i32's range clamp to
/// MIN/MAX rather than wrapping or being UB), so the explicit `clamp` is really
/// just to define the ceiling precisely at ±1.0.
#[inline]
fn to_i32(x: f32) -> i32 {
    (x.clamp(-1.0, 1.0) * I32_FULL_SCALE) as i32
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::stems::StemPair;

    fn song(pairs: Vec<StemPair>) -> PreloadedSong {
        let frames = pairs.iter().map(|p| p.frames).max().unwrap_or(0);
        PreloadedSong { name: "t".into(), sample_rate: 48_000, frames, pairs }
    }

    fn pair(index: u8, samples: Vec<f32>) -> StemPair {
        let frames = samples.len() / 2;
        StemPair { index, samples, frames }
    }

    #[test]
    fn transparent_chain_passes_stems_through() {
        // One pair, two frames; all samples below the 0.98 limiter ceiling, so
        // the default (transparent) chain is an exact passthrough.
        let s = song(vec![pair(0, vec![0.5, -0.25, 0.1, 0.2])]);
        let mut m = Mixer::new(s, 48_000);
        let mut out = [0i32; 4];
        m.render(&mut out);
        assert_eq!(out[0], to_i32(0.5));
        assert_eq!(out[1], to_i32(-0.25));
        assert_eq!(out[2], to_i32(0.1));
        assert_eq!(out[3], to_i32(0.2));
        assert_eq!(m.position(), 2);
    }

    #[test]
    fn sums_pairs() {
        // Frame 0: L = 0.3 + 0.4 = 0.7, R = -0.1 + 0.2 = 0.1 (both < ceiling).
        let s = song(vec![pair(0, vec![0.3, -0.1]), pair(1, vec![0.4, 0.2])]);
        let mut m = Mixer::new(s, 48_000);
        let mut out = [0i32; 2];
        m.render(&mut out);
        // Compare against the *same* accumulation order the mixer uses: f32
        // addition isn't associative, so `0.3 + 0.4` is not bit-identical to the
        // literal `0.7`. Reproducing the sum keeps the check exact.
        assert_eq!(out[0], to_i32(0.3 + 0.4));
        assert_eq!(out[1], to_i32(-0.1 + 0.2));
    }

    #[test]
    fn zero_pads_past_end_of_song() {
        // A one-frame song rendered over three frames: frames 1 and 2 are silence.
        let s = song(vec![pair(0, vec![0.5, 0.5])]);
        let mut m = Mixer::new(s, 48_000);
        let mut out = [123i32; 6];
        m.render(&mut out);
        assert_eq!(out[0], to_i32(0.5));
        assert_eq!(out[1], to_i32(0.5));
        assert_eq!(&out[2..], &[0, 0, 0, 0]);
        assert!(m.is_finished());
    }

    #[test]
    fn seek_repositions_and_clears() {
        let s = song(vec![pair(0, vec![0.5, 0.5, 0.5, 0.5])]);
        let mut m = Mixer::new(s, 48_000);
        let mut out = [0i32; 4];
        m.render(&mut out);
        assert_eq!(m.position(), 2);
        m.seek(0);
        assert_eq!(m.position(), 0);
        assert!(!m.is_finished());
    }

    #[test]
    fn master_never_exceeds_ceiling() {
        // Two loud pairs sum well past full scale; the limiter must pin |out|
        // to the 0.98 ceiling rather than clipping/wrapping.
        let s = song(vec![pair(0, vec![0.9, -0.9]), pair(1, vec![0.9, -0.9])]);
        let mut m = Mixer::new(s, 48_000);
        let mut out = [0i32; 2];
        m.render(&mut out);
        let ceiling = to_i32(0.98);
        assert!(out[0].abs() <= ceiling, "L overshoot: {}", out[0]);
        assert!(out[1].abs() <= ceiling, "R overshoot: {}", out[1]);
    }

    #[test]
    fn to_i32_saturates_out_of_range() {
        assert_eq!(to_i32(2.0), i32::MAX);
        assert_eq!(to_i32(-2.0), (-1.0 * I32_FULL_SCALE) as i32);
        assert_eq!(to_i32(0.0), 0);
    }
}
