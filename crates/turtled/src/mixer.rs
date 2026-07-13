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

use turtle_dsp::{one_pole_coeff, Biquad, Delay, FilterType, Gain, Limiter};

use crate::control_map::DspParam;
use crate::stems::PreloadedSong;

/// Smoothing time for the per-pair gain so mute/CC moves don't click (§6).
const GAIN_SMOOTH_MS: f32 = 5.0;
/// Smoothing time for live cutoff/resonance CC moves. Without this, sweeping
/// the filter "zippers" — the biquad's coefficients jump on every incoming CC
/// while its internal state (z1/z2) is non-zero, producing an audible click
/// per jump. Same order of magnitude as `GAIN_SMOOTH_MS`.
const FILTER_SMOOTH_MS: f32 = 5.0;
/// Headroom for the delay line: enough for any musical delay time (§6). Also
/// the ceiling a `DelayTime` CC maps to (127 -> exactly this many seconds).
const DELAY_MAX_SECONDS: usize = 2;
/// Cutoff CC range (§6): 20 Hz-20 kHz, mapped log/exponentially since that's
/// how frequency is perceived. 20 kHz doubles as the transparent default
/// (near-inaudible filtering) so an untouched pair stays a passthrough.
const MIN_CUTOFF_HZ: f32 = 20.0;
const MAX_CUTOFF_HZ: f32 = 20_000.0;
/// Resonance (Q) CC range. `DEFAULT_Q` is the flat Butterworth response (no
/// resonant peak) — "minimal Q" per §6's transparent-defaults note.
const MIN_Q: f32 = 0.5;
const MAX_Q: f32 = 10.0;
const DEFAULT_Q: f32 = 0.707;
/// Linear headroom above unity a `Gain` CC can reach (127 -> +6 dB-ish boost);
/// the master limiter is the backstop against clipping.
const MAX_GAIN: f32 = 2.0;
/// The `Gain` CC value that maps to exactly unity (1.0x). Not the fader's
/// midpoint (63.5) — like most mixing-console faders, more of the travel
/// (0..=100) is devoted to attenuation than to boost (100..=127).
const GAIN_UNITY_CC: u8 = 100;
/// Full-scale for the S32 device format: map f32 [-1.0, 1.0] onto the i32 range.
const I32_FULL_SCALE: f32 = i32::MAX as f32;

/// Map a raw `0..=127` `Gain` CC to a linear gain, piecewise around
/// [`GAIN_UNITY_CC`] so that value lands on *exactly* 1.0 rather than the
/// nearest of two off-by-half-a-step neighbors: `0..=GAIN_UNITY_CC` ramps
/// 0x -> 1x, `GAIN_UNITY_CC..=127` ramps 1x -> [`MAX_GAIN`].
fn gain_from_cc(value: u8) -> f32 {
    if value <= GAIN_UNITY_CC {
        value as f32 / GAIN_UNITY_CC as f32
    } else {
        let v = (value - GAIN_UNITY_CC) as f32 / (127 - GAIN_UNITY_CC) as f32;
        1.0 + v * (MAX_GAIN - 1.0)
    }
}

/// The fixed per-channel DSP chain (§6), in signal order. `filter_type` is
/// the pair's fixed topology (from `song.toml`, set once at load); `cutoff_hz`/`q`
/// are the *currently applied* (smoothed) biquad params, ramping each sample
/// toward `target_cutoff_hz`/`target_q` while `filter_live`. They're tracked
/// here (rather than inside `Biquad`) because `Biquad::set` needs all three
/// — type, cutoff, Q — together on every recompute, so a CC that moves just
/// one still has to resupply the other's last (smoothed) value.
struct ChannelChain {
    gain: Gain,
    biquad: Biquad,
    filter_type: FilterType,
    /// True once a Cutoff/Resonance CC has "grabbed" this pair (§6). While
    /// false, `process` never touches `biquad`'s coefficients — it stays the
    /// exact `Biquad::identity()` it was constructed with, bit-exact
    /// passthrough, no per-sample recompute cost. Once true, `cutoff_hz`/`q`
    /// ramp toward their targets and the biquad is recomputed every sample —
    /// the fix for the "zipper" click a snapped coefficient change causes.
    filter_live: bool,
    cutoff_hz: f32,
    target_cutoff_hz: f32,
    q: f32,
    target_q: f32,
    /// Per-sample smoothing coefficient for `cutoff_hz`/`q`, from `FILTER_SMOOTH_MS`.
    filter_coeff: f32,
    delay: Delay,
    sample_rate: f32,
}

