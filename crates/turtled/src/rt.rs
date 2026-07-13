//! The audio RT loop (spec §3/§3.1): drive the mixer, feed ALSA, publish the clock.
//!
//! This module is split deliberately so most of it stays testable on the dev Mac:
//!
//!   * [`RtAudio`] — the **portable** per-period control logic (play/stop/seek
//!     state, what to render, which sample position to publish). Unit-tested.
//!   * [`run_audio`] — the **`cfg(linux)`** blocking loop that actually talks to
//!     `AlsaAudio` and times the clock. It only *sequences* the tested pieces, so
//!     the untestable surface is thin. Compiled/verified on the Pi.

use crate::engine::RtCommand;
use crate::mixer::Mixer;

/// Per-period control state for the audio thread. Holds only the transport
/// play/pause flag; the sample position lives in the [`Mixer`] (§3.1).
pub struct RtAudio {
    playing: bool,
    /// Whether `EndReached` has already been reported for the current run
    /// (since `Start`/`Seek`), so a finished-but-not-yet-stopped mixer
    /// doesn't spam the event queue every period while the control thread's
    /// resulting `Stop`/`Seek` is still in flight (§8).
    reported_end: bool,
}

impl Default for RtAudio {
    fn default() -> Self {
        RtAudio {
            playing: false,
            reported_end: false,
        }
    }
}

impl RtAudio {
    pub fn new() -> Self {
        Self::default()
    }

    /// Apply one command from the control thread (drained from the SPSC queue).
    /// `Seek` repositions the transport whether or not we're playing, matching
    /// the engine's Stop→Seek(0) rewind (see `engine.rs`).
    pub fn apply(&mut self, mixer: &mut Mixer, cmd: RtCommand) {
        match cmd {
            RtCommand::Start => {
                self.playing = true;
                self.reported_end = false;
            }
            RtCommand::Stop => self.playing = false,
            RtCommand::Seek(pos) => {
                mixer.seek(pos);
                self.reported_end = false;
            }
            RtCommand::ToggleMute(pair) => mixer.toggle_pair_mute(pair),
            RtCommand::SetDsp(pair, param, value) => mixer.set_dsp_param(pair, param, value),
        }
    }

    /// True exactly once per run when the mixer has played past the end of
    /// the song while playing (§8 `EndReached`) — call after [`step`](Self::step).
    /// Silent (returns `false`) once already reported, until the next
    /// `Start`/`Seek` resets the flag.
    pub fn check_end(&mut self, mixer: &Mixer) -> bool {
        if self.playing && mixer.is_finished() && !self.reported_end {
            self.reported_end = true;
            true
        } else {
            false
        }
    }

    /// Fill one period into `out` and return the sample position to publish as
    /// the clock anchor. We snapshot the position *before* rendering, because
    /// that is the transport time of the first sample in this buffer — which is
    /// what the MIDI scheduler interpolates against (§3.1).
    ///
    /// When stopped we output silence and do **not** advance the transport, but
    /// still return the (frozen) position so the scheduler sees a stable anchor.
    pub fn step(&mut self, mixer: &mut Mixer, out: &mut [i32]) -> u64 {
        let pos = mixer.position();
        if self.playing {
            mixer.render(out);
        } else {
            // `fill` writes the whole slice in one call — silence, no allocation.
            out.fill(0);
        }
        pos
    }

    pub fn is_playing(&self) -> bool {
        self.playing
    }
}

