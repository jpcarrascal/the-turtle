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
    /// Musical time since the transport started, in samples, accumulated **across
    /// loop wraps**.
    ///
    /// The pulse train runs on this rather than on the raw transport position,
    /// because the position is not monotonic: it jumps back at every wrap. Deriving
    /// pulses from it restarted the train at pulse 0 each iteration, and since a
    /// loop is rarely a whole number of pulses, each iteration emitted a fractional
    /// pulse too many. That accumulates — measured in simulation at 1.5 beats over
    /// two minutes on a 2-second loop, which is what a drum machine hears as
    /// steadily running fast.
    elapsed: u64,
    /// Highest transport position seen in the current pass through the song.
    ///
    /// A high-water mark rather than "the previous position", because the
    /// interpolated position jitters backwards by a few samples and merely ignoring
    /// those steps is not enough: the recovery would then be counted as forward
    /// motion, inflating musical time by the size of every blip. Measuring progress
    /// beyond the high-water mark makes the total exactly the distance travelled.
    high_water: Option<u64>,
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
        ClockOut { ports, bpm, rate, elapsed: 0, high_water: None }
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
        self.restart();
    }

    /// Send `0xFA` Start. Called when the transport starts, before any pulse.
    ///
    /// The pulse train is rebuilt here rather than merely rewound: a device that
    /// takes Start as "reset to the top" would otherwise be handed pulses whose
    /// phase came from wherever the previous run stopped.
    pub fn start(&mut self, midi: &mut impl MidiSink) {
        self.restart();
        for p in &mut self.ports {
            midi.send(p.port, &[START]);
        }
    }

    /// Rewind musical time and rebuild every port's train.
    fn restart(&mut self) {
        let (bpm, rate) = (self.bpm, self.rate);
        self.elapsed = 0;
        self.high_water = None;
        for p in &mut self.ports {
            p.clock = MidiClock::new(bpm, rate);
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

    /// Emit whatever pulses are due, per port, applying each port's dispatch offset
    /// exactly as cued events do.
    ///
    /// `loop_frames` is the length the transport wraps at, or 0 when the song does
    /// not loop. It is needed to know how much musical time a backwards jump
    /// covered: the position went from near `loop_frames` to near 0, and the music
    /// in between really did play.
    ///
    /// `offsets` is indexed by port, in `show.destinations` order — the same slice
    /// the scheduler uses, so a port's latency trim cannot mean one thing for its
    /// cues and another for its clock.
    pub fn tick(&mut self, pos: u64, loop_frames: u64, offsets: &[f64], midi: &mut impl MidiSink) {
        if self.ports.is_empty() {
            return;
        }
        // Musical time only ever moves forward, even though the position does not.
        let (delta, new_high) = self.step(pos, loop_frames);
        self.high_water = Some(new_high);
        self.elapsed += delta;

        for p in &mut self.ports {
            // `None` = still inside a positive offset at the start; nothing due.
            let Some(adj) = crate::play::dispatch_pos(self.elapsed, offsets[p.port], self.rate)
            else {
                continue;
            };
            // `elapsed` is monotonic, so `advance` sees a discontinuity only on a
            // genuine restart — never on a wrap, which is the entire point.
            if let Advance::Pulses(due) = p.clock.advance(adj) {
                for _ in due {
                    midi.send(p.port, &[CLOCK]);
                }
            }
        }
    }
    /// One tick's worth of musical time, and the new high-water mark.
    ///
    /// Three ways the position can move backwards, and they mean different things:
    ///
    /// * **A loop wrap** — back by most of a loop. The music kept playing, so the
    ///   increment is the rest of the loop plus the new position.
    /// * **A reposition** — a large backwards move in a song that does not loop, or
    ///   one too small to be a wrap. No musical time passed, and the clock carries
    ///   on from wherever the transport now is.
    /// * **Interpolation noise** — a few samples, because the position is
    ///   extrapolated between anchors the RT thread publishes once per audio period
    ///   and a fresh anchor can land behind the extrapolation. This must contribute
    ///   nothing *and* leave the mark alone: clamping the step to zero but then
    ///   counting the recovery as forward motion inflates musical time by the size
    ///   of every blip, which is the same drift in miniature.
    fn step(&self, pos: u64, loop_frames: u64) -> (u64, u64) {
        let Some(high) = self.high_water else {
            // First tick of a run: nothing elapsed yet.
            return (0, pos);
        };
        if pos >= high {
            return (pos - high, pos);
        }
        let backwards = high - pos;
        if loop_frames > 0 && backwards > loop_frames / 2 {
            // `saturating_sub`: the extrapolation can be a little past the loop end
            // at the moment the wrap is spotted.
            (loop_frames.saturating_sub(high) + pos, pos)
        } else if backwards > self.noise_floor() {
            (0, pos)
        } else {
            (0, high)
        }
    }

    /// A backwards move smaller than this is interpolation noise, not a reposition:
    /// 100 ms, far larger than the few samples an anchor update can shift the
    /// extrapolation, and far smaller than any real seek.
    fn noise_floor(&self) -> u64 {
        self.rate as u64 / 10
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
        c.tick(1000, 0, &[0.0, 0.0], &mut midi);

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
        c.tick(48_000, 0, &[0.0, 0.0], &mut midi);
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
            c.tick(tick * 48, 0, &[0.0, 0.0], &mut midi);
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
            c.tick(tick * 48, 0, &[0.0, 0.0], &mut midi);
        }
        let before = midi.sent(1).len();
        // The song wraps back to the top.
        c.tick(0, 0, &[0.0, 0.0], &mut midi);
        assert_eq!(midi.sent(1).len(), before, "a wrap must send nothing");
        // And the train continues afterwards.
        c.tick(1000, 0, &[0.0, 0.0], &mut midi);
        assert!(midi.sent(1).len() > before, "pulses resume after the wrap");
    }

    /// Start must restart the pulse phase, not continue from wherever the previous
    /// run left off — gear that treats Start as "reset to the top" would otherwise
    /// be handed a train whose phase is meaningless.
    #[test]
    fn start_restarts_the_pulse_train() {
        let mut c = ClockOut::new(&show(), 120.0, 48_000);
        let mut midi = RecordingMidi::default();
        c.tick(10_000, 0, &[0.0, 0.0], &mut midi);

        c.start(&mut midi);
        let after_start = midi.sent(1).len();
        // Position 0 is pulse 0's time, so it is due immediately after a restart.
        c.tick(0, 0, &[0.0, 0.0], &mut midi);
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
            c.tick(tick * 48, 0, &[0.0, 0.0], &mut midi);
        }
        c.retempo(60.0);
        let before = midi.sent(1).len();
        // At 60 BPM a pulse is 2000 samples, so one second owes 24 pulses + pulse 0.
        for tick in 0..=1000u64 {
            c.tick(tick * 48, 0, &[0.0, 0.0], &mut midi);
        }
        let pulses = midi.sent(1).len() - before;
        assert_eq!(pulses, 25, "half the pulses of the old tempo over the same span");
    }
    /// The drift found on hardware: a drum machine synced to a looping song ran
    /// steadily fast, almost a beat out after two minutes.
    ///
    /// A loop is rarely a whole number of pulses, so restarting the train at pulse
    /// 0 on every wrap emitted a fractional pulse too many each iteration — and it
    /// accumulates. Simulated at these numbers it was +36 pulses over two minutes,
    /// 1.5 beats. Running the train on accumulated musical time instead makes the
    /// count depend only on elapsed time, not on how often the song wrapped.
    #[test]
    fn a_short_loop_does_not_accumulate_pulses() {
        let rate = 48_000u64;
        let bpm = 88.0;
        let mut c = ClockOut::new(&show(), bpm, rate as u32);
        let mut midi = RecordingMidi::default();

        let loop_frames = 2 * rate; // a 2-second loop: 59 wraps in two minutes
        let run_secs = 120u64;
        let mut pos = 0u64;
        c.start(&mut midi);
        for _ in 0..(run_secs * rate / 48) {
            c.tick(pos, loop_frames, &[0.0, 0.0], &mut midi);
            pos = (pos + 48) % loop_frames;
        }

        let pulses = midi.sent(1).iter().filter(|m| m.as_slice() == [CLOCK]).count() as f64;
        let ideal = run_secs as f64 * bpm / 60.0 * 24.0;
        let drift_beats = (pulses - ideal) / 24.0;
        assert!(
            drift_beats.abs() < 0.05,
            "drifted {drift_beats:.2} beats over {run_secs}s ({pulses} pulses vs {ideal} ideal)"
        );
    }

    /// The same song played straight through must give the same count — the point
    /// is that the pulse total depends on elapsed time and nothing else.
    #[test]
    fn looping_and_not_looping_emit_the_same_pulse_count() {
        let rate = 48_000u64;
        let mut midi_loop = RecordingMidi::default();
        let mut midi_straight = RecordingMidi::default();

        let mut c = ClockOut::new(&show(), 88.0, rate as u32);
        c.start(&mut midi_loop);
        let mut pos = 0u64;
        for _ in 0..(30 * rate / 48) {
            c.tick(pos, 2 * rate, &[0.0, 0.0], &mut midi_loop);
            pos = (pos + 48) % (2 * rate);
        }

        let mut c = ClockOut::new(&show(), 88.0, rate as u32);
        c.start(&mut midi_straight);
        for tick in 0..(30 * rate / 48) {
            c.tick(tick * 48, 0, &[0.0, 0.0], &mut midi_straight);
        }

        let count = |m: &RecordingMidi| {
            m.sent(1).iter().filter(|b| b.as_slice() == [CLOCK]).count() as i64
        };
        let (a, b) = (count(&midi_loop), count(&midi_straight));
        assert!((a - b).abs() <= 1, "looping sent {a} pulses, straight-through {b}");
    }

    /// The position is interpolated between anchors the RT thread publishes once
    /// per audio period, so it steps back by a few samples now and then — most
    /// often while the clock settles at the start of playback.
    ///
    /// Treating those as loop wraps added a whole loop of musical time each. The
    /// pulses are not simply doubled, because `MidiClock` recognises the resulting
    /// huge forward jump as a seek and rebases — so what a synced device hears is
    /// the train repeatedly losing and re-finding its phase, which is exactly the
    /// "scrambles for a second, then locks" reported from hardware. This asserts
    /// against elapsed musical time, which catches both shapes.
    #[test]
    fn a_few_samples_backwards_is_noise_not_a_wrap() {
        let rate = 48_000u64;
        let bpm = 88.0;
        let loop_frames = 2 * rate;
        let mut c = ClockOut::new(&show(), bpm, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(&mut midi);

        // Ten seconds of forward motion, well short of the 2-second loop's wrap
        // (the position never actually wraps here), with a 20-sample backwards
        // blip every 100 ticks.
        let run_secs = 10u64;
        let ticks = run_secs * rate / 48;
        let mut pos = 0u64;
        for t in 0..ticks {
            c.tick(pos, loop_frames, &[0.0, 0.0], &mut midi);
            if t % 100 == 99 {
                c.tick(pos - 20, loop_frames, &[0.0, 0.0], &mut midi);
            }
            pos += 48;
        }

        let pulses = midi.sent(1).iter().filter(|m| m.as_slice() == [CLOCK]).count() as f64;
        let ideal = run_secs as f64 * bpm / 60.0 * 24.0;
        assert!(
            (pulses - ideal).abs() <= 1.0,
            "{pulses} pulses over {run_secs}s, expected {ideal}: backwards blips \
             must not add musical time"
        );
    }

}