impl ChannelChain {
    fn new(sample_rate: f32) -> Self {
        ChannelChain {
            gain: Gain::new(sample_rate, GAIN_SMOOTH_MS),
            // Identity = transparent until a live CC picks a cutoff/resonance.
            biquad: Biquad::identity(),
            filter_type: FilterType::Lowpass,
            filter_live: false,
            cutoff_hz: MAX_CUTOFF_HZ,
            target_cutoff_hz: MAX_CUTOFF_HZ,
            q: DEFAULT_Q,
            target_q: DEFAULT_Q,
            filter_coeff: one_pole_coeff(FILTER_SMOOTH_MS, sample_rate),
            delay: Delay::new(DELAY_MAX_SECONDS * sample_rate as usize),
            sample_rate,
        }
    }

    /// One sample through gain -> biquad -> delay. `&mut self` because each
    /// stage advances its internal state.
    #[inline]
    fn process(&mut self, x: f32) -> f32 {
        let g = self.gain.process(x);
        if self.filter_live {
            self.cutoff_hz += (self.target_cutoff_hz - self.cutoff_hz) * self.filter_coeff;
            self.q += (self.target_q - self.q) * self.filter_coeff;
            self.recompute_biquad();
        }
        let f = self.biquad.process(g);
        self.delay.process(f)
    }

    /// Recompute the biquad from the current `filter_type`/`cutoff_hz`/`q`.
    fn recompute_biquad(&mut self) {
        self.biquad
            .set(self.filter_type, self.cutoff_hz, self.q, self.sample_rate);
    }

    /// Clear filter/delay tails so a seek doesn't bleed the old position's
    /// reverberant state into the new one. Doesn't touch `filter_live`/the
    /// cutoff-resonance targets — a seek shouldn't un-grab a live knob, only
    /// clear the transient audio state (matches `Delay`, which also keeps
    /// its time/feedback/mix across a seek).
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

    /// Toggle mute on `pair` (§6/§8 per-pair mute), applied to both channels so
    /// the stereo image stays balanced. Out-of-range indices (e.g. a mute note
    /// for a pair the current song doesn't have) are a silent no-op.
    pub fn toggle_pair_mute(&mut self, pair: usize) {
        if let Some(p) = self.pairs.get_mut(pair) {
            p.left.gain.toggle_mute();
            p.right.gain.toggle_mute();
        }
    }

    /// Set `pair`'s fixed filter topology (from `song.toml`'s `[dsp.pairN]`,
    /// §6). Called once at song load, before any live CC; doesn't itself
    /// touch the biquad, which stays the transparent identity until a
    /// `Cutoff`/`Resonance` CC "grabs" it (`set_dsp_param` below) and
    /// recomputes using this topology.
    pub fn set_filter_type(&mut self, pair: usize, filter: FilterType) {
        if let Some(p) = self.pairs.get_mut(pair) {
            p.left.filter_type = filter;
            p.right.filter_type = filter;
        }
    }

