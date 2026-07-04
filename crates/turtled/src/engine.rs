//! Control-thread engine: turn transport [`Action`]s into RT commands + MIDI.
//!
//! The control thread owns the [`Transport`] state machine (`turtle-core`) and
//! this translator. Incoming foot-controller MIDI is decoded ([`control_map`]),
//! applied to the transport, and the resulting actions are dispatched:
//!
//!   * transport actions (start/stop/seek) -> [`RtCommand`]s pushed to the audio
//!     RT thread over a lock-free SPSC queue ([`rt_channel`], `rtrb`);
//!   * MIDI actions (clean release / panic) -> emitted immediately via the sink.

use rtrb::{Consumer, Producer, RingBuffer};

use turtle_core::transport::Action;
use turtle_core::{Command, Show, State, Transport};

use crate::backend::MidiSink;
use crate::control_map;
use crate::notes::ActiveNotes;

/// A command from the control thread to the audio RT thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtCommand {
    Start,
    Stop,
    Seek(u64),
}

pub type RtProducer = Producer<RtCommand>;
pub type RtConsumer = Consumer<RtCommand>;

/// The lock-free SPSC boundary between the control and audio-RT threads (§3).
pub fn rt_channel(capacity: usize) -> (RtProducer, RtConsumer) {
    RingBuffer::new(capacity)
}

pub struct Engine<M: MidiSink> {
    transport: Transport,
    control: turtle_core::model::Control,
    midi: M,
    active_notes: ActiveNotes,
    num_ports: usize,
    pending_preload: Option<String>,
}

impl<M: MidiSink> Engine<M> {
    pub fn new(show: &Show, midi: M) -> Self {
        Engine {
            transport: Transport::from_show(show),
            control: show.control.clone(),
            midi,
            active_notes: ActiveNotes::new(),
            num_ports: show.destinations.len().max(1),
            pending_preload: None,
        }
    }

    pub fn state(&self) -> State {
        self.transport.state()
    }

    /// A song the loader should preload, taken once (set by a `Preload` action).
    pub fn take_pending_preload(&mut self) -> Option<String> {
        self.pending_preload.take()
    }

    /// Record a message the scheduler dispatched, so clean-release stays correct.
    pub fn observe_output(&mut self, port: usize, bytes: &[u8]) {
        if bytes.len() >= 3 {
            self.active_notes.observe(port, bytes[0], bytes[1], bytes[2]);
        }
    }

    /// Decode + apply an incoming foot-controller message.
    pub fn handle_midi(&mut self, status: u8, d1: u8, d2: u8) -> Vec<RtCommand> {
        match control_map::decode(&self.control, status, d1, d2) {
            Some(cmd) => self.handle(cmd),
            None => Vec::new(),
        }
    }

    /// Apply a transport command, returning the RT commands to forward.
    pub fn handle(&mut self, cmd: Command) -> Vec<RtCommand> {
        let mut rt = Vec::new();
        for action in self.transport.apply(cmd) {
            match action {
                Action::Preload(song) => self.pending_preload = Some(song),
                Action::StartPlayback => rt.push(RtCommand::Start),
                Action::StopPlayback => rt.push(RtCommand::Stop),
                Action::SeekToZero => rt.push(RtCommand::Seek(0)),
                Action::ReleaseNotes => self.emit_release(),
                Action::Panic => self.emit_panic(),
            }
        }
        rt
    }

    /// Send note-offs for currently-sounding notes (clean release, §8).
    fn emit_release(&mut self) {
        for (port, bytes) in self.active_notes.release_all() {
            self.midi.send(port, &bytes);
        }
    }

    /// All-notes-off + reset-all-controllers on every port/channel (§5).
    fn emit_panic(&mut self) {
        for port in 0..self.num_ports {
            for ch in 0u8..16 {
                self.midi.send(port, &[0xB0 | ch, 123, 0]); // all notes off
                self.midi.send(port, &[0xB0 | ch, 121, 0]); // reset all controllers
            }
        }
        self.active_notes.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A MIDI sink that records everything sent, for assertions.
    #[derive(Default)]
    struct RecordingMidi {
        sent: Vec<(usize, Vec<u8>)>,
    }
    impl MidiSink for RecordingMidi {
        fn send(&mut self, port: usize, bytes: &[u8]) {
            self.sent.push((port, bytes.to_vec()));
        }
    }

    const SHOW: &str = r#"
[show]
name = "x"
playback_rate = 48000
auto_advance = false
rewind_on_stop = true
[audio]
device = "hw:0"
[[destinations]]
name = "lights"
port = "CME:1"
[[destinations]]
name = "pedals"
port = "CME:2"
[control]
input_port = "CME:in"
select_channel = 1
start = { type = "note", note = 60 }
stop  = { type = "note", note = 61 }
next  = { type = "note", note = 62 }
prev  = { type = "note", note = 63 }
panic = { type = "note", note = 65 }
mute  = { type = "note", notes = [72, 73, 74, 75] }
[[setlist]]
pc = 0
song = "01-opener"
"#;

    fn engine() -> Engine<RecordingMidi> {
        let show = Show::from_toml_str(SHOW).unwrap();
        Engine::new(&show, RecordingMidi::default())
    }

    #[test]
    fn select_then_start_forwards_rt_commands() {
        let mut e = engine();
        // Program Change 0 arms the opener.
        assert!(e.handle_midi(0xC0, 0, 0).is_empty());
        assert_eq!(e.take_pending_preload().as_deref(), Some("01-opener"));

        e.handle(Command::Loaded);
        assert_eq!(e.state(), State::Armed);

        // Note 60 = Start -> RtCommand::Start.
        assert_eq!(e.handle_midi(0x90, 60, 100), vec![RtCommand::Start]);
        assert_eq!(e.state(), State::Playing);
    }

    #[test]
    fn stop_releases_sounding_notes_then_rewinds() {
        let mut e = engine();
        e.handle(Command::Select(0));
        e.handle(Command::Loaded);
        e.handle(Command::Start);

        // The scheduler dispatched a note-on on port 0; the engine observes it.
        e.observe_output(0, &[0x90, 60, 100]);

        // Note 61 = Stop -> clean release (note-off) + Stop + Seek(0).
        let rt = e.handle_midi(0x90, 61, 100);
        assert_eq!(rt, vec![RtCommand::Stop, RtCommand::Seek(0)]);
        assert_eq!(e.midi.sent, vec![(0, vec![0x80, 60, 0])]);
    }

    #[test]
    fn double_stop_emits_panic_on_all_ports() {
        let mut e = engine();
        e.handle(Command::Select(0));
        e.handle(Command::Loaded);
        e.handle(Command::Start);
        e.handle(Command::Stop); // first stop
        e.midi.sent.clear();

        e.handle(Command::Stop); // second stop -> panic
        // 2 destinations x 16 channels x 2 messages = 64 messages.
        assert_eq!(e.midi.sent.len(), 64);
        assert!(e.midi.sent.iter().any(|(p, b)| *p == 1 && b == &[0xB0, 123, 0]));
    }

    #[test]
    fn rt_channel_round_trips() {
        let (mut tx, mut rx) = rt_channel(8);
        tx.push(RtCommand::Start).unwrap();
        tx.push(RtCommand::Seek(0)).unwrap();
        assert_eq!(rx.pop().ok(), Some(RtCommand::Start));
        assert_eq!(rx.pop().ok(), Some(RtCommand::Seek(0)));
    }
}
