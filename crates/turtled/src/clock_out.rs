//! MIDI clock master: pulses and transport bytes to the ports that asked (§5).
//!
//! # What is sent, and what deliberately is not
//!
//! Tempo only: 24 `0xF8` pulses per quarter note while playing, `0xFA` on start and
//! `0xFC` on stop. No Song Position Pointer and no `0xFB` Continue, so downstream
//! learns how *fast* the song is going but not *where* it is.
//!
//! That is a smaller feature than it could be, and it buys something concrete: a
//! loop wrap needs no handling at all. With position on the wire, every wrap would
//! have to re-send SPP — which most gear responds to by briefly stopping, audibly —
//! and the constant-tempo limit (§14: no tempo-map following yet) would start to
//! matter. Without it, a wrap is simply a discontinuity the pulse train rebases
//! across, and the tempo it reports stays correct throughout.
//!
//! # Why the ~1 ms control loop is good enough
//!
//! Measured on the Pi with `--clock-probe` before this was written: pulses land
//! within one dispatch tick, peak-to-peak 1.0–1.2 ms against a 28.4 ms pulse
//! (3.5–4.3%), mean lateness a stable half-tick across 4800 pulses. Pulse times are
//! derived from the transport position rather than accumulated, so there is no
//! drift — the *average* tempo downstream is exact, and only each pulse's phase
//! moves. The alternative, a thread sleeping to each pulse deadline, would buy
//! sub-100 µs for real added complexity, and the measurement said it was not needed.

use turtle_core::model::Show;
use turtle_core::timing::{Advance, MidiClock};

use crate::backend::MidiSink;

/// MIDI System Real-Time status bytes (single-byte messages, no data).
const CLOCK: u8 = 0xF8;
const START: u8 = 0xFA;
const STOP: u8 = 0xFC;

/// One clock-enabled destination: which port, and its own pulse train.
struct ClockPort {
    /// Index into the MIDI sink's ports, matching `show.destinations` order.
    port: usize,
    clock: MidiClock,
}

/// Drives MIDI clock for every destination with `clock = true`.
///
/// Each port keeps its **own** [`MidiClock`], because each has its own `offset_ms`
/// and therefore its own idea of what the current position is. Sharing one clock
/// would silently give every port the first one's offset.
pub struct ClockOut {
    ports: Vec<ClockPort>,
    bpm: f64,
    rate: u32,
}

impl ClockOut {
    /// Build from the show's destinations. Empty when none opted in, which makes
    /// every method below a no-op — there is no separate "clock disabled" path.
    pub fn new(show: &Show, bpm: f64, rate: u32) -> Self {
        let ports = show
            .destinations
            .iter()
            .enumerate()
            .filter(|(_, d)| d.clock)
            .map(|(port, _)| ClockPort { port, clock: MidiClock::new(bpm, rate) })
            .collect();
        ClockOut { ports, bpm, rate }
    }

    /// Whether any destination asked for clock.
    pub fn is_enabled(&self) -> bool {
        !self.ports.is_empty()
    }

    /// Point the clock at a newly armed song's tempo (§14: nominal BPM per song).
    ///
    /// A song switch must change the tempo *and* restart the pulse train — the new
    /// song's position starts from 0, and a clock still counting from the previous
    /// song's index would treat that as a wrap on the first tick.
    pub fn retempo(&mut self, bpm: f64) {
        self.bpm = bpm;
        for p in &mut self.ports {
            p.clock = MidiClock::new(bpm, self.rate);
        }
    }

    /// Send `0xFA` Start. Called when the transport starts, before any pulse.
    ///
    /// The pulse train is rebuilt here rather than merely rewound: a device that
    /// takes Start as "reset to the top" would otherwise be handed pulses whose
    /// phase came from wherever the previous run stopped.
    pub fn start(&mut self, midi: &mut impl MidiSink) {
        let bpm = self.bpm;
        let rate = self.rate;
        for p in &mut self.ports {
            p.clock = MidiClock::new(bpm, rate);
            midi.send(p.port, &[START]);
        }
    }

    /// Send `0xFC` Stop.
    ///
    /// Downstream gear typically reverts to its own tempo here — for a delay pedal
    /// that means a ringing tail can change time mid-decay. That is the accepted
    /// consequence of clock running only while playing; keeping it free-running
    /// while armed would be the alternative.
    pub fn stop(&mut self, midi: &mut impl MidiSink) {
        for p in &mut self.ports {
            midi.send(p.port, &[STOP]);
        }
    }

