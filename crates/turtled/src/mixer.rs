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

use rtrb::{Consumer, Producer, RingBuffer};
use turtle_dsp::{one_pole_coeff, Biquad, Delay, FilterType, Gain, StereoLimiter};

use turtle_core::model::DelayDefaults;
use turtle_core::timing::DelayDivision;

use crate::control_map::DspParam;
use crate::stems::PreloadedSong;

pub type SongProducer = Producer<Mixer>;
pub type SongConsumer = Consumer<Mixer>;

/// The lock-free SPSC boundary the control thread uses to hand the audio RT
/// thread a freshly loaded [`Mixer`] (§3/§8 song switching). Separate from
/// [`crate::engine::rt_channel`]'s `RtCommand` queue so an infrequent,
/// heavier payload (a whole `Mixer`) doesn't sit alongside frequent small
/// commands — and so `RtCommand` can stay `Copy`.
pub fn song_channel(capacity: usize) -> (SongProducer, SongConsumer) {
    RingBuffer::new(capacity)
}

/// Smoothing time for the per-pair gain so mute/CC moves don't click (§6).
const GAIN_SMOOTH_MS: f32 = 5.0;
/// Smoothing time for live cutoff/resonance CC moves. Without this, sweeping
/// the filter "zippers" — the biquad's coefficients jump on every incoming CC
/// while its internal state (z1/z2) is non-zero, producing an audible click
/// per jump. Same order of magnitude as `GAIN_SMOOTH_MS`.
const FILTER_SMOOTH_MS: f32 = 5.0;
/// The division a fresh delay bus starts on, before any CC has selected one.
/// A quarter note is the least surprising default.
const DEFAULT_DIVISION: DelayDivision = DelayDivision::Quarter;
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
    /// Level fed to the shared delay bus (§6). Post-gain and post-filter, so a
    /// muted pair stops feeding the delay and its existing tail rings out —
    /// mute should mean "this pair is silent", including its echoes.
    send: Gain,
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
            // Silent until a send CC is touched, so the delay is inaudible by
            // default exactly like the rest of the chain (§6).
            send: silent_gain(sample_rate),
            sample_rate,
        }
    }

    /// One sample through gain -> biquad, returning `(dry, to_delay_bus)`.
    ///
    /// The send is taken **after** gain and filter, so what goes to the delay is
    /// the pair as you hear it — muted means muted, and a filter sweep is echoed
    /// as swept. `&mut self` because each stage advances its internal state.
    #[inline]
    fn process(&mut self, x: f32) -> (f32, f32) {
        let g = self.gain.process(x);
        if self.filter_live {
            self.cutoff_hz += (self.target_cutoff_hz - self.cutoff_hz) * self.filter_coeff;
            self.q += (self.target_q - self.q) * self.filter_coeff;
            self.recompute_biquad();
        }
        let f = self.biquad.process(g);
        (f, self.send.process(f))
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
    }
}

/// A stereo pair's two channel chains.
struct PairChain {
    left: ChannelChain,
    right: ChannelChain,
}

/// The one shared stereo delay every pair sends into (§6).
///
/// Replaces the four independent per-pair insert delays. That was the wrong shape
/// twice over: musically, four uncorrelated echoes is rarely what anyone wants
/// from a delay, and structurally it meant eight delay lines — 3 MB of
/// continuously-streaming ring buffer on a Pi 4 whose L2 cache is 1 MB. Two lines
/// fit far better, and the CPU saved pays for the feedback filter several times.
struct DelayBus {
    left: Delay,
    right: Delay,
    /// Return level: how much of the delay output reaches the master (§6). The
    /// dry signal is never attenuated by this — sends control what goes *in*.
    ///
    /// Two of them, one per channel, because `Gain::process` advances its own
    /// smoothing each call: sharing one across L and R would ramp it twice as
    /// fast as configured.
    return_l: Gain,
    return_r: Gain,
    /// Selected note division, kept so a tempo change could recompute the time.
    division: DelayDivision,
    /// Output-filter state, kept so cutoff and Q can be set independently by
    /// separate CCs without either resetting the other.
    cutoff_hz: f32,
    q: f32,
    filter_live: bool,
    bpm: f64,
    sample_rate: u32,
}

impl DelayBus {
    fn new(bpm: f64, sample_rate: u32) -> Self {
        // Sized for the longest division at *this song's* tempo, computed at load
        // rather than fixed: a whole note at 60 BPM is 4 seconds, twice the old
        // hardcoded 2 s cap, while a fast song needs far less. The mixer is built
        // per song, so this costs nothing to get exactly right.
        // `+ 2`, not `+ 1`: the tap reads `i` and `i + 1` to interpolate, so `Delay`
        // clamps it to `len - 2`. Sized one short, the longest division came out a
        // sample early — inaudible, but it means the longest note is not actually
        // reachable, and a test caught it as an exact-equality failure.
        let capacity = DelayDivision::longest().to_samples(bpm, sample_rate) as usize + 2;
        let sr = sample_rate as f32;
        let mut bus = DelayBus {
            left: Delay::new(capacity, sr),
            right: Delay::new(capacity, sr),
            return_l: silent_gain(sr),
            return_r: silent_gain(sr),
            division: DEFAULT_DIVISION,
            cutoff_hz: MAX_CUTOFF_HZ,
            q: DEFAULT_Q,
            filter_live: false,
            bpm,
            sample_rate,
        };
        bus.apply_division();
        // Fully wet, and this is load-bearing: `Delay` defaults `mix` to 0.0, which
        // returns the input *undelayed*. On a send/return bus that is not a subtle
        // mistake — the delay becomes a wire, so raising a send just doubles the dry
        // signal and every other delay control appears dead. Exactly what happened
        // on the first Pi test of this PR.
        bus.left.set_mix(1.0);
        bus.right.set_mix(1.0);
        // Immediately usable rather than all-zero (§6): the built-in defaults give
        // half feedback, unity return, and a gently darkened echo, so raising a send
        // is enough to hear a delay. Sends stay at zero, so nothing is audible until
        // one is raised — the delay is opt-in, its *settings* just are not blank.
        bus.apply_defaults(&DelayDefaults::default());
        bus
    }

    /// Push the current division's sample count into both delay lines.
    fn apply_division(&mut self) {
        let samples = self.division.to_samples(self.bpm, self.sample_rate) as usize;
        self.left.set_delay_samples(samples);
        self.right.set_delay_samples(samples);
    }

    fn set_division(&mut self, division: DelayDivision) {
        // Nothing to do if the pedal has not crossed a band boundary — and
        // re-setting the same time would restart the tape glide pointlessly.
        if division == self.division {
            return;
        }
        self.division = division;
        self.apply_division();
    }

    /// Set the division unconditionally, for applying defaults at construction
    /// where the value may equal the field's initial one and the early-out in
    /// [`DelayBus::set_division`] would skip it.
    fn force_division(&mut self, division: DelayDivision) {
        self.division = division;
        self.apply_division();
    }

