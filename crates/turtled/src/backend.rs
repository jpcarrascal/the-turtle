//! Hardware backend traits and portable no-op implementations.
//!
//! The RT engine talks to audio and MIDI only through these traits, so the
//! interesting logic (clock, scheduler, engine wiring) is testable on any host
//! with the `Null*` backends. The concrete ALSA implementations (spec §2/§3)
//! are Linux-only and live behind the same traits; they are **not** part of
//! this skeleton because they cannot be compiled or run here.

/// The audio device: fixed rate, fixed buffer, stereo out (§4).
pub trait AudioBackend {
    fn sample_rate(&self) -> u32;
    fn buffer_frames(&self) -> usize;
}

/// A MIDI output fan-out: `send` writes raw bytes to a logical port index (§5).
pub trait MidiSink {
    fn send(&mut self, port: usize, bytes: &[u8]);
}

/// A do-nothing audio backend for non-RT hosts and tests.
pub struct NullAudio {
    pub sample_rate: u32,
    pub buffer_frames: usize,
}

impl AudioBackend for NullAudio {
    fn sample_rate(&self) -> u32 {
        self.sample_rate
    }
    fn buffer_frames(&self) -> usize {
        self.buffer_frames
    }
}

/// A do-nothing MIDI sink.
pub struct NullMidi;

impl MidiSink for NullMidi {
    fn send(&mut self, _port: usize, _bytes: &[u8]) {}
}