/// The blocking audio RT loop. Linux-only because it drives `AlsaAudio`.
///
/// Each iteration: swap in a freshly loaded song if the control thread sent
/// one, drain pending commands, render/silence one period, publish the clock
/// anchor, report `EndReached` if the song just finished, then hand the
/// period to ALSA. `write_period` blocks until the device has room, which is
/// what paces the whole loop (no manual sleep).
///
/// `epoch` is a shared monotonic reference: this thread and the MIDI scheduler
/// both measure `epoch.elapsed()` so their nanosecond timestamps are comparable.
/// `running` lets the control thread ask the loop to exit.
#[cfg(target_os = "linux")]
pub fn run_audio(
    audio: &crate::alsa_backend::AlsaAudio,
    mixer: &mut Mixer,
    clock: &crate::clock::TransportClock,
    rx: &mut crate::engine::RtConsumer,
    song_rx: &mut crate::mixer::SongConsumer,
    events: &mut crate::engine::RtEventProducer,
    epoch: std::time::Instant,
    running: &std::sync::atomic::AtomicBool,
) {
    use std::sync::atomic::Ordering;
    // `buffer_frames()` is a method on the `AudioBackend` trait, so the trait
    // must be in scope here to call it on the concrete `AlsaAudio`.
    use crate::backend::AudioBackend;

    // One allocation, before the loop: the interleaved stereo period buffer.
    // Nothing inside the loop allocates, per the RT-thread contract (§3).
    let frames = audio.buffer_frames();
    let mut buf = vec![0i32; frames * 2];

    let mut rt = RtAudio::new();

    while running.load(Ordering::Acquire) {
        // A song swap (if any) lands before commands, so a Seek/Start queued
        // alongside it (e.g. gapless auto-advance) applies to the new song,
        // not the one it's replacing. At most one is ever queued in practice.
        while let Ok(new_mixer) = song_rx.pop() {
            *mixer = new_mixer;
        }

        // Drain every queued command. `pop()` is `Err` when the queue is empty,
        // so this `while let` stops as soon as we've caught up.
        while let Ok(cmd) = rx.pop() {
            rt.apply(mixer, cmd);
        }

        let now_ns = epoch.elapsed().as_nanos() as u64;
        let pos = rt.step(mixer, &mut buf);
        clock.publish(pos, now_ns);

        if rt.check_end(mixer) {
            let _ = events.push(crate::engine::RtEvent::EndReached);
        }

        // `write_period` already recovers from xruns internally; a hard error
        // here means the device is gone, so we stop the loop.
        // TODO: report the error via the RT log ring (§13) rather than silently.
        if audio.write_period(&buf).is_err() {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mixer::Mixer;
    use crate::stems::{PreloadedSong, StemPair};

    // A mixer over one pair of constant non-silent samples, so "did we render?"
    // is easy to see in the output.
    fn mixer() -> Mixer {
        let pair = StemPair { index: 0, samples: vec![0.5, 0.5, 0.5, 0.5], frames: 2 };
        let song = PreloadedSong { name: "t".into(), sample_rate: 48_000, frames: 2, pairs: vec![pair] };
        Mixer::new(song, 48_000)
    }

    #[test]
    fn stopped_outputs_silence_and_does_not_advance() {
        let mut m = mixer();
        let mut rt = RtAudio::new();
        let mut out = [7i32; 4];
        let pos = rt.step(&mut m, &mut out);
        assert_eq!(pos, 0);
        assert_eq!(out, [0, 0, 0, 0]);
        assert_eq!(m.position(), 0, "stopped transport must not advance");
    }

    #[test]
    fn start_plays_and_advances() {
        let mut m = mixer();
        let mut rt = RtAudio::new();
        rt.apply(&mut m, RtCommand::Start);

        let mut out = [0i32; 2]; // one frame
        let pos = rt.step(&mut m, &mut out);
        assert_eq!(pos, 0, "anchor is the position at the start of the period");
        assert!(out[0] != 0, "playing output should be non-silent");
        assert_eq!(m.position(), 1, "playing advances the transport by the period");
    }

    #[test]
    fn stop_freezes_transport() {
        let mut m = mixer();
        let mut rt = RtAudio::new();
        rt.apply(&mut m, RtCommand::Start);
        let mut out = [0i32; 2];
        rt.step(&mut m, &mut out); // advance to 1
        rt.apply(&mut m, RtCommand::Stop);
        let pos = rt.step(&mut m, &mut out);
        assert_eq!(pos, 1);
        assert_eq!(out, [0, 0]);
        assert_eq!(m.position(), 1, "no advance while stopped");
    }

    #[test]
    fn check_end_fires_once_when_the_song_finishes_while_playing() {
        let mut m = mixer(); // 2 frames
        let mut rt = RtAudio::new();
        rt.apply(&mut m, RtCommand::Start);
        let mut out = [0i32; 2];
        rt.step(&mut m, &mut out); // pos 0 -> 1
        assert!(!rt.check_end(&m), "not finished yet");
        rt.step(&mut m, &mut out); // pos 1 -> 2: now finished
        assert!(rt.check_end(&m), "should report exactly once when finished");
        assert!(!rt.check_end(&m), "must not report again until Start/Seek");
    }

    #[test]
    fn check_end_is_silent_while_stopped() {
        let mut m = mixer();
        let mut rt = RtAudio::new();
        rt.apply(&mut m, RtCommand::Start);
        let mut out = [0i32; 2];
        rt.step(&mut m, &mut out);
        rt.step(&mut m, &mut out); // now finished, but not yet checked
        rt.apply(&mut m, RtCommand::Stop);
        assert!(
            !rt.check_end(&m),
            "finished-but-stopped shouldn't report EndReached"
        );
    }

    #[test]
    fn check_end_resets_on_seek_and_start() {
        let mut m = mixer();
        let mut rt = RtAudio::new();
        rt.apply(&mut m, RtCommand::Start);
        let mut out = [0i32; 2];
        rt.step(&mut m, &mut out);
        rt.step(&mut m, &mut out);
        assert!(rt.check_end(&m));

        // Seek back to the start and re-run to the end: should report again.
        rt.apply(&mut m, RtCommand::Seek(0));
        rt.step(&mut m, &mut out);
        rt.step(&mut m, &mut out);
        assert!(
            rt.check_end(&m),
            "seeking back should allow EndReached to fire again"
        );
    }

    #[test]
    fn toggle_mute_forwards_to_the_mixer() {
        let mut m = mixer();
        let mut rt = RtAudio::new();
        rt.apply(&mut m, RtCommand::Start);
        rt.apply(&mut m, RtCommand::ToggleMute(0));
        let mut out = [0i32; 2];
        rt.step(&mut m, &mut out);
        // Mute starts ramping toward silence immediately (0 ms elapsed isn't
        // enough to reach it exactly, but the sample must already be reduced
        // below the unmuted 0.5-gain full-scale value).
        let unmuted_half = (i32::MAX as f32 * 0.5) as i32;
        assert!(out[0] < unmuted_half, "expected {} < {}", out[0], unmuted_half);
    }

    #[test]
    fn set_dsp_forwards_to_the_mixer() {
        let mut m = mixer();
        let mut rt = RtAudio::new();
        rt.apply(&mut m, RtCommand::Start);
        rt.apply(
            &mut m,
            RtCommand::SetDsp(0, crate::control_map::DspParam::Gain, 0),
        );
        let mut out = [0i32; 2];
        rt.step(&mut m, &mut out);
        // Gain starts ramping toward 0 immediately, same shape as mute.
        let unmuted_half = (i32::MAX as f32 * 0.5) as i32;
        assert!(
            out[0] < unmuted_half,
            "expected {} < {}",
            out[0],
            unmuted_half
        );
    }

    #[test]
    fn seek_repositions_regardless_of_play_state() {
        let mut m = mixer();
        let mut rt = RtAudio::new();
        rt.apply(&mut m, RtCommand::Start);
        let mut out = [0i32; 2];
        rt.step(&mut m, &mut out); // pos -> 1
        rt.apply(&mut m, RtCommand::Seek(0));
        assert_eq!(m.position(), 0);
        assert!(rt.is_playing(), "seek must not change play state");
    }
}
