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
use crate::control_map::{self, DspParam};
use crate::notes::ActiveNotes;

/// A command from the control thread to the audio RT thread.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtCommand {
    Start,
    Stop,
    Seek(u64),
    /// Toggle the mute on pair `index` (§6/§8), independent of transport state.
    ToggleMute(usize),
    /// Set a live DSP param on pair `index` to the raw `0..=127` CC value
    /// (§6), independent of transport state.
    SetDsp(usize, DspParam, u8),
}

pub type RtProducer = Producer<RtCommand>;
pub type RtConsumer = Consumer<RtCommand>;

/// The lock-free SPSC boundary between the control and audio-RT threads (§3).
pub fn rt_channel(capacity: usize) -> (RtProducer, RtConsumer) {
    RingBuffer::new(capacity)
}

/// An event from the audio RT thread back to the control thread — the
/// mirror-image boundary of [`RtCommand`] (§3/§8).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RtEvent {
    /// The mixer just rendered past the end of the current song.
    EndReached,
}

pub type RtEventProducer = Producer<RtEvent>;
pub type RtEventConsumer = Consumer<RtEvent>;

/// The lock-free SPSC boundary for audio-RT -> control events (§3).
pub fn rt_event_channel(capacity: usize) -> (RtEventProducer, RtEventConsumer) {
    RingBuffer::new(capacity)
}

/// What an incoming MIDI message decoded to.
///
/// Recorded by [`Engine::handle_midi`] and read back with
/// [`Engine::take_last_decoded`] so a reporter (`turtle monitor`, `--verbose`)
/// can say what a message *meant* without re-running the decode — two decode
/// paths would be free to drift apart, and the mute/DSP/transport precedence
/// only exists in one of them.
#[derive(Debug, Clone, PartialEq)]
pub enum Decoded {
    /// Toggled a pair's mute.
    Mute(usize),
    /// Moved a live DSP parameter on a pair.
    Dsp(usize, DspParam, u8),
    /// A transport command (§8).
    Transport(Command),
}

/// Rendered for humans reading `turtle monitor` while debugging a controller
/// map, so it names the thing the way `show.toml` does (`pair0`, `cutoff`).
impl std::fmt::Display for Decoded {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Decoded::Mute(pair) => write!(f, "mute pair{pair}"),
            Decoded::Dsp(pair, param, value) => {
                write!(f, "dsp pair{pair} {} = {value}", format!("{param:?}").to_lowercase())
            }
            Decoded::Transport(cmd) => write!(f, "{}", format!("{cmd:?}").to_lowercase()),
        }
    }
}

pub struct Engine {
    transport: Transport,
    control: turtle_core::model::Control,
    active_notes: ActiveNotes,
    num_ports: usize,
    pending_preload: Option<String>,
    last_decoded: Vec<Decoded>,
}

impl Engine {
    pub fn new(show: &Show) -> Self {
        Engine {
            transport: Transport::from_show(show),
            control: show.control.clone(),
            active_notes: ActiveNotes::new(),
            num_ports: show.destinations.len().max(1),
            pending_preload: None,
            last_decoded: Vec::new(),
        }
    }

    pub fn state(&self) -> State {
        self.transport.state()
    }

    /// A song the loader should preload, taken once (set by a `Preload` action).
    pub fn take_pending_preload(&mut self) -> Option<String> {
        self.pending_preload.take()
    }

    /// What the last [`Engine::handle_midi`] decoded, taken once.
    ///
    /// A `Vec` because one CC may legitimately fan out to several pairs (the
    /// "same CC on all four delay times" mapping), and a reporter that showed
    /// only the first would misrepresent it. **Empty** means the message
    /// matched no binding — precisely what you need to see when a controller
    /// map isn't firing, so callers should report that rather than skip it.
    pub fn take_last_decoded(&mut self) -> Vec<Decoded> {
        std::mem::take(&mut self.last_decoded)
    }

    /// Record a message the scheduler dispatched, so clean-release stays correct.
    pub fn observe_output(&mut self, port: usize, bytes: &[u8]) {
        if bytes.len() >= 3 {
            self.active_notes.observe(port, bytes[0], bytes[1], bytes[2]);
        }
    }