    /// Apply a live DSP CC (§6) to `pair`'s chain: map the raw `0..=127`
    /// value to the parameter's engineering range and push it to both
    /// channels. Out-of-range `pair` (e.g. a CC for a pair the current song
    /// doesn't have) is a silent no-op, matching `toggle_pair_mute`.
    pub fn set_dsp_param(&mut self, pair: usize, param: DspParam, value: u8) {
        let Some(p) = self.pairs.get_mut(pair) else {
            return;
        };
        let v = value as f32 / 127.0;
        match param {
            DspParam::Gain => {
                let gain = gain_from_cc(value);
                p.left.gain.set_target(gain);
                p.right.gain.set_target(gain);
            }
            // Cutoff/resonance set a *target*; `ChannelChain::process` ramps
            // toward it and recomputes the biquad every sample (§6) so a
            // sweep glides instead of zippering.
            DspParam::Cutoff => {
                // Exponential (not linear) so the sweep feels even across the
                // audible range, matching how frequency is perceived.
                let hz = MIN_CUTOFF_HZ * (MAX_CUTOFF_HZ / MIN_CUTOFF_HZ).powf(v);
                p.left.target_cutoff_hz = hz;
                p.left.filter_live = true;
                p.right.target_cutoff_hz = hz;
                p.right.filter_live = true;
            }
            DspParam::Resonance => {
                let q = MIN_Q + v * (MAX_Q - MIN_Q);
                p.left.target_q = q;
                p.left.filter_live = true;
                p.right.target_q = q;
                p.right.filter_live = true;
            }
            DspParam::DelayTime => {
                let samples = (v * DELAY_MAX_SECONDS as f32 * self.sample_rate as f32) as usize;
                p.left.delay.set_delay_samples(samples);
                p.right.delay.set_delay_samples(samples);
            }
            // `Delay::set_feedback`/`set_mix` already clamp to their safe
            // range, so the normalized 0..=1 CC value passes straight through.
            DspParam::DelayFeedback => {
                p.left.delay.set_feedback(v);
                p.right.delay.set_feedback(v);
            }
            DspParam::DelayMix => {
                p.left.delay.set_mix(v);
                p.right.delay.set_mix(v);
            }
        }
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
    fn toggle_pair_mute_ramps_that_pair_to_silence() {
        // Two pairs, both constant 0.5, held over one second so the 5 ms
        // smoother has long since converged (mirrors turtle_dsp::gain's own
        // convergence test).
        let frames = 48_000;
        let s = song(vec![
            pair(0, [0.5, 0.5].repeat(frames)),
            pair(1, [0.5, 0.5].repeat(frames)),
        ]);
        let mut m = Mixer::new(s, 48_000);
        m.toggle_pair_mute(0);
        let mut out = vec![0i32; frames * 2];
        m.render(&mut out);
        let (last_l, last_r) = (out[out.len() - 2], out[out.len() - 1]);
        // Only pair 1 sounds once pair 0's mute has converged: L = R ~= 0.5
        // (pair 0's residual is exponentially small but not bit-exact zero,
        // same tolerance as turtle_dsp::gain's own convergence test).
        let tolerance = (to_i32(0.5) as f64 * 1e-3) as i32;
        assert!((last_l - to_i32(0.5)).abs() <= tolerance, "L = {last_l}");
        assert!((last_r - to_i32(0.5)).abs() <= tolerance, "R = {last_r}");
    }

    #[test]
    fn toggle_pair_mute_twice_restores_the_pair() {
        let s = song(vec![pair(0, vec![0.5, 0.5])]);
        let mut m = Mixer::new(s, 48_000);
        m.toggle_pair_mute(0);
        m.toggle_pair_mute(0);
        // Smoother starts already at unity, so this is instant, unlike the
        // convergence test above.
        let mut out = [0i32; 2];
        m.render(&mut out);
        assert_eq!(out[0], to_i32(0.5));
        assert_eq!(out[1], to_i32(0.5));
    }

    #[test]
    fn toggle_pair_mute_out_of_range_is_a_silent_no_op() {
        let s = song(vec![pair(0, vec![0.5, 0.5])]);
        let mut m = Mixer::new(s, 48_000);
        m.toggle_pair_mute(3); // no pair 3 in this song
        let mut out = [0i32; 2];
        m.render(&mut out);
        assert_eq!(out[0], to_i32(0.5));
        assert_eq!(out[1], to_i32(0.5));
    }

    #[test]
    fn to_i32_saturates_out_of_range() {
        assert_eq!(to_i32(2.0), i32::MAX);
        assert_eq!(to_i32(-2.0), (-1.0 * I32_FULL_SCALE) as i32);
        assert_eq!(to_i32(0.0), 0);
    }

    fn to_f32(x: i32) -> f32 {
        x as f32 / I32_FULL_SCALE
    }

    #[test]
    fn dsp_gain_scales_the_pair_after_convergence() {
        // 1s far exceeds the 5 ms smoother's settling time.
        let frames = 48_000;
        let s = song(vec![pair(0, [0.1, 0.1].repeat(frames))]);
        let mut m = Mixer::new(s, 48_000);
        let cc_value = 64u8; // below GAIN_UNITY_CC (100): the attenuation leg.
        m.set_dsp_param(0, DspParam::Gain, cc_value);
        let mut out = vec![0i32; frames * 2];
        m.render(&mut out);
        let expected = to_i32(0.1 * gain_from_cc(cc_value));
        let (last_l, last_r) = (out[out.len() - 2], out[out.len() - 1]);
        let tolerance = (expected.unsigned_abs() as f64 * 1e-2).max(2.0) as i32;
        assert!(
            (last_l - expected).abs() <= tolerance,
            "L = {last_l}, expected ~{expected}"
        );
        assert_eq!(last_l, last_r);
    }

    #[test]
    fn gain_cc_100_is_exactly_unity() {
        assert_eq!(gain_from_cc(GAIN_UNITY_CC), 1.0);
    }

    #[test]
    fn gain_cc_endpoints_are_silence_and_max_gain() {
        assert_eq!(gain_from_cc(0), 0.0);
        assert_eq!(gain_from_cc(127), MAX_GAIN);
    }

    #[test]
    fn dsp_cutoff_grabs_the_biquad_using_the_configured_topology() {
        // A highpass blocks DC: a sustained near-DC input should decay toward
        // zero once the cutoff CC "grabs" the (until-now-identity) biquad.
        let frames = 10_000;
        let s = song(vec![pair(0, [1.0, 1.0].repeat(frames))]);
        let mut m = Mixer::new(s, 48_000);
        m.set_filter_type(0, FilterType::Highpass);
        m.set_dsp_param(0, DspParam::Cutoff, 64);
        let mut out = vec![0i32; frames * 2];
        m.render(&mut out);
        let last_l = out[out.len() - 2];
        assert!(
            to_f32(last_l).abs() < 1e-2,
            "expected near-zero, got {}",
            to_f32(last_l)
        );
    }

    #[test]
    fn dsp_cutoff_ramps_gradually_rather_than_snapping_instantly() {
        // A lowpass swept from its wide-open default (~20 kHz) down to the
        // bottom of the range (20 Hz, CC 0) should pass a mid tone almost
        // unattenuated right after the CC lands, then crush it once the
        // cutoff has actually glided down — proving the cutoff ramps toward
        // its target instead of snapping there instantly (an instant snap
        // would crush the tone from sample 0).
        let sr = 48_000.0;
        let hz = 5_000.0;
        let frames = 20_000;
        let mut samples = vec![0.0f32; frames * 2];
        for i in 0..frames {
            let t = i as f32 / sr;
            let v = (2.0 * std::f32::consts::PI * hz * t).sin();
            samples[2 * i] = v;
            samples[2 * i + 1] = v;
        }
        let s = song(vec![pair(0, samples)]);
        let mut m = Mixer::new(s, 48_000);
        // Default topology is already Lowpass (no set_filter_type needed).
        m.set_dsp_param(0, DspParam::Cutoff, 0);
        let mut out = vec![0i32; frames * 2];
        m.render(&mut out);

        let peak = |range: std::ops::Range<usize>| {
            range
                .map(|i| to_f32(out[2 * i]).abs())
                .fold(0.0f32, f32::max)
        };
        // Not the very first samples: a 20 Hz lowpass has its own natural
        // step-response settling time (~8 ms / ~400 samples) even with an
        // instant coefficient snap, so comparing against sample 0 mostly
        // measures that, not the CC ramp. Sample ~1000 is past the filter's
        // own settling time but — with the ramp — the *cutoff parameter
        // itself* hasn't reached 20 Hz yet (needs ~25 ms / ~1200 samples),
        // so it should still be passing more signal than the fully-settled
        // end of the render.
        let early = peak(900..1000);
        let late = peak(frames - 100..frames); // long since settled

        // Empirically: ~546x with the ramp in place, ~67x for an instant
        // snap at the same measurement windows (the filter's own natural
        // step-response settling time alone accounts for a real but much
        // smaller gap). 100x sits cleanly between the two, so this catches
        // a regression back to an instant snap without being flaky.
        assert!(
            early > late * 100.0,
            "expected the ramp to still pass much more signal shortly \
             after the CC lands than once settled (early={early}, late={late})"
        );
    }

    #[test]
    fn dsp_delay_time_places_the_echo_at_the_cc_mapped_sample() {
        // An impulse (well below the limiter ceiling) at frame 0, silence after.
        let frames = 2_000;
        let mut samples = vec![0.0f32; frames * 2];
        samples[0] = 0.5;
        samples[1] = 0.5;
        let s = song(vec![pair(0, samples)]);
        let mut m = Mixer::new(s, 48_000);
        m.set_dsp_param(0, DspParam::DelayMix, 127); // fully wet
        m.set_dsp_param(0, DspParam::DelayFeedback, 0); // single echo, no repeats
        let cc_value = 1u8;
        m.set_dsp_param(0, DspParam::DelayTime, cc_value);
        let v = cc_value as f32 / 127.0;
        let expected_samples = (v * DELAY_MAX_SECONDS as f32 * 48_000.0) as usize;
        assert!(
            expected_samples < frames,
            "test needs to render past the tap"
        );

        let mut out = vec![0i32; frames * 2];
        m.render(&mut out);
        assert_eq!(
            out[0],
            to_i32(0.0),
            "fully wet: nothing sounds before the tap"
        );
        assert_eq!(out[expected_samples * 2], to_i32(0.5));
        assert_eq!(out[expected_samples * 2 + 1], to_i32(0.5));
    }

    #[test]
    fn set_dsp_param_out_of_range_is_a_silent_no_op() {
        let s = song(vec![pair(0, vec![0.5, 0.5])]);
        let mut m = Mixer::new(s, 48_000);
        m.set_dsp_param(3, DspParam::Gain, 127); // no pair 3 in this song
        let mut out = [0i32; 2];
        m.render(&mut out);
        assert_eq!(out[0], to_i32(0.5));
        assert_eq!(out[1], to_i32(0.5));
    }
}
