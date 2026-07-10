//! ALSA hardware backends (spec §2/§3) — **Linux only**.
//!
//! These are the concrete implementations of the [`AudioBackend`] and
//! [`MidiSink`] traits from [`crate::backend`]. They wrap the `alsa` crate,
//! which links `libasound` and therefore only exists on Linux — so the whole
//! module is compiled behind `#[cfg(target_os = "linux")]` (see `main.rs`) and
//! the `alsa` dependency is gated to Linux targets in `Cargo.toml`. On the dev
//! Mac this file is skipped entirely; it is built and run on the Pi.
//!
//! Scope of this first slice: **the hardware boundary only** — open + configure
//! the PCM device, push a period of interleaved samples, and fan MIDI bytes out
//! to rawmidi ports. The RT audio loop that fills those sample buffers (stem
//! mixing + DSP, §4/§6), the `SCHED_FIFO` thread spawning, and rawmidi *input*
//! for the control thread are follow-ups that sit on top of this layer.

// `use` brings names into scope so we can write `PCM` instead of the full
// `alsa::pcm::PCM` path each time. Grouping by module keeps it tidy.
use std::io::Write;

use alsa::pcm::{Access, Format, HwParams, PCM};
use alsa::rawmidi::Rawmidi;
use alsa::{Direction, ValueOr};

use crate::backend::{AudioBackend, MidiSink};

/// The master audio output: a stereo PCM playback device (§4).
///
/// The master bus is always stereo (2 channels) regardless of how many stem
/// channels a song uses — the RT mixer sums the pairs down to L/R before this.
pub struct AlsaAudio {
    // `PCM` owns the ALSA device handle and closes it on drop (RAII — Rust runs
    // `Drop` when the value goes out of scope, so there is no manual `close`).
    pcm: PCM,
    sample_rate: u32,
    buffer_frames: usize,
}

impl AlsaAudio {
    /// Open `device` (an ALSA name like `hw:CARD=HXStomp`) and configure it for
    /// fixed-rate, fixed-buffer stereo playback.
    ///
    /// Returns `Result<Self, alsa::Error>`: on any failure we hand the ALSA
    /// error back to the caller rather than panicking. Every `?` below is
    /// "unwrap the `Ok`, or early-return the `Err`" — the idiomatic way to
    /// bubble failures up a call chain.
    pub fn open(device: &str, sample_rate: u32, buffer_frames: usize) -> Result<Self, alsa::Error> {
        // `false` = blocking mode: `writei` will sleep until the device has room
        // for another period. Latency is irrelevant here (§3.1), so blocking on
        // big buffers is exactly what we want.
        let pcm = PCM::new(device, Direction::Playback, false)?;

        // Hardware parameters are configured through a temporary `HwParams`
        // handle. The inner scope `{ ... }` drops `hwp` before we return, so it
        // can't be used after we've committed it to the device.
        {
            let hwp = HwParams::any(&pcm)?;
            hwp.set_channels(2)?; // stereo master out
            hwp.set_rate(sample_rate, ValueOr::Nearest)?; // 48 kHz on the HX Stomp
            // S32LE = 32-bit signed little-endian samples. 24-bit stem audio is
            // carried left-justified in the top 24 bits; a 24-bit DAC ignores
            // the low byte. S32 is the safe common denominator for USB/I2S DACs.
            hwp.set_format(Format::s32())?;
            // Interleaved: L,R,L,R,... in one buffer (vs. one buffer per channel).
            hwp.set_access(Access::RWInterleaved)?;
            // Sizes are in *frames* (one frame = one sample per channel). A large
            // buffer split into a few periods is xrun-proof (§3.1).
            hwp.set_buffer_size(buffer_frames as i64)?;
            hwp.set_period_size((buffer_frames / 4).max(1) as i64, ValueOr::Nearest)?;
            // Commit. This transitions the PCM to the "prepared" state; the first
            // `writei` then starts the stream.
            pcm.hw_params(&hwp)?;
        }

        Ok(AlsaAudio { pcm, sample_rate, buffer_frames })
    }

    /// Write one period of interleaved stereo samples to the device.
    ///
    /// `interleaved.len()` must be `frames * 2`. The RT loop calls this once per
    /// period after mixing. On an xrun (buffer underrun — the RT thread was late)
    /// or a device suspend, `try_recover` resets the stream and we retry once.
    pub fn write_period(&self, interleaved: &[i32]) -> Result<(), alsa::Error> {
        // `io_i32()` is a thin, alloc-free view over the PCM as `i32` samples.
        // We build it per call instead of storing it, because it borrows `pcm`
        // (`IO<'_>` holds a `&PCM`): keeping both the `PCM` and a borrow of it in
        // one struct would be self-referential, which Rust's borrow checker
        // forbids. Re-deriving the cheap view each call sidesteps that entirely.
        let io = self.pcm.io_i32()?;
        match io.writei(interleaved) {
            Ok(_frames_written) => Ok(()),
            Err(err) => {
                // `silent = true`: don't let ALSA print to stderr from the RT path.
                self.pcm.try_recover(err, true)?;
                io.writei(interleaved)?;
                Ok(())
            }
        }
    }
}

/// The trait impl is what lets the generic engine treat `AlsaAudio` and the
/// portable `NullAudio` interchangeably (`impl AudioBackend for ...`).
impl AudioBackend for AlsaAudio {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn buffer_frames(&self) -> usize {
        self.buffer_frames
    }
}

/// MIDI output fan-out over ALSA rawmidi (§5): one open port per destination.
pub struct AlsaMidi {
    // Indexed by the logical port number the scheduler/engine use. `Vec` owns
    // the handles; each `Rawmidi` closes on drop.
    ports: Vec<Rawmidi>,
}

impl AlsaMidi {
    /// Open one rawmidi playback handle per name, in order.
    ///
    /// NOTE: `names` must be **resolved ALSA rawmidi device names** (e.g.
    /// `hw:1,0,0`), not the logical labels from `show.toml` (`"CME:1"`).
    /// Translating a label to the CME card's `hw:` address is a bring-up step
    /// that belongs above this layer — a follow-up (spec §5 name resolution).
    pub fn open(names: &[String]) -> Result<Self, alsa::Error> {
        // Pre-size the Vec so pushing the handles does not reallocate.
        let mut ports = Vec::with_capacity(names.len());
        for name in names {
            // `&str` from `&String` via deref coercion; `false` = blocking write.
            ports.push(Rawmidi::new(name, Direction::Playback, false)?);
        }
        Ok(AlsaMidi { ports })
    }
}

impl MidiSink for AlsaMidi {
    fn send(&mut self, port: usize, bytes: &[u8]) {
        // The trait signature returns `()`, and this runs on the MIDI scheduler
        // thread, so we can't propagate an error with `?`. `.get()` returns an
        // `Option`, so an out-of-range port is a no-op rather than a panic.
        if let Some(rmidi) = self.ports.get(port) {
            // `io()` gives a `std::io::Write` over the port (same per-call,
            // borrow-avoiding pattern as the PCM view above). rawmidi writes are
            // syscalls — allowed on the MIDI thread, unlike the audio RT thread.
            let mut io = rmidi.io();
            // TODO: route write failures to the preallocated RT log ring (§13);
            // for now a failed MIDI byte is dropped rather than crashing a show.
            let _ = io.write_all(bytes);
        }
    }
}