    fn set_feedback(&mut self, feedback: f32) {
        self.left.set_feedback(feedback);
        self.right.set_feedback(feedback);
    }

    /// Recompute the output filter from the current cutoff and Q.
    ///
    /// # Why there is no bypass at the top of the cutoff range
    ///
    /// There used to be: at `MAX_CUTOFF_HZ` the filter was cleared, on the grounds
    /// that a 20 kHz lowpass does nothing audible and two biquads per sample are
    /// worth skipping. But CC 127 maps *exactly* onto that cutoff, so the last step
    /// of the sweep switched the filter off — and with it the resonant-gain
    /// compensation, producing a jump of `Q` in one CC step. That was reported from
    /// the Pi as "a large jump in volume, like if the filter had been suddenly
    /// turned off", which it was.
    ///
    /// Once any filter CC has been touched the filter therefore stays live across
    /// the whole range, so sweeping cutoff can never step. The bit-exact bypass
    /// still applies before anything is touched (§6's transparent default), which is
    /// the case that actually matters for a show that never uses the delay.
    fn apply_filter(&mut self) {
        self.left.set_output_filter(self.cutoff_hz, self.q);
        self.right.set_output_filter(self.cutoff_hz, self.q);
        self.filter_live = true;
    }

    /// One frame: feed the summed sends in, get the delayed return out.
    #[inline]
    fn process(&mut self, send_l: f32, send_r: f32) -> (f32, f32) {
        // Fully wet (set in `new`): this is a send/return, so the delay's output
        // *is* the wet signal and the dry path bypasses it entirely. Blending here
        // as well would attenuate the return for no reason.
        let wet_l = self.left.process(send_l);
        let wet_r = self.right.process(send_r);
        (self.return_l.process(wet_l), self.return_r.process(wet_r))
    }