    /// Decode + apply an incoming foot-controller message. MIDI output (clean
    /// release / panic) is written to the borrowed `midi` sink, which the caller
    /// also uses for the scheduler — so a single `AlsaMidi` serves both without
    /// the engine owning it.
    ///
    /// Per-pair mute and live DSP CC are checked first: both bypass the
    /// transport state machine entirely (valid in any state), so neither
    /// reaches `control_map::decode` / `Command`.
    pub fn handle_midi(
        &mut self,
        status: u8,
        d1: u8,
        d2: u8,
        midi: &mut impl MidiSink,
    ) -> Vec<RtCommand> {
        // Cleared first so a message that matches nothing leaves an empty
        // record, rather than the previous message's decode lingering.
        self.last_decoded.clear();
        if let Some(pair) = control_map::decode_mute(&self.control, status, d1, d2) {
            self.last_decoded.push(Decoded::Mute(pair));
            return vec![RtCommand::ToggleMute(pair)];
        }
        let dsp = control_map::decode_dsp(&self.control, status, d1, d2);
        if !dsp.is_empty() {
            self.last_decoded
                .extend(dsp.iter().map(|&(pair, param, value)| Decoded::Dsp(pair, param, value)));
            return dsp
                .into_iter()
                .map(|(pair, param, value)| RtCommand::SetDsp(pair, param, value))
                .collect();
        }
        match control_map::decode(&self.control, status, d1, d2) {
            Some(cmd) => {
                self.last_decoded.push(Decoded::Transport(cmd));
                self.handle(cmd, midi)
            }
            None => Vec::new(),
        }
    }

    /// Apply a transport command, returning the RT commands to forward.
    pub fn handle(&mut self, cmd: Command, midi: &mut impl MidiSink) -> Vec<RtCommand> {
        let mut rt = Vec::new();
        for action in self.transport.apply(cmd) {
            match action {
                Action::Preload(song) => self.pending_preload = Some(song),
                Action::StartPlayback => rt.push(RtCommand::Start),
                Action::StopPlayback => rt.push(RtCommand::Stop),
                Action::SeekToZero => rt.push(RtCommand::Seek(0)),
                Action::ReleaseNotes => self.emit_release(midi),
                Action::Panic => self.emit_panic(midi),
            }
        }
        rt
    }

    /// Send note-offs for currently-sounding notes (clean release, §8).
    fn emit_release(&mut self, midi: &mut impl MidiSink) {
        for (port, bytes) in self.active_notes.release_all() {
            midi.send(port, &bytes);
        }
    }