    /// Emit whatever pulses are due at `pos`, per port, applying each port's
    /// dispatch offset exactly as cued events do.
    ///
    /// `offsets` is indexed by port, in `show.destinations` order — the same slice
    /// the scheduler uses, so a port's latency trim cannot mean one thing for its
    /// cues and another for its clock.
    pub fn tick(&mut self, pos: u64, offsets: &[f64], midi: &mut impl MidiSink) {
        for p in &mut self.ports {
            // `None` = still inside the offset at the start of a song; nothing due.
            let Some(pos_adj) = crate::play::dispatch_pos(pos, offsets[p.port], self.rate)
            else {
                continue;
            };
            // A wrap, rewind or seek rebases without emitting: no time passed, so
            // those pulses were never due. Firing them would read downstream as a
            // burst of tempo, which is worse than the momentary gap.
            if let Advance::Pulses(due) = p.clock.advance(pos_adj) {
                for _ in due {
                    midi.send(p.port, &[CLOCK]);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Records what reached each port, so a test can assert one port stayed silent.
    #[derive(Default)]
    struct RecordingMidi {
        sent: Vec<(usize, Vec<u8>)>,
    }
    impl MidiSink for RecordingMidi {
        fn send(&mut self, port: usize, bytes: &[u8]) {
            self.sent.push((port, bytes.to_vec()));
        }
    }
    impl RecordingMidi {
        fn sent(&self, port: usize) -> Vec<Vec<u8>> {
            self.sent.iter().filter(|(p, _)| *p == port).map(|(_, b)| b.clone()).collect()
        }
    }

    const SHOW: &str = r#"
[show]
name = "x"
playback_rate = 48000
[audio]
device = "hw:0"

[[destinations]]
name = "lights"
port = "hw:1"
[[destinations]]
name = "pedals"
port = "hw:2"
clock = true

[control]
input_port = "hw:1"
select_channel = 1
start = { type = "note", note = 60 }
stop  = { type = "note", note = 61 }
next  = { type = "note", note = 62 }
prev  = { type = "note", note = 63 }
panic = { type = "note", note = 65 }
mute  = { type = "note", notes = [72, 73, 74, 75] }
"#;

    fn show() -> Show {
        Show::from_toml_str(SHOW).unwrap()
    }

    /// Clock is opt-in: a port that did not ask for it must stay quiet, or every
    /// lighting rig in the show starts parsing 48 bytes a second it will ignore.
    #[test]
    fn only_the_ports_that_asked_receive_clock() {
        let mut c = ClockOut::new(&show(), 120.0, 48_000);
        let mut midi = RecordingMidi::default();
        assert!(c.is_enabled());

        c.start(&mut midi);
        c.tick(1000, &[0.0, 0.0], &mut midi);

        assert!(midi.sent(0).is_empty(), "the non-clock port must be silent");
        let pedals = midi.sent(1);
        assert_eq!(pedals[0], vec![START], "Start comes first");
        assert!(pedals[1..].iter().all(|m| m == &vec![CLOCK]), "then pulses: {pedals:?}");
    }

    /// A show with no clock destinations must do nothing at all, rather than
    /// needing a separate disabled path at every call site.
    #[test]
    fn a_show_with_no_clock_destinations_is_inert() {
        let toml = SHOW.replace("clock = true", "");
        let mut c = ClockOut::new(&Show::from_toml_str(&toml).unwrap(), 120.0, 48_000);
        let mut midi = RecordingMidi::default();
        assert!(!c.is_enabled());

        c.start(&mut midi);
        c.tick(48_000, &[0.0, 0.0], &mut midi);
        c.stop(&mut midi);
        assert!(midi.sent(0).is_empty() && midi.sent(1).is_empty(), "nothing should be sent");
    }

    /// 24 pulses per quarter note is the whole specification. At 120 BPM one beat
    /// is half a second, so half a second of transport owes exactly 24 pulses.
    #[test]
    fn a_beat_is_twenty_four_pulses() {
        let mut c = ClockOut::new(&show(), 120.0, 48_000);
        let mut midi = RecordingMidi::default();
        // Tick every 48 samples (1 ms), through one beat.
        for tick in 0..=500u64 {
            c.tick(tick * 48, &[0.0, 0.0], &mut midi);
        }
        let pulses = midi.sent(1).iter().filter(|m| m.as_slice() == [CLOCK]).count();
        assert_eq!(pulses, 25, "pulse 0 plus 24 more across one beat");
    }

    /// A loop wrap must not fire a burst of pulses. This is the failure the probe
    /// found on hardware, in the path that actually reaches the wire.
    #[test]
    fn a_loop_wrap_does_not_emit_a_burst() {
        let mut c = ClockOut::new(&show(), 120.0, 48_000);
        let mut midi = RecordingMidi::default();
        for tick in 0..2000u64 {
            c.tick(tick * 48, &[0.0, 0.0], &mut midi);
        }
        let before = midi.sent(1).len();
        // The song wraps back to the top.
        c.tick(0, &[0.0, 0.0], &mut midi);
        assert_eq!(midi.sent(1).len(), before, "a wrap must send nothing");
        // And the train continues afterwards.
        c.tick(1000, &[0.0, 0.0], &mut midi);
        assert!(midi.sent(1).len() > before, "pulses resume after the wrap");
    }

    /// Start must restart the pulse phase, not continue from wherever the previous
    /// run left off — gear that treats Start as "reset to the top" would otherwise
    /// be handed a train whose phase is meaningless.
    #[test]
    fn start_restarts_the_pulse_train() {
        let mut c = ClockOut::new(&show(), 120.0, 48_000);
        let mut midi = RecordingMidi::default();
        c.tick(10_000, &[0.0, 0.0], &mut midi);

        c.start(&mut midi);
        let after_start = midi.sent(1).len();
        // Position 0 is pulse 0's time, so it is due immediately after a restart.
        c.tick(0, &[0.0, 0.0], &mut midi);
        assert_eq!(
            midi.sent(1).len(),
            after_start + 1,
            "pulse 0 should be due again from the top"
        );
    }

    /// A song switch changes the tempo AND restarts the train — a clock still
    /// counting from the previous song's pulse index would read the new song's
    /// position 0 as a wrap on the very first tick.
    #[test]
    fn a_song_switch_changes_tempo_and_rebases() {
        let mut c = ClockOut::new(&show(), 120.0, 48_000);
        let mut midi = RecordingMidi::default();
        for tick in 0..1000u64 {
            c.tick(tick * 48, &[0.0, 0.0], &mut midi);
        }
        c.retempo(60.0);
        let before = midi.sent(1).len();
        // At 60 BPM a pulse is 2000 samples, so one second owes 24 pulses + pulse 0.
        for tick in 0..=1000u64 {
            c.tick(tick * 48, &[0.0, 0.0], &mut midi);
        }
        let pulses = midi.sent(1).len() - before;
        assert_eq!(pulses, 25, "half the pulses of the old tempo over the same span");
    }
}