    /// Apply a set of starting values (§6), through the same CC mapping a live
    /// pedal move uses — there is deliberately no second conversion path.
    fn apply_defaults(&mut self, d: &DelayDefaults) {
        self.force_division(d.time);
        self.set_feedback(d.feedback as f32 / 127.0);
        self.set_return(gain_from_cc(d.r#return));
        self.cutoff_hz = MIN_CUTOFF_HZ * (MAX_CUTOFF_HZ / MIN_CUTOFF_HZ)
            .powf(d.cutoff as f32 / 127.0);
        self.q = MIN_Q + (d.resonance as f32 / 127.0) * (MAX_Q - MIN_Q);
        self.apply_filter();
    }

    fn set_return(&mut self, level: f32) {
        self.return_l.set_target(level);
        self.return_r.set_target(level);
    }
}

/// A `Gain` that starts silent — the default for sends and the delay return, so
/// the delay is inaudible until a knob is touched (§6's transparent defaults).
fn silent_gain(sample_rate: f32) -> Gain {
    let mut g = Gain::new(sample_rate, GAIN_SMOOTH_MS);
    // `set_immediate`, not `set_target`: `Gain::new` starts at unity, so a target
    // of zero would *ramp down* from unity and leak the delay bus for the first
    // few milliseconds of every song.
    g.set_immediate(0.0);
    g
}

/// Reads preloaded stems at the transport position and mixes them down to a
/// stereo master. Owns the master sample counter (§3.1): the RT loop advances
/// it by rendering and publishes `(position, monotonic_ns)` to the clock.
pub struct Mixer {
    song: PreloadedSong,
    pairs: Vec<PairChain>,
    /// **Linked** master limiter: one gain reduction shared by both channels, so
    /// heavy limiting changes the level without shifting the stereo image. Two
    /// independent mono limiters used to reduce each channel by whatever it needed
    /// on its own, which pulls the image toward the quieter side exactly when the
    /// music is loudest.
    limiter: StereoLimiter,
    /// The one shared delay every pair sends into (§6).
    delay_bus: DelayBus,
    sample_rate: u32,
    /// Current playback position in frames (samples per channel) from the start of
    /// the **active section**.
    pos: u64,
    /// Which section is playing (§14). Always a valid index: `PreloadedSong` always
    /// has at least one section.
    active: usize,
    /// A section waiting to take over at the active section's next boundary (§4.1).
    ///
    /// The switch happens *here*, on the audio thread, rather than on the control
    /// thread reacting to a wrap event: the event is reported after the wrap, so a
    /// control-thread swap would land a period or two late and stutter the downbeat.
    queued: Option<usize>,
}

impl Mixer {
    /// Build the mixer for a preloaded song. Allocates all DSP state here, off
    /// the RT thread, so [`render`](Self::render) never allocates.
    pub fn new(song: PreloadedSong, sample_rate: u32) -> Self {
        let sr = sample_rate as f32;
        // One transparent chain per pair, sized for the WIDEST section rather than
        // the active one. Sections may use different pair counts, and switching
        // between them must not allocate — it happens on the RT thread (§14).
        let widest = song
            .sections
            .iter()
            .map(|s| s.pairs.len())
            .max()
            .unwrap_or(0);
        let pairs = (0..widest)
            .map(|_| PairChain {
                left: ChannelChain::new(sr),
                right: ChannelChain::new(sr),
            })
            .collect();
        Mixer {
            delay_bus: DelayBus::new(song.bpm, sample_rate),
            // Playback starts at the first section unless one is selected (§14).
            active: 0,
            queued: None,
            song,
            pairs,
            limiter: StereoLimiter::default_master(sr),
            sample_rate,
            pos: 0,
        }
    }

    /// Queue `index` to take over at the active section's next boundary (§4.1).
    ///
    /// Last one wins, with no special cases: queueing while something is already
    /// queued replaces it, and queueing the section already playing is how you
    /// cancel — at the boundary it does exactly what looping would have done.
    /// An index the current song does not have is ignored, so one `[control]`
    /// binding can cover every song in a show.
    pub fn queue_section(&mut self, index: usize) {
        if index < self.song.sections.len() {
            self.queued = Some(index);
        }
    }

    /// Switch to `index` immediately, from the top (§4.1).
    ///
    /// For choosing an entry point while stopped — mid-playback this would be an
    /// audible jump, which is what [`queue_section`](Self::queue_section) exists to
    /// avoid.
    pub fn select_section(&mut self, index: usize) {
        if index < self.song.sections.len() {
            self.active = index;
            self.queued = None;
            self.pos = 0;
        }
    }

    /// The name of the section waiting to take over, if any — for `turtle status`.
    pub fn queued_section_name(&self) -> Option<&str> {
        self.queued.map(|i| self.song.sections[i].name.as_str())
    }

    /// The active section's index, so the control thread can track switches.
    pub fn active_section(&self) -> usize {
        self.active
    }

    /// The section currently playing (§14).
    #[inline]
    fn section(&self) -> &crate::stems::PreloadedSection {
        &self.song.sections[self.active]
    }

    /// Every section as `(name, frames, looping)`, for the control thread to keep
    /// `turtle status` honest across a switch without reaching into the mixer.
    pub fn section_summary(&self) -> Vec<(String, u64, bool)> {
        self.song
            .sections
            .iter()
            .map(|s| (s.name.clone(), s.frames as u64, s.looping))
            .collect()
    }

    /// The active section's name, for `turtle status` and logs.
    pub fn section_name(&self) -> &str {
        &self.section().name
    }

    /// How many sections this song has. `1` for a song without `[[sections]]`.
    pub fn section_count(&self) -> usize {
        self.song.sections.len()
    }

    /// Length of the active section in frames — what the transport loops or ends at.
    pub fn section_frames(&self) -> usize {
        self.section().frames
    }

    /// Whether the loaded song repeats instead of ending (§14).
    ///
    /// Read from the song rather than stored separately on the mixer, so a song
    /// switch cannot leave a stale flag behind.
    pub fn is_looping(&self) -> bool {
        self.section().looping
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
        let v = value as f32 / 127.0;

        // Bus parameters first: there is one shared delay, so these carry no pair
        // and must not be dropped by the out-of-range check below. `pair` is
        // ignored for them by design — see `DspParam`.
        match param {
            DspParam::DelayTime => {
                self.delay_bus.set_division(DelayDivision::from_cc(value));
                return;
            }
            DspParam::DelayFeedback => {
                self.delay_bus.set_feedback(v);
                return;
            }
            DspParam::DelayReturn => {
                self.delay_bus.set_return(gain_from_cc(value));
                return;
            }
            DspParam::DelayCutoff => {
                // Same exponential taper as the per-pair filter, so both sweeps
                // feel the same under a pedal.
                self.delay_bus.cutoff_hz =
                    MIN_CUTOFF_HZ * (MAX_CUTOFF_HZ / MIN_CUTOFF_HZ).powf(v);
                self.delay_bus.apply_filter();
                return;
            }
            DspParam::DelayResonance => {
                self.delay_bus.q = MIN_Q + v * (MAX_Q - MIN_Q);
                self.delay_bus.apply_filter();
                return;
            }
            _ => {}
        }

        let Some(p) = self.pairs.get_mut(pair) else {
            return;
        };
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
            DspParam::Send => {
                // Same taper as `Gain` so a send at CC 100 is unity, matching the
                // pair fader and making "send it at the level you hear it" the
                // natural centre position.
                let level = gain_from_cc(value);
                p.left.send.set_target(level);
                p.right.send.set_target(level);
            }
            // Bus parameters are handled before the pair lookup above.
            DspParam::DelayTime
            | DspParam::DelayFeedback
            | DspParam::DelayReturn
            | DspParam::DelayCutoff
            | DspParam::DelayResonance => unreachable!("handled as bus params"),
        }
    }

    /// True once the transport has run past the end of every stem (§8 ENDED).
    pub fn is_finished(&self) -> bool {
        // A looping song never finishes, which is what stops `EndReached` firing
        // and therefore what stops the gapless auto-advance (§8) from carrying the
        // setlist forward. Stop is the only way out, by design.
        // A queued section means there IS more to play: a non-looping section with
        // something queued hands over at its end instead of ending the song, which
        // is what makes a one-shot intro work.
        !self.section().looping
            && self.queued.is_none()
            && self.pos >= self.section().frames as u64
    }

    /// Jump to `pos` (rewind / restart) and clear DSP tails.
    pub fn seek(&mut self, pos: u64) {
        self.pos = pos;
        for pair in &mut self.pairs {
            pair.left.reset();
            pair.right.reset();
        }
        // `StereoLimiter` is `Copy`, so reassigning a fresh one is the cheapest reset.
        let sr = self.sample_rate as f32;
        self.limiter = StereoLimiter::default_master(sr);
    }

    /// Render only the delay's tail: no stems, no transport advance (§6).
    ///
    /// Called instead of [`Mixer::render`] while the transport is stopped, so a
    /// delay ringing at the moment of Stop decays away naturally rather than being
    /// cut off mid-echo. The bus is fed silence and recirculates on its own
    /// feedback, so how long it lasts is whatever the feedback knob says — at high
    /// feedback, indefinitely, and turning feedback down is how you end it.
    ///
    /// The master limiter still runs, so the tail is limited consistently with the
    /// playback it came from.
    pub fn render_tail(&mut self, out: &mut [i32]) {
        let frames = out.len() / 2;
        for f in 0..frames {
            let (wet_l, wet_r) = self.delay_bus.process(0.0, 0.0);
            let (lim_l, lim_r) = self.limiter.process(wet_l, wet_r);
            out[2 * f] = to_i32(lim_l);
            out[2 * f + 1] = to_i32(lim_r);
        }
    }

    /// Apply the show's `[delay]` starting values (§6).
    ///
    /// Separate from `Mixer::new` rather than a constructor argument: the mixer is
    /// built in ~30 places (mostly tests) that have no `Show` to hand, and they
    /// should all get the same built-in defaults. This applies the *overrides*.
    pub fn apply_delay_defaults(&mut self, defaults: &DelayDefaults) {
        self.delay_bus.apply_defaults(defaults);
    }

    /// Discard the delay's contents immediately (panic, §6/§8).
    pub fn clear_delay(&mut self) {
        self.delay_bus.left.reset();
        self.delay_bus.right.reset();
    }

    /// Render one period into `out`, an interleaved `L, R, L, R, …` buffer whose
    /// length is `frames * 2`. Advances the transport by `frames`.
    /// Wraps the read position when the song loops (§14).
    ///
    /// The wrap happens **per frame**, not per buffer, which is what makes the
    /// seam inaudible: wrapping only at buffer boundaries would quantise the loop
    /// point to the period size — 21 ms at 1024 frames — and you would hear the
    /// gap. Doing it here costs a compare and a subtract per frame.
    pub fn render(&mut self, out: &mut [i32]) {
        let frames = out.len() / 2;
        for f in 0..frames {
            self.cross_section_boundary();
            let frame_idx = self.pos;
            // Sum every pair's contribution for this frame.
            let mut acc_l = 0.0f32;
            let mut acc_r = 0.0f32;
            // What this frame contributes to the shared delay bus.
            let mut send_l = 0.0f32;
            let mut send_r = 0.0f32;
            // `zip` walks the stem data and its matching DSP chain together;
            // `iter_mut` on the chains because `process` mutates their state.
            // `zip` stops at the shorter side, so a section using fewer pairs than
            // the widest simply leaves the spare chains idle.
            for (stem, chain) in self.song.sections[self.active]
                .pairs
                .iter()
                .zip(self.pairs.iter_mut())
            {
                // Past the end of a (possibly shorter) stem, read silence.
                let (l, r) = if (frame_idx as usize) < stem.frames {
                    let i = frame_idx as usize * 2;
                    (stem.samples[i], stem.samples[i + 1])
                } else {
                    (0.0, 0.0)
                };
                let (dry_l, to_delay_l) = chain.left.process(l);
                let (dry_r, to_delay_r) = chain.right.process(r);
                acc_l += dry_l;
                acc_r += dry_r;
                send_l += to_delay_l;
                send_r += to_delay_r;
            }
            // One delay for all four pairs, fed by the summed sends (§6).
            let (wet_l, wet_r) = self.delay_bus.process(send_l, send_r);
            acc_l += wet_l;
            acc_r += wet_r;
            // Master limiter, then map to the device's i32 sample format.
            let (lim_l, lim_r) = self.limiter.process(acc_l, acc_r);
            out[2 * f] = to_i32(lim_l);
            out[2 * f + 1] = to_i32(lim_r);
            // Advancing per frame — rather than adding `frames` at the end — is
            // what keeps `pos` meaningful *inside* this loop, so the boundary check
            // above sees the true position of each sample. `position()` therefore
            // also stays within the active section, which is what the clock and the
            // MIDI scheduler want: past the end they would never fire again.
            self.pos += 1;
        }
        // Once more for the final frame's advance, so `position()` is already
        // normalised when the control thread reads it between periods rather than
        // briefly reporting a position equal to the section length.
        self.cross_section_boundary();
    }

    /// Apply the section boundary if `pos` has reached the end of the active
    /// section (§4.1): hand over to a queued section, else wrap if it loops, else
    /// leave `pos` past the end for `is_finished` to report.
    ///
    /// Called per frame — not per buffer — which is what lets a switch or a wrap
    /// land on the exact sample rather than being quantised to the period. At 1024
    /// frames that is 21 ms, easily audible as a gap or a late downbeat. The cost
    /// is a compare per frame on the common path.
    #[inline]
    fn cross_section_boundary(&mut self) {
        let end = self.section().frames as u64;
        if end == 0 || self.pos < end {
            return;
        }
        if let Some(next) = self.queued.take() {
            // A queued section takes over at the boundary whether the active one
            // loops or not: one rule, so a `loop = false` section is "play once,
            // then hand over" for free.
            self.active = next;
            self.pos = 0;
        } else if self.section().looping {
            // A single subtraction covers any section at least one period long;
            // the modulo is the fallback for a section shorter than one buffer,
            // which is degenerate but should still loop.
            self.pos = if self.pos < end * 2 { self.pos - end } else { self.pos % end };
        }
        // Otherwise: past the end, not looping, nothing queued. `is_finished`
        // reports it and the stem reads fall off the end into silence.
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
    use crate::stems::{PreloadedSection, StemPair};

    fn song(pairs: Vec<StemPair>) -> PreloadedSong {
        sectioned(vec![PreloadedSection::new("t".into(), pairs, false)])
    }

    /// A song of several named sections, for the section tests.
    fn sectioned(sections: Vec<PreloadedSection>) -> PreloadedSong {
        PreloadedSong {
            name: "t".into(),
            sample_rate: 48_000,
            sections,
            bpm: 120.0,
        }
    }

    /// The same, but with its one section flagged `loop = true`.
    fn looping_song(pairs: Vec<StemPair>) -> PreloadedSong {
        sectioned(vec![PreloadedSection::new("t".into(), pairs, true)])
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

/// The shared bus, end to end: a pair's send feeds the one delay and the echo
    /// comes back at the tempo-synced time.
    ///
    /// Three things this test learned the hard way, each a real property of the
    /// system rather than a testing detail:
    ///
    ///  - The first version asserted only that nothing arrived *early*, and skipped
    ///    the sample where the bug lived — so it passed while the bus returned the
    ///    send undelayed (`mix` defaulted to 0.0, making the delay a wire).
    ///  - An impulse at sample 0 is 240x too quiet, because the send's 5 ms
    ///    smoothing ramp has not opened yet.
    ///  - Setting a division *glides* the tap over 150 ms (§6, tape-style). An
    ///    impulse fired during the glide emerges at a shifted position — correct
    ///    behaviour, but it means a test of the steady-state delay time has to let
    ///    the glide settle first.
    #[test]
    fn a_pair_send_feeds_the_shared_delay_at_the_synced_time() {
        // 120 BPM: an eighth note is exactly 12000 samples at 48 kHz.
        let division = DelayDivision::Eighth.to_samples(120.0, 48_000) as usize;
        assert_eq!(division, 12_000, "sanity: the division maths");
        // Clear of both the send ramp (240 samples) and the tap glide (7200).
        let impulse_at = 12_000usize;
        let frames = impulse_at + division * 2;

        let mut samples = vec![0.0f32; frames * 2];
        samples[impulse_at * 2] = 1.0;
        samples[impulse_at * 2 + 1] = 1.0;
        let mut m = Mixer::new(song(vec![pair(0, samples)]), 48_000);

        m.set_dsp_param(0, DspParam::Send, 100); // unity
        m.set_dsp_param(0, DspParam::DelayReturn, 100); // unity
        m.set_dsp_param(0, DspParam::DelayFeedback, 0); // one echo only
        m.set_dsp_param(0, DspParam::DelayTime, 32); // eighth-note band
        // Open the filter: the default cutoff (~2.5 kHz) would swallow most of a
        // broadband impulse, and this test is about *when* the echo lands, not what
        // the filter does to it.
        m.set_dsp_param(0, DspParam::DelayCutoff, 127);

        let mut out = vec![0i32; frames * 2];
        m.render(&mut out);
        let at = |frame: usize| out[frame * 2].abs();

        // The echo arrives at the synced sample, at a level near the input.
        let echo = at(impulse_at + division);
        assert!(
            echo > to_i32(0.5).abs(),
            "expected a strong echo at +{division}, got {echo}"
        );

        // And it is genuinely *delayed*: the gap between impulse and echo is quiet.
        // With the `mix` bug the send returned at the impulse instead, leaving this
        // region silent and the echo position empty — the inverse of this.
        let midpoint = at(impulse_at + division / 2);
        assert!(
            midpoint < echo / 100,
            "the gap before the echo should be quiet: {midpoint} vs echo {echo}"
        );
    }

    /// Changing the division must move where the echo lands — the control that
    /// appeared dead on the Pi. Checked in the settled state, so this measures the
    /// delay time rather than the glide.
    #[test]
    fn changing_the_division_moves_the_echo() {
        // Past the 150 ms tap glide, so the tap has arrived before the impulse.
        let impulse_at = 12_000usize;
        // Long enough for the *longest* division to come back: a whole note at
        // 120 BPM is 96000 samples, so a shorter render would find only the end of
        // the buffer and report a bogus position.
        let frames = impulse_at + DelayDivision::Whole.to_samples(120.0, 48_000) as usize + 2_000;

        let echo_offset = |cc: u8| -> usize {
            let mut samples = vec![0.0f32; frames * 2];
            samples[impulse_at * 2] = 1.0;
            samples[impulse_at * 2 + 1] = 1.0;
            let mut m = Mixer::new(song(vec![pair(0, samples)]), 48_000);
            m.set_dsp_param(0, DspParam::Send, 100);
            m.set_dsp_param(0, DspParam::DelayReturn, 100);
            m.set_dsp_param(0, DspParam::DelayFeedback, 0);
            m.set_dsp_param(0, DspParam::DelayTime, cc);
            // As above: measure timing, not filtering.
            m.set_dsp_param(0, DspParam::DelayCutoff, 127);
            let mut out = vec![0i32; frames * 2];
            m.render(&mut out);
            // The loudest frame after the dry impulse has passed is the echo.
            out.iter()
                .step_by(2)
                .enumerate()
                .skip(impulse_at + 100)
                .max_by_key(|(_, v)| v.abs())
                .map(|(i, _)| i - impulse_at)
                .unwrap()
        };

        // Exactly on the division, now that the glide has settled.
        assert_eq!(
            echo_offset(0),
            DelayDivision::Sixteenth.to_samples(120.0, 48_000) as usize,
            "CC 0 should be a sixteenth"
        );
        assert_eq!(
            echo_offset(127),
            DelayDivision::Whole.to_samples(120.0, 48_000) as usize,
            "CC 127 should be a whole note"
        );
    }

    /// The bus is transparent until asked for: with no send and no return, the
    /// output must be bit-identical to the dry signal (§6's transparent default).
    #[test]
    fn the_delay_bus_is_silent_until_a_send_is_raised() {
        let frames = 64;
        let mut samples = vec![0.0f32; frames * 2];
        for (i, v) in samples.iter_mut().enumerate() {
            *v = if i % 2 == 0 { 0.3 } else { -0.3 };
        }
        let mut with_bus = Mixer::new(song(vec![pair(0, samples.clone())]), 48_000);
        let mut plain = Mixer::new(song(vec![pair(0, samples)]), 48_000);
        // `plain` gets a send but no return; `with_bus` gets neither. Both must
        // equal the dry signal, since a send with nothing returning is inaudible.
        plain.set_dsp_param(0, DspParam::Send, 127);

        let mut a = vec![0i32; frames * 2];
        let mut b = vec![0i32; frames * 2];
        with_bus.render(&mut a);
        plain.render(&mut b);
        assert_eq!(a, b, "a send with no return must be inaudible");
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

    /// A looping song must repeat its samples indefinitely rather than running
    /// into the silence past its end.
    #[test]
    fn a_looping_song_repeats_its_samples() {
        // Two frames whose left channel is distinguishable: 1.0 then -1.0.
        let m_pair = pair(0, vec![0.5, 0.5, -0.5, -0.5]);
        let mut m = Mixer::new(looping_song(vec![m_pair]), 48_000);

        // Render six frames over a two-frame song: the pattern must repeat 3x.
        let mut out = vec![0i32; 6 * 2];
        m.render(&mut out);
        let left: Vec<i32> = out.iter().step_by(2).copied().collect();
        assert_eq!(left[0].signum(), 1, "{left:?}");
        assert_eq!(left[1].signum(), -1, "{left:?}");
        assert_eq!(left[2].signum(), 1, "frame 2 should be frame 0 again: {left:?}");
        assert_eq!(left[3].signum(), -1, "{left:?}");
        assert_eq!(left[4].signum(), 1, "{left:?}");
        assert_eq!(left[5].signum(), -1, "{left:?}");
    }

    /// The seam must fall **inside** a buffer, not be quantised to the buffer
    /// boundary. This is the difference between a seamless loop and a ~21 ms gap
    /// at 1024 frames, and it is the entire reason the wrap lives in the per-frame
    /// loop rather than around it.
    #[test]
    fn the_loop_seam_falls_mid_buffer_not_at_a_buffer_boundary() {
        // A 3-frame song rendered in 2-frame buffers, so the wrap lands at
        // frame 1 of the second buffer — mid-buffer by construction.
        let m_pair = pair(0, vec![0.5, 0.5, 0.5, 0.5, -0.5, -0.5]);
        let mut m = Mixer::new(looping_song(vec![m_pair]), 48_000);

        let mut buf = vec![0i32; 2 * 2];
        m.render(&mut buf); // frames 0,1  -> +,+
        assert_eq!(m.position(), 2);
        m.render(&mut buf); // frames 2,0  -> -,+   <- wrap inside this buffer
        let left: Vec<i32> = buf.iter().step_by(2).copied().collect();
        assert_eq!(left[0].signum(), -1, "frame 2 of the song: {left:?}");
        assert_eq!(
            left[1].signum(),
            1,
            "second half of this buffer must already be the loop restart: {left:?}"
        );
        assert_eq!(m.position(), 1, "position wraps with the audio");
    }

    /// A looping song must never report finished — that is what keeps
    /// `EndReached` (and therefore the setlist auto-advance) from firing.
    #[test]
    fn a_looping_song_never_finishes() {
        let mut m = Mixer::new(looping_song(vec![pair(0, vec![0.5, 0.5])]), 48_000);
        let mut out = vec![0i32; 2];
        for _ in 0..10 {
            m.render(&mut out);
            assert!(!m.is_finished(), "a looping song must not finish");
        }
    }

    /// Non-looping songs must behave exactly as before: this feature is additive.
    #[test]
    fn a_normal_song_still_ends_and_does_not_wrap() {
        let mut m = Mixer::new(song(vec![pair(0, vec![0.5, 0.5])]), 48_000);
        let mut out = vec![0i32; 2 * 2];
        m.render(&mut out);
        assert!(m.is_finished(), "a one-frame-pair song ends");
        assert_eq!(m.position(), 2, "position keeps running past the end");
    }

    /// A song shorter than one period is degenerate, but must still loop rather
    /// than read out of range or stall — the fallback path in `render`.
    #[test]
    fn a_song_shorter_than_one_buffer_still_loops() {
        // One frame, rendered five at a time: every output frame is that frame.
        let mut m = Mixer::new(looping_song(vec![pair(0, vec![0.5, 0.5])]), 48_000);
        let mut out = vec![0i32; 5 * 2];
        m.render(&mut out);
        let left: Vec<i32> = out.iter().step_by(2).copied().collect();
        assert!(left.iter().all(|v| v.signum() == 1), "{left:?}");
        assert!(m.position() < 1 || m.position() == 0, "pos wrapped: {}", m.position());
    }

    /// Report how much faster than real time the mixer renders, so the §6 DSP
    /// rearchitecture can be judged on measurements rather than flop-counting.
    ///
    /// Not an assertion about speed — a threshold here would be a flaky test on
    /// whatever machine CI happens to use. It prints, and the number is only
    /// meaningful on the target hardware:
    ///
    /// ```text
    /// cargo test --release -p turtled -- --nocapture mixer::tests::render_throughput
    /// ```
    ///
    /// Run it on the Pi before and after a DSP change. The figure to watch is the
    /// realtime factor: 20x means one core spends 5% of its time mixing.
    #[test]
    fn render_throughput_realtime_factor() {
        use std::time::Instant;

        // A full-width song: 4 stereo pairs, the worst case the engine supports.
        let rate = 48_000usize;
        let secs = 2;
        let pairs: Vec<StemPair> = (0..4u8)
            .map(|i| pair(i, [0.25, -0.25].repeat(rate * secs)))
            .collect();
        let mut m = Mixer::new(song(pairs), rate as u32);

        // Every block engaged: gain, filter, and delay all doing real work, since
        // the transparent defaults would otherwise measure the cheap path.
        for p in 0..4usize {
            m.set_dsp_param(p, DspParam::Gain, 100);
            m.set_dsp_param(p, DspParam::Cutoff, 64);
            m.set_dsp_param(p, DspParam::Resonance, 80);
            m.set_dsp_param(p, DspParam::Send, 90);
        }

        // The shared bus, fully engaged including the feedback filter — the
        // configuration this rearchitecture exists to make affordable.
        m.set_dsp_param(0, DspParam::DelayTime, 40);
        m.set_dsp_param(0, DspParam::DelayFeedback, 80);
        m.set_dsp_param(0, DspParam::DelayReturn, 64);
        m.set_dsp_param(0, DspParam::DelayCutoff, 70);
        m.set_dsp_param(0, DspParam::DelayResonance, 60);

        let period = 1024usize;
        let periods = (rate * secs) / period;
        let mut out = vec![0i32; period * 2];
        // One untimed period first, so page-faulting the buffers in does not land
        // in the measurement.
        m.render(&mut out);

        let start = Instant::now();
        for _ in 0..periods {
            m.render(&mut out);
        }
        let elapsed = start.elapsed();

        let audio_secs = (periods * period) as f64 / rate as f64;
        let factor = audio_secs / elapsed.as_secs_f64();
        eprintln!(
            "render: {:.1} s of audio in {:.1} ms = {factor:.1}x realtime \
             ({:.2}% of one core), 4 pairs @ {rate} Hz, {period}-frame periods",
            audio_secs,
            elapsed.as_secs_f64() * 1000.0,
            100.0 / factor
        );
        // The only assertion: it must at least keep up, or the design is broken
        // on this machine regardless of headroom.
        assert!(factor > 1.0, "slower than realtime: {factor:.2}x");
    }

    /// The cutoff sweep must be continuous at the very top, including the step onto
    /// CC 127 where the filter bypasses.
    ///
    /// Reported from the Pi: "the last step of the filter (126 to 127) makes a large
    /// jump in volume, like if the filter had been suddenly turned off." It had —
    /// CC 127 maps exactly onto `MAX_CUTOFF_HZ`, which clears the filter, and the
    /// `1/Q` output compensation then vanished in one step. At maximum resonance
    /// that was +20 dB.
    ///
    /// The first version of this test measured *total* output and passed even with
    /// the bug reinstated: the dry signal dominates and masks a 10x change in the
    /// wet path. It now measures a window where the dry has already stopped and only
    /// the echo is sounding, which is the only way to see the delay's own level.
    #[test]
    fn the_top_of_the_cutoff_sweep_has_no_level_jump() {
        // A burst that ENDS well before the echo returns, so the measurement window
        // contains the delay's output and nothing else.
        let echo_at = DelayDivision::Sixteenth.to_samples(120.0, 48_000) as usize; // 6000
        let burst = 2_000usize;
        let frames = echo_at + burst * 3;

        let wet_energy = |cutoff_cc: u8| -> f64 {
            let mut samples = vec![0.0f32; frames * 2];
            for i in 0..burst {
                // Broadband-ish, so the filter has something to act on.
                let t = i as f32;
                let v = 0.3 * ((t * 0.31).sin() + (t * 0.017).sin()) * 0.5;
                samples[i * 2] = v;
                samples[i * 2 + 1] = v;
            }
            let mut m = Mixer::new(song(vec![pair(0, samples)]), 48_000);
            m.set_dsp_param(0, DspParam::Send, 100);
            m.set_dsp_param(0, DspParam::DelayReturn, 100);
            m.set_dsp_param(0, DspParam::DelayFeedback, 0); // one echo, no repeats
            m.set_dsp_param(0, DspParam::DelayTime, 0); // shortest division
            m.set_dsp_param(0, DspParam::DelayResonance, 127); // maximum Q
            m.set_dsp_param(0, DspParam::DelayCutoff, cutoff_cc);

            let mut out = vec![0i32; frames * 2];
            m.render(&mut out);

            // Only the echo lives here: the burst finished at `burst`, and the echo
            // spans `echo_at ..= echo_at + burst`.
            out.iter()
                .skip(echo_at * 2)
                .take(burst * 2)
                .map(|&v| {
                    let f = v as f64 / i32::MAX as f64;
                    f * f
                })
                .sum()
        };

        let at_126 = wet_energy(126);
        let at_127 = wet_energy(127);
        assert!(at_126 > 1e-9, "the echo should be audible at CC 126: {at_126}");
        let ratio = (at_127 / at_126).sqrt(); // amplitude ratio
        assert!(
            (0.5..2.0).contains(&ratio),
            "the 126->127 step should be a small change, not a cliff: \
             amplitude ratio {ratio:.2} (energies {at_126:.9} -> {at_127:.9})"
        );
    }

    /// The thing this feature is for: raising a send alone should produce an audible
    /// delay, with no need to dial in feedback, return, cutoff and Q first.
    #[test]
    fn raising_a_send_alone_is_enough_to_hear_the_delay() {
        let echo_at = DelayDivision::Quarter.to_samples(120.0, 48_000) as usize;
        let burst = 4_000usize;
        let frames = echo_at + burst * 2;

        let mut samples = vec![0.0f32; frames * 2];
        for i in 0..burst {
            let t = i as f32;
            let v = 0.4 * ((t * 0.05).sin() + (t * 0.013).sin()) * 0.5;
            samples[i * 2] = v;
            samples[i * 2 + 1] = v;
        }
        let mut m = Mixer::new(song(vec![pair(0, samples)]), 48_000);

        // ONLY the send. Nothing else touched.
        m.set_dsp_param(0, DspParam::Send, 100);

        let mut out = vec![0i32; frames * 2];
        m.render(&mut out);

        // The burst ends at `burst`; a quarter note later there must be an echo.
        let echo: i64 = out
            .iter()
            .skip(echo_at * 2)
            .take(burst * 2)
            .map(|&v| (v as i64).abs())
            .max()
            .unwrap();
        assert!(
            echo > to_i32(0.01).abs() as i64,
            "a send alone should be audible with the default delay settings, got {echo}"
        );
    }

    /// And it must still be *silent* until that send is raised: the delay is opt-in,
    /// its settings just are not blank. A show that never touches a send never hears
    /// it, even though feedback and return are live.
    #[test]
    fn the_delay_is_silent_until_a_send_is_raised_despite_live_defaults() {
        let frames = 20_000usize;
        let mut samples = vec![0.0f32; frames * 2];
        for (i, v) in samples.iter_mut().enumerate() {
            *v = if i % 2 == 0 { 0.4 } else { -0.4 };
        }
        // Two mixers on identical audio: one untouched, one with the delay knobs
        // explicitly zeroed. If the live defaults leaked into the output, these
        // would differ.
        let mut untouched = Mixer::new(song(vec![pair(0, samples.clone())]), 48_000);
        let mut zeroed = Mixer::new(song(vec![pair(0, samples)]), 48_000);
        zeroed.set_dsp_param(0, DspParam::DelayReturn, 0);
        zeroed.set_dsp_param(0, DspParam::DelayFeedback, 0);

        let mut a = vec![0i32; frames * 2];
        let mut b = vec![0i32; frames * 2];
        untouched.render(&mut a);
        zeroed.render(&mut b);
        assert_eq!(
            a, b,
            "with no send raised, live delay defaults must be inaudible"
        );
    }

    /// The defaults are applied through the same CC mapping a pedal move uses, so a
    /// default and an explicit CC of the same value must be indistinguishable. Two
    /// conversion paths would be two places to drift.
    #[test]
    fn defaults_match_the_equivalent_cc_moves() {
        use turtle_core::model::DelayDefaults;
        let d = DelayDefaults::default();

        let frames = 30_000usize;
        let render = |explicit: bool| -> Vec<i32> {
            let mut samples = vec![0.0f32; frames * 2];
            for i in 0..4_000 {
                let t = i as f32;
                let v = 0.4 * (t * 0.05).sin();
                samples[i * 2] = v;
                samples[i * 2 + 1] = v;
            }
            let mut m = Mixer::new(song(vec![pair(0, samples)]), 48_000);
            m.set_dsp_param(0, DspParam::Send, 100);
            if explicit {
                // Re-apply the same values as if a pedal had sent them.
                m.set_dsp_param(0, DspParam::DelayFeedback, d.feedback);
                m.set_dsp_param(0, DspParam::DelayReturn, d.r#return);
                m.set_dsp_param(0, DspParam::DelayCutoff, d.cutoff);
                m.set_dsp_param(0, DspParam::DelayResonance, d.resonance);
            }
            let mut out = vec![0i32; frames * 2];
            m.render(&mut out);
            out
        };
        assert_eq!(
            render(false),
            render(true),
            "a default must be identical to the same value arriving as a CC"
        );
    }
    /// Looping is the ACTIVE section's business: a non-looping section inside a
    /// looping song must still end, and a looping one must not.
    #[test]
    fn looping_follows_the_active_section() {
        let one_shot =
            || PreloadedSection::new("one_shot".into(), vec![pair(0, vec![0.5; 8])], false);
        let vamp = || PreloadedSection::new("vamp".into(), vec![pair(0, vec![0.5; 8])], true);

        let mut m = Mixer::new(sectioned(vec![one_shot(), vamp()]), 48_000);
        assert!(!m.is_looping());
        let mut out = vec![0i32; 8];
        m.render(&mut out);
        assert!(m.is_finished(), "a non-looping section ends even mid-song");

        // The same two sections, the looping one first.
        let mut m = Mixer::new(sectioned(vec![vamp(), one_shot()]), 48_000);
        assert!(m.is_looping());
        for _ in 0..4 {
            m.render(&mut out);
            assert!(!m.is_finished(), "a looping section never finishes");
        }
    }

    /// A section note that arrives in the gap between a one-shot section running
    /// out and the transport actually stopping must still be honoured: the song is
    /// only over when nothing is waiting to play.
    #[test]
    fn queueing_after_a_section_has_run_out_rescues_the_song() {
        let intro = PreloadedSection::new("intro".into(), vec![pair(0, vec![0.5; 8])], false);
        let verse = PreloadedSection::new("verse".into(), vec![pair(0, vec![0.25; 8])], true);
        let mut m = Mixer::new(sectioned(vec![intro, verse]), 48_000);

        let mut out = vec![0i32; 4 * 2];
        m.render(&mut out);
        assert!(m.is_finished(), "the one-shot section ran out");

        // The note lands after the end but before the transport has stopped.
        m.queue_section(1);
        assert!(!m.is_finished(), "something is queued, so the song is not over");
        m.render(&mut out);
        assert_eq!(m.section_name(), "verse", "and it takes over on the next period");
    }

    // ---- section switching (§4.1) ----

    /// The whole point: a queued section takes over at the boundary, not before —
    /// and the seam lands mid-buffer, on the exact sample, not at a period edge.
    #[test]
    fn a_queued_section_takes_over_at_the_boundary_mid_buffer() {
        // Section a: 4 frames of 0.5, looping. Section b: 4 frames of 0.25.
        let a = PreloadedSection::new("a".into(), vec![pair(0, vec![0.5; 8])], true);
        let b = PreloadedSection::new("b".into(), vec![pair(0, vec![0.25; 8])], true);
        let mut m = Mixer::new(sectioned(vec![a, b]), 48_000);
        m.queue_section(1);

        // Render 6 frames: 4 of section a, then 2 of section b — the switch has to
        // land inside this buffer, which is what makes it sample-accurate.
        let mut out = vec![0i32; 6 * 2];
        m.render(&mut out);
        let left: Vec<i32> = out.iter().step_by(2).copied().collect();
        for (i, v) in left.iter().enumerate().take(4) {
            assert!((v - to_i32(0.5)).abs() < 16, "frame {i} should still be section a: {v}");
        }
        for (i, v) in left.iter().enumerate().skip(4) {
            assert!((v - to_i32(0.25)).abs() < 16, "frame {i} should be section b: {v}");
        }
        assert_eq!(m.section_name(), "b");
        assert_eq!(m.position(), 2, "position restarts inside the new section");
        assert!(m.queued_section_name().is_none(), "the queue clears after a switch");
    }

    /// Last one wins, with no special case: the most recent note is the one that
    /// happens, and re-queueing the playing section is how you cancel.
    #[test]
    fn the_last_queued_section_wins_and_requeueing_the_active_one_cancels() {
        let sections = vec![
            PreloadedSection::new("a".into(), vec![pair(0, vec![0.5; 8])], true),
            PreloadedSection::new("b".into(), vec![pair(0, vec![0.25; 8])], true),
            PreloadedSection::new("c".into(), vec![pair(0, vec![0.125; 8])], true),
        ];
        let mut m = Mixer::new(sectioned(sections), 48_000);
        m.queue_section(1);
        m.queue_section(2);
        assert_eq!(m.queued_section_name(), Some("c"), "the later note replaces the earlier");

        let mut out = vec![0i32; 8 * 2];
        m.render(&mut out);
        assert_eq!(m.section_name(), "c");

        // Now cancel a pending switch by queueing the section already playing: at
        // the boundary it does exactly what looping would have done.
        m.queue_section(0);
        m.queue_section(2);
        m.render(&mut out);
        assert_eq!(m.section_name(), "c", "re-queueing the active section is a no-op");
    }

    /// An index the song does not have is ignored, so one `[control]` binding of
    /// eight notes can serve a show whose songs have two sections and six.
    #[test]
    fn queueing_a_section_that_does_not_exist_is_ignored() {
        let mut m = Mixer::new(
            sectioned(vec![PreloadedSection::new("only".into(), vec![pair(0, vec![0.5; 8])], true)]),
            48_000,
        );
        m.queue_section(7);
        assert!(m.queued_section_name().is_none());
        let mut out = vec![0i32; 8 * 2];
        m.render(&mut out);
        assert_eq!(m.section_name(), "only");
    }

    /// A `loop = false` section with something queued hands over at its end instead
    /// of ending the song — that is what makes a one-shot intro work. With nothing
    /// queued it still ends, which is the behaviour `loop = false` promises.
    #[test]
    fn a_non_looping_section_hands_over_when_one_is_queued_and_ends_when_not() {
        let intro = || PreloadedSection::new("intro".into(), vec![pair(0, vec![0.5; 8])], false);
        let verse = || PreloadedSection::new("verse".into(), vec![pair(0, vec![0.25; 8])], true);

        let mut m = Mixer::new(sectioned(vec![intro(), verse()]), 48_000);
        m.queue_section(1);
        assert!(!m.is_finished(), "a queued section means there is more to play");
        let mut out = vec![0i32; 6 * 2];
        m.render(&mut out);
        assert_eq!(m.section_name(), "verse", "the intro handed over at its end");
        assert!(!m.is_finished());

        // Nothing queued: the same section ends the song.
        let mut m = Mixer::new(sectioned(vec![intro(), verse()]), 48_000);
        m.render(&mut out);
        assert!(m.is_finished(), "a one-shot section with nothing queued ends");
        assert_eq!(m.section_name(), "intro");
    }

    /// Selecting while stopped is a jump, not a queue: it is how you choose where
    /// Start comes in from.
    #[test]
    fn selecting_a_section_applies_immediately_and_from_the_top() {
        let a = PreloadedSection::new("a".into(), vec![pair(0, vec![0.5; 8])], true);
        let b = PreloadedSection::new("b".into(), vec![pair(0, vec![0.25; 8])], true);
        let mut m = Mixer::new(sectioned(vec![a, b]), 48_000);

        let mut out = vec![0i32; 2 * 2];
        m.render(&mut out); // advance into section a
        assert_eq!(m.position(), 2);

        m.queue_section(1);
        m.select_section(1);
        assert_eq!(m.section_name(), "b");
        assert_eq!(m.position(), 0, "a selected section starts from the top");
        assert!(m.queued_section_name().is_none(), "and supersedes anything queued");

        m.render(&mut out);
        let left: Vec<i32> = out.iter().step_by(2).copied().collect();
        assert!(left.iter().all(|v| (v - to_i32(0.25)).abs() < 16), "{left:?}");
    }

    // ---- sections (§14) ----

    /// The mixer plays the first section, not a concatenation and not the others.
    #[test]
    fn playback_reads_the_active_section() {
        let a = PreloadedSection::new("a".into(), vec![pair(0, vec![0.5; 8])], false);
        let b = PreloadedSection::new("b".into(), vec![pair(0, vec![0.25; 8])], false);
        let mut m = Mixer::new(sectioned(vec![a, b]), 48_000);
        assert_eq!(m.section_count(), 2);
        assert_eq!(m.section_name(), "a");

        let mut out = vec![0i32; 8];
        m.render(&mut out);
        // Section a's level, unmistakably not section b's.
        let expect = to_i32(0.5);
        assert!(
            out.iter().all(|&v| (v - expect).abs() < 16),
            "expected section a's 0.5, got {:?}",
            &out[..4]
        );
    }

    /// Sections have their own lengths, so end-of-song is the ACTIVE section's
    /// length — not the song's longest or first.
    #[test]
    fn end_of_playback_follows_the_active_sections_length() {
        // Section a is 2 frames; section b is much longer.
        let a = PreloadedSection::new("a".into(), vec![pair(0, vec![0.5; 4])], false);
        let b = PreloadedSection::new("b".into(), vec![pair(0, vec![0.5; 400])], false);
        let mut m = Mixer::new(sectioned(vec![a, b]), 48_000);
        let mut out = vec![0i32; 4];
        m.render(&mut out);
        assert!(m.is_finished(), "2-frame section should be done after 2 frames");
    }

    /// A switch will happen on the RT thread, so the chains must already exist for
    /// the widest section — allocating there would be a dropout.
    #[test]
    fn chains_are_allocated_for_the_widest_section() {
        let narrow = || PreloadedSection::new("narrow".into(), vec![pair(0, vec![0.5; 4])], false);
        let wide = || {
            PreloadedSection::new(
                "wide".into(),
                vec![pair(0, vec![0.5; 4]), pair(1, vec![0.5; 4]), pair(2, vec![0.5; 4])],
                false,
            )
        };
        // Widest last, then widest first: order must not matter.
        let m = Mixer::new(sectioned(vec![narrow(), wide()]), 48_000);
        assert_eq!(m.pairs.len(), 3);
        let m = Mixer::new(sectioned(vec![wide(), narrow()]), 48_000);
        assert_eq!(m.pairs.len(), 3);
    }

    /// A section using fewer pairs than the widest leaves the spare chains idle
    /// rather than reading past the end of its pair list.
    #[test]
    fn a_narrow_section_renders_without_touching_spare_chains() {
        let narrow = PreloadedSection::new("narrow".into(), vec![pair(0, vec![0.5; 8])], false);
        let wide = PreloadedSection::new(
            "wide".into(),
            vec![pair(0, vec![0.5; 8]), pair(1, vec![0.5; 8]), pair(2, vec![0.5; 8])],
            false,
        );
        let mut m = Mixer::new(sectioned(vec![narrow, wide]), 48_000);
        let mut out = vec![0i32; 8];
        m.render(&mut out);
        // One pair at 0.5, not three summed to 1.5 (which would also clip).
        let expect = to_i32(0.5);
        assert!(
            out.iter().all(|&v| (v - expect).abs() < 16),
            "spare chains contributed audio: {:?}",
            &out[..4]
        );
    }

}