    /// All-notes-off + reset-all-controllers on every port/channel (§5).
    fn emit_panic(&mut self, midi: &mut impl MidiSink) {
        for port in 0..self.num_ports {
            for ch in 0u8..16 {
                midi.send(port, &[0xB0 | ch, 123, 0]); // all notes off
                midi.send(port, &[0xB0 | ch, 121, 0]); // reset all controllers
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
dsp_pair0_cutoff = { type = "cc", cc = 20 }
dsp_pair0_delay_time = { type = "cc", cc = 30 }
dsp_pair1_delay_time = { type = "cc", cc = 30 }
[[setlist]]
pc = 0
song = "01-opener"
"#;

    fn engine() -> Engine {
        Engine::new(&Show::from_toml_str(SHOW).unwrap())
    }

    #[test]
    fn select_then_start_forwards_rt_commands() {
        let mut e = engine();
        let mut midi = RecordingMidi::default();
        // Program Change 0 arms the opener.
        assert!(e.handle_midi(0xC0, 0, 0, &mut midi).is_empty());
        assert_eq!(e.take_pending_preload().as_deref(), Some("01-opener"));

        e.handle(Command::Loaded, &mut midi);
        assert_eq!(e.state(), State::Armed);

        // Note 60 = Start -> RtCommand::Start.
        assert_eq!(e.handle_midi(0x90, 60, 100, &mut midi), vec![RtCommand::Start]);
        assert_eq!(e.state(), State::Playing);
    }

    #[test]
    fn stop_releases_sounding_notes_then_rewinds() {
        let mut e = engine();
        let mut midi = RecordingMidi::default();
        e.handle(Command::Select(0), &mut midi);
        e.handle(Command::Loaded, &mut midi);
        e.handle(Command::Start, &mut midi);

        // The scheduler dispatched a note-on on port 0; the engine observes it.
        e.observe_output(0, &[0x90, 60, 100]);

        // Note 61 = Stop -> clean release (note-off) + Stop + Seek(0).
        let rt = e.handle_midi(0x90, 61, 100, &mut midi);
        assert_eq!(rt, vec![RtCommand::Stop, RtCommand::Seek(0)]);
        assert_eq!(midi.sent, vec![(0, vec![0x80, 60, 0])]);
    }

    #[test]
    fn double_stop_emits_panic_on_all_ports() {
        let mut e = engine();
        let mut midi = RecordingMidi::default();
        e.handle(Command::Select(0), &mut midi);
        e.handle(Command::Loaded, &mut midi);
        e.handle(Command::Start, &mut midi);
        e.handle(Command::Stop, &mut midi); // first stop
        midi.sent.clear();

        e.handle(Command::Stop, &mut midi); // second stop -> panic
        // 2 destinations x 16 channels x 2 messages = 64 messages.
        assert_eq!(midi.sent.len(), 64);
        assert!(midi.sent.iter().any(|(p, b)| *p == 1 && b == &[0xB0, 123, 0]));
    }

    #[test]
    fn mute_note_forwards_toggle_mute_without_touching_transport() {
        let mut e = engine();
        let mut midi = RecordingMidi::default();
        // Note 73 = mute pair 1, regardless of transport state (still Idle here).
        assert_eq!(
            e.handle_midi(0x90, 73, 100, &mut midi),
            vec![RtCommand::ToggleMute(1)]
        );
        assert_eq!(e.state(), State::Idle);
        assert!(midi.sent.is_empty());
    }

    #[test]
    fn dsp_cc_forwards_set_dsp_without_touching_transport() {
        let mut e = engine();
        let mut midi = RecordingMidi::default();
        // CC 20 = dsp_pair0_cutoff, regardless of transport state (still Idle).
        assert_eq!(
            e.handle_midi(0xB0, 20, 90, &mut midi),
            vec![RtCommand::SetDsp(0, DspParam::Cutoff, 90)]
        );
        assert_eq!(e.state(), State::Idle);
        assert!(midi.sent.is_empty());
    }

    #[test]
    fn dsp_cc_shared_by_two_pairs_fans_out_to_both() {
        let mut e = engine();
        let mut midi = RecordingMidi::default();
        // CC 30 drives both dsp_pair0_delay_time and dsp_pair1_delay_time.
        assert_eq!(
            e.handle_midi(0xB0, 30, 40, &mut midi),
            vec![
                RtCommand::SetDsp(0, DspParam::DelayTime, 40),
                RtCommand::SetDsp(1, DspParam::DelayTime, 40),
            ]
        );
    }

    #[test]
    fn decode_is_recorded_for_each_binding_kind() {
        let mut e = engine();
        let mut midi = RecordingMidi::default();

        e.handle_midi(0x90, 60, 100, &mut midi); // start
        assert_eq!(e.take_last_decoded(), vec![Decoded::Transport(Command::Start)]);

        e.handle_midi(0x90, 73, 100, &mut midi); // mute pair 1
        assert_eq!(e.take_last_decoded(), vec![Decoded::Mute(1)]);

        e.handle_midi(0xB0, 20, 90, &mut midi); // dsp_pair0_cutoff
        assert_eq!(e.take_last_decoded(), vec![Decoded::Dsp(0, DspParam::Cutoff, 90)]);
    }

    /// The record must show *every* pair a fanned-out CC drove, or `monitor`
    /// would under-report the "one CC on all four delay times" mapping.
    #[test]
    fn a_fanned_out_dsp_cc_records_every_pair_it_drove() {
        let mut e = engine();
        let mut midi = RecordingMidi::default();
        e.handle_midi(0xB0, 30, 40, &mut midi);
        assert_eq!(
            e.take_last_decoded(),
            vec![
                Decoded::Dsp(0, DspParam::DelayTime, 40),
                Decoded::Dsp(1, DspParam::DelayTime, 40),
            ]
        );
    }

    /// An unmapped message records nothing — that empty result is what `monitor`
    /// renders as "arrived, matched nothing", the whole point of map debugging.
    #[test]
    fn an_unmapped_message_records_no_decode() {
        let mut e = engine();
        let mut midi = RecordingMidi::default();
        e.handle_midi(0xB0, 99, 1, &mut midi); // CC 99 is bound to nothing
        assert!(e.take_last_decoded().is_empty());
    }

    /// `take` must not let a previous message's decode linger and be reported
    /// against a later, unmapped one.
    #[test]
    fn a_recorded_decode_does_not_leak_into_the_next_message() {
        let mut e = engine();
        let mut midi = RecordingMidi::default();
        e.handle_midi(0x90, 60, 100, &mut midi); // start — recorded
        e.handle_midi(0xB0, 99, 1, &mut midi); // unmapped — must clear it
        assert!(e.take_last_decoded().is_empty());
    }

    #[test]
    fn decoded_renders_the_way_show_toml_names_things() {
        assert_eq!(Decoded::Mute(2).to_string(), "mute pair2");
        assert_eq!(Decoded::Dsp(0, DspParam::Cutoff, 90).to_string(), "dsp pair0 cutoff = 90");
        assert_eq!(Decoded::Transport(Command::Start).to_string(), "start");
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
