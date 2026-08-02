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
    /// Re-send Start at each loop wrap (§5.1).
    restart: bool,
    /// This port's dispatch offset in milliseconds — `output_latency_ms` plus its
    /// own `offset_ms`, the same figure the scheduler applies to its cues.
    ///
    /// Held per port rather than passed in per tick because *everything* for a port
    /// lives in the timeline this defines: its pulses, its loop boundaries, and its
    /// Start messages. Having the boundary in one timeline and the pulses in another
    /// put Start ~21 ms ahead of the pulses it introduces, and a device takes Start
    /// as the downbeat.
    offset_ms: f64,
    /// Musical time at which this port's current pass ends.
    next_boundary: u64,
    /// Start is owed but not yet due — it goes out with the downbeat pulse, not
    /// when the transport command was processed.
    pending_start: bool,
    /// Musical time at which this port's pulse train last began, so a restarting
    /// port can be re-phased to the top of the loop while other ports carry on
    /// uninterrupted.
    origin: u64,
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
    /// Running total of per-loop deviations since the transport started.
    ///
    /// The per-loop figure alone is ambiguous and has misled twice: a loop is rarely
    /// a whole number of pulses, so the pulse nearest the boundary lands in one
    /// window or the next depending on tick phase, showing as `+1` followed by `-1`.
    /// That is a pulse *moving* between windows, not one created — and only the
    /// running total distinguishes it from real drift, which accumulates.
    cumulative: i64,
    /// Pulses emitted since the last loop wrap, for the per-loop audit.
    ///
    /// A loop is a known number of beats, so it owes a known number of pulses. This
    /// is the only measurement that separates "our clock is wrong" from "the synced
    /// device is not actually following it" — and without it the two are
    /// indistinguishable by ear, which cost two wrong diagnoses.
    pulses_this_loop: u32,
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
            .map(|(port, d)| ClockPort {
                port,
                clock: MidiClock::new(bpm, rate),
                restart: d.clock_restart,
                offset_ms: show.audio.output_latency_ms + d.offset_ms,
                next_boundary: 0,
                pending_start: false,
                origin: 0,
            })
            .collect();
        ClockOut { ports, bpm, rate, pulses_this_loop: 0, cumulative: 0 }
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
    pub fn retempo(&mut self, bpm: f64, origin: u64) {
        self.bpm = bpm;
        self.restart(origin);
    }

    /// Send `0xFA` Start. Called when the transport starts, before any pulse.
    ///
    /// The pulse train is rebuilt here rather than merely rewound: a device that
    /// takes Start as "reset to the top" would otherwise be handed pulses whose
    /// phase came from wherever the previous run stopped.
    pub fn start(&mut self, origin: u64) {
        self.restart(origin);
    }

    /// Rebuild every port's train, phased from `origin` — the frame count at which
    /// this run began.
    fn restart(&mut self, origin: u64) {
        let (bpm, rate) = (self.bpm, self.rate);
        self.pulses_this_loop = 0;
        self.cumulative = 0;
        for p in &mut self.ports {
            p.clock = MidiClock::new(bpm, rate);
            p.origin = origin;
            // Set on the first tick, once the loop length is known.
            p.next_boundary = 0;
            // Held until the downbeat is due in this port's own timeline.
            p.pending_start = true;
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
    ///
    /// Returns an audit line at each loop wrap: how many pulses the loop actually
    /// emitted against how many its length owes.
    pub fn tick(
        &mut self,
        elapsed: u64,
        loop_frames: u64,
        midi: &mut impl MidiSink,
    ) -> Option<String> {
        if self.ports.is_empty() {
            return None;
        }
        let (bpm, rate) = (self.bpm, self.rate);
        let first_port = self.ports[0].port;
        let owed = loop_frames as f64 / self.samples_per_pulse();
        let mut audit = None;

        for p in &mut self.ports {
            // This port's own timeline: musical time shifted by its dispatch offset,
            // exactly as its cues are. `None` = the offset has not elapsed yet, so
            // nothing — pulse, boundary or Start — is due.
            let Some(now) = crate::play::dispatch_pos(elapsed, p.offset_ms, rate) else {
                continue;
            };
            if loop_frames > 0 && p.next_boundary == 0 {
                p.next_boundary = p.origin + loop_frames;
            }

            // Has this port's pass ended? Computed from its origin, not observed:
            // the transport position is interpolated and its backwards jump is not
            // visible until up to an audio period after the audio wrapped. Tracked
            // for every port, not just restarting ones, so the audit is available
            // whichever kind the first clock port happens to be.
            if loop_frames > 0 && now >= p.next_boundary {
                let at = p.next_boundary;
                if p.restart {
                    // Finish the outgoing pass first. A pulse whose time falls
                    // between the last tick and the boundary is still owed, and
                    // rebuilding the train without emitting it drops it. Up to the
                    // sample *before* the boundary: the pulse landing exactly on it
                    // belongs to the new pass.
                    // Stop short of the pulse the new pass will supply as its
                    // downbeat. A loop is rarely an exact multiple of the pulse
                    // period, so the last pulse of the pass can sit *inside* the
                    // boundary — flushing it and then restarting emits two pulses a
                    // sample apart, one extra every loop. At 88 BPM that is 1/24 of
                    // a beat per pass, about a third of a beat after seven rounds.
                    let boundary_index =
                        (loop_frames as f64 / (60.0 * rate as f64 / (bpm * 24.0))).round() as u64;
                    let last_of_pass = p.clock.pulse_sample(boundary_index);
                    let final_since = at
                        .saturating_sub(p.origin)
                        .min(last_of_pass)
                        .saturating_sub(1);
                    if let Advance::Pulses(due) = p.clock.advance(final_since) {
                        for _ in due {
                            midi.send(p.port, &[CLOCK]);
                            if p.port == first_port {
                                self.pulses_this_loop += 1;
                            }
                        }
                    }
                    // Phased from the boundary itself, so the downbeat lands where
                    // the loop began however late in the tick we ran.
                    p.clock = MidiClock::new(bpm, rate);
                    p.origin = at;
                    midi.send(p.port, &[START]);
                }
                p.next_boundary = at + loop_frames;

                // Taken here, before the pulse loop below — that loop emits the
                // *new* pass's downbeat, and billing it to the pass that just ended
                // reported one pulse too many every loop.
                if p.port == first_port {
                    let sent = std::mem::replace(&mut self.pulses_this_loop, 0);
                    let delta = sent as f64 - owed;
                    // Rounded before accumulating: the per-loop figure is a whole
                    // number of pulses against a fractional owing, so summing the
                    // raw difference would drift by the fraction rather than
                    // reporting whether the *pulses* did.
                    self.cumulative += delta.round() as i64;
                    audit = Some(format!(
                        "[clock] loop audit: sent {sent} pulses, loop owes {owed:.2} \
                         ({delta:+.2}, cumulative {:+})",
                        self.cumulative
                    ));
                }
            }

            // The transport's own Start, held until the downbeat is due in this
            // port's timeline so it arrives with the first pulse rather than an
            // offset ahead of it.
            if p.pending_start && now >= p.origin {
                midi.send(p.port, &[START]);
                p.pending_start = false;
            }

            let since_origin = now.saturating_sub(p.origin);
            if let Advance::Pulses(due) = p.clock.advance(since_origin) {
                for _ in due {
                    midi.send(p.port, &[CLOCK]);
                    // Counted once, not once per port: every port runs the same
                    // train, so one count describes them all.
                    if p.port == first_port {
                        self.pulses_this_loop += 1;
                    }
                }
            }


        }
        audit
    }

    /// Samples between pulses at the current tempo.
    fn samples_per_pulse(&self) -> f64 {
        60.0 * self.rate as f64 / (self.bpm * 24.0)
    }
}

/// Does the song's declared tempo agree with the length of its loop (§5.1)?
///
/// # Why this check exists
///
/// Clock pulses run at `song.toml`'s `bpm`, while the audio runs at whatever tempo
/// its stems were actually rendered at. If those disagree, everything synced to the
/// clock diverges from the audio linearly and forever — a 0.5% error is a whole beat
/// every two minutes — and no amount of care inside the clock can fix it, because
/// the clock is faithfully reproducing a wrong number.
///
/// A looping song makes the real tempo checkable: a loop is virtually always a whole
/// number of beats, so `beats = loop_seconds x bpm / 60` should come out very close
/// to an integer. If it does not, the loop implies a different tempo, and that is
/// almost certainly the true one.
///
/// Returns `None` when there is nothing to check — no loop, or a loop so short that
/// rounding to the nearest beat says little.
pub fn tempo_check(bpm: f64, loop_frames: u64, rate: u32) -> Option<String> {
    if loop_frames == 0 || bpm <= 0.0 || !bpm.is_finite() {
        return None;
    }
    let secs = loop_frames as f64 / rate as f64;
    let beats = secs * bpm / 60.0;
    if beats < 2.0 {
        return None;
    }
    let nearest = beats.round();
    // The implied tempo if the loop really is `nearest` beats long.
    let implied = nearest * 60.0 / secs;
    let err_pct = (implied - bpm) / bpm * 100.0;
    // A beat every two minutes is 0.5%, which is glaring; a tenth of that is
    // inaudible over a set. 0.05% is the line between "worth saying" and "noise".
    if err_pct.abs() < 0.05 {
        return Some(format!(
            "[clock] {bpm:.3} BPM; loop {secs:.3}s = {nearest:.0} beats — tempo agrees"
        ));
    }
    let drift_s_per_min = (err_pct / 100.0).abs() * 60.0;
    let beats_per_min = drift_s_per_min * bpm / 60.0;
    Some(format!(
        "[clock] WARNING: {bpm:.3} BPM disagrees with the loop. {secs:.3}s = {beats:.3} beats \
         at {bpm:.3}, but {nearest:.0} beats implies {implied:.3} BPM ({err_pct:+.3}%). \
         Anything synced to clock will drift ~{beats_per_min:.2} beats per minute against the \
         audio. Fix `bpm` in song.toml."
    ))
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

[[destinations]]
name = "groovebox"
port = "hw:3"
clock = true
clock_restart = true

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

        c.start(0);
        let _ = c.tick(1000, 0, &mut midi);

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

        c.start(0);
        let _ = c.tick(48_000, 0, &mut midi);
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
            let _ = c.tick(tick * 48, 0, &mut midi);
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
            let _ = c.tick(tick * 48, 0, &mut midi);
        }
        let before = midi.sent(1).len();
        // The song wraps back to the top.
        let _ = c.tick(0, 0, &mut midi);
        assert_eq!(midi.sent(1).len(), before, "a wrap must send nothing");
        // And the train continues afterwards.
        let _ = c.tick(1000, 0, &mut midi);
        assert!(midi.sent(1).len() > before, "pulses resume after the wrap");
    }

    /// Start restarts the pulse phase rather than continuing from wherever the
    /// previous run left off, and it is emitted from the dispatch tick so it
    /// arrives with the downbeat rather than an offset ahead of it.
    #[test]
    fn start_restarts_the_pulse_train() {
        let mut c = ClockOut::new(&show(), 120.0, 48_000);
        let mut midi = RecordingMidi::default();
        let _ = c.tick(10_000, 0, &mut midi);

        c.start(50_000);
        let before = midi.sent(1).len();
        let _ = c.tick(50_000, 0, &mut midi);
        let batch = midi.sent(1).split_off(before);
        assert_eq!(batch.len(), 2, "Start then the downbeat, got {batch:?}");
        assert_eq!(batch[0].as_slice(), [START]);
        assert_eq!(batch[1].as_slice(), [CLOCK], "pulse 0 is due from the top");
    }

    /// A song switch changes the tempo AND restarts the train — a clock still
    /// counting from the previous song's pulse index would read the new song's
    /// position 0 as a wrap on the very first tick.
    #[test]
    fn a_song_switch_changes_tempo_and_rebases() {
        let mut c = ClockOut::new(&show(), 120.0, 48_000);
        let mut midi = RecordingMidi::default();
        for tick in 0..1000u64 {
            let _ = c.tick(tick * 48, 0, &mut midi);
        }
        c.retempo(60.0, 0);
        let before = midi.sent(1).len();
        // At 60 BPM a pulse is 2000 samples, so one second owes 24 pulses + pulse 0.
        for tick in 0..=1000u64 {
            let _ = c.tick(tick * 48, 0, &mut midi);
        }
        // Count pulses only: the switch also re-arms Start, which the next tick
        // emits alongside the new tempo's downbeat.
        let pulses =
            midi.sent(1)[before..].iter().filter(|m| m.as_slice() == [CLOCK]).count();
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
        let mut elapsed = 0u64;
        c.start(0);
        for _ in 0..(run_secs * rate / 48) {
            let _ = c.tick(elapsed, loop_frames, &mut midi);
            pos = (pos + 48) % loop_frames;
            elapsed += 48;
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
        c.start(0);
        let mut pos = 0u64;
        let mut elapsed = 0u64;
        for _ in 0..(30 * rate / 48) {
            let _ = c.tick(elapsed, 2 * rate, &mut midi_loop);
            pos = (pos + 48) % (2 * rate);
            elapsed += 48;
        }

        let mut c = ClockOut::new(&show(), 88.0, rate as u32);
        c.start(0);
        for tick in 0..(30 * rate / 48) {
            let _ = c.tick(tick * 48, 0, &mut midi_straight);
        }

        let count = |m: &RecordingMidi| {
            m.sent(1).iter().filter(|b| b.as_slice() == [CLOCK]).count() as i64
        };
        let (a, b) = (count(&midi_loop), count(&midi_straight));
        assert!((a - b).abs() <= 1, "looping sent {a} pulses, straight-through {b}");
    }

    /// Pulse timing depends on musical time alone. The clock is no longer given the
    /// transport position at all — that is now structural rather than defended, so
    /// what remains to check is that the pulse count matches elapsed time exactly.
    #[test]
    fn pulses_follow_elapsed_time_exactly() {
        let rate = 48_000u64;
        let bpm = 120.0;
        let mut c = ClockOut::new(&show(), bpm, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(0);

        let run_secs = 10u64;
        for t in 0..(run_secs * rate / 48) {
            let _ = c.tick(t * 48, 0, &mut midi);
        }

        let pulses = midi.sent(1).iter().filter(|m| m.as_slice() == [CLOCK]).count() as f64;
        let ideal = run_secs as f64 * bpm / 60.0 * 24.0;
        assert!(
            (pulses - ideal).abs() <= 1.0,
            "{pulses} pulses over {run_secs}s, expected {ideal}"
        );
    }

    /// The audit is the measurement that separates "our clock is wrong" from "the
    /// synced device is not following it". A loop of a whole number of beats owes a
    /// whole number of pulses, so the line must read within a pulse of that.
    #[test]
    fn the_loop_audit_counts_the_pulses_the_loop_owes() {
        let rate = 48_000u64;
        let bpm = 88.0;
        // 64 beats at 88 BPM: 1536 pulses per loop.
        let loop_frames = (64.0 * 60.0 / bpm * rate as f64) as u64;
        let mut c = ClockOut::new(&show(), bpm, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(0);

        let mut pos = 0u64;
        let mut elapsed = 0u64;
        let mut audits = Vec::new();
        // Two full loops.
        for _ in 0..(2 * loop_frames / 48) {
            if let Some(line) = c.tick(elapsed, loop_frames, &mut midi) {
                audits.push(line);
            }
            pos = (pos + 48) % loop_frames;
            elapsed += 48;
        }

        assert!(!audits.is_empty(), "a wrap should have produced an audit line");
        for line in &audits {
            let sent: i64 = line
                .split("sent ")
                .nth(1)
                .and_then(|s| s.split(' ').next())
                .and_then(|s| s.parse().ok())
                .expect("audit should name a pulse count");
            assert!(
                (sent - 1536).abs() <= 1,
                "a 64-beat loop owes 1536 pulses, audit says {sent}: {line}"
            );
        }
    }

    /// The point of `clock_restart`: a device playing a pattern is told to return
    /// to the top when the song does, because clock carries no position and it has
    /// no other way to learn the transport jumped.
    #[test]
    fn a_wrap_restarts_only_the_ports_that_asked() {
        let rate = 48_000u64;
        let loop_frames = 2 * rate;
        let mut c = ClockOut::new(&show(), 120.0, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(0);

        let mut pos = 0u64;
        let mut elapsed = 0u64;
        for _ in 0..(3 * loop_frames / 48) {
            let _ = c.tick(elapsed, loop_frames, &mut midi);
            pos = (pos + 48) % loop_frames;
            elapsed += 48;
        }

        let starts = |port: usize| {
            midi.sent(port).iter().filter(|m| m.as_slice() == [START]).count()
        };
        // Port 2 opted in: one Start at the transport start, plus one per wrap.
        assert!(starts(2) >= 3, "expected a Start per wrap, got {}", starts(2));
        // Port 1 takes clock but not restarts: only the transport's own Start.
        assert_eq!(starts(1), 1, "a plain clock port must not be restarted");
        assert_eq!(starts(0), 0, "a port with no clock gets nothing");
    }

    /// A restart re-phases that port's train to the top of the loop, which is the
    /// whole point: Start means "the downbeat is now". If the train kept its old
    /// phase, the first pulse after Start would arrive up to a full pulse period
    /// late — 20.8 ms at 120 BPM — and the device would place its downbeat there.
    ///
    /// Counting pulses per pass does NOT test this: a free-running train delivers
    /// the same number either way. The distinguishing property is that the downbeat
    /// pulse falls in the *same tick* as the Start.
    #[test]
    fn a_restarted_port_is_rephased_to_the_loop_top() {
        let rate = 48_000u64;
        // Deliberately not a whole number of pulses, so a free-running train would
        // be at an arbitrary phase when the wrap comes.
        let loop_frames = rate + 137;
        let mut c = ClockOut::new(&show(), 120.0, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(0);

        let mut pos = 0u64;
        let mut elapsed = 0u64;
        let mut restarts = 0;
        for _ in 0..(4 * loop_frames / 48) {
            let before = midi.sent(2).len();
            let _ = c.tick(elapsed, loop_frames, &mut midi);
            let batch = midi.sent(2).split_off(before);
            if batch.iter().any(|m| m.as_slice() == [START]) {
                restarts += 1;
                assert!(
                    batch.iter().any(|m| m.as_slice() == [CLOCK]),
                    "the downbeat pulse must accompany the Start, got {batch:?}"
                );
            }
            pos = (pos + 48) % loop_frames;
            elapsed += 48;
        }
        assert!(restarts >= 3, "expected a restart per wrap, saw {restarts}");
    }

    /// A song that does not loop must never see an extra Start — there is no wrap.
    #[test]
    fn a_non_looping_song_is_never_restarted() {
        let mut c = ClockOut::new(&show(), 120.0, 48_000);
        let mut midi = RecordingMidi::default();
        c.start(0);
        for tick in 0..2000u64 {
            let _ = c.tick(tick * 48, 0, &mut midi);
        }
        let starts = midi.sent(2).iter().filter(|m| m.as_slice() == [START]).count();
        assert_eq!(starts, 1, "only the transport's own Start");
    }

    /// The failure from hardware: at 120 BPM the loop is an exact multiple of the
    /// pulse period, so losing even a fraction of a pulse of reconstructed time
    /// dropped a whole pulse — the audit read 767 where 768 were owed, every loop.
    /// At 72 BPM the same loss fell inside a pulse and the count looked perfect.
    ///
    /// Musical time now comes from the RT thread, so the count is exact at any
    /// tempo. This checks the tempo that exposed it and one that hid it.
    #[test]
    fn the_loop_audit_is_exact_at_every_tempo() {
        let rate = 48_000u64;
        // Two tick sizes: 1 ms as the dispatch loop really runs, and a whole audio
        // period, which is how coarsely a loaded system can cross the boundary. The
        // coarse case is what catches a restart phased from "now" rather than from
        // the boundary — at 120 BPM a period is most of a pulse.
        for tick in [48u64, 1024] {
        for (bpm, beats) in [(120.0, 32u64), (72.0, 8), (66.0, 8), (88.0, 64)] {
            let loop_frames = (beats as f64 * 60.0 / bpm * rate as f64).round() as u64;
            let owed = (beats * 24) as i64;
            let mut c = ClockOut::new(&show(), bpm, rate as u32);
            let mut midi = RecordingMidi::default();
            c.start(0);

            let mut elapsed = 0u64;
            let mut audits = Vec::new();
            for _ in 0..(3 * loop_frames / tick) {
                if let Some(line) = c.tick(elapsed, loop_frames, &mut midi) {
                    audits.push(line);
                }
                elapsed += tick;
            }

            assert!(!audits.is_empty(), "{bpm} BPM: no wrap seen");
            for line in &audits {
                let sent: i64 = line
                    .split("sent ")
                    .nth(1)
                    .and_then(|s| s.split(' ').next())
                    .and_then(|s| s.parse().ok())
                    .expect("audit names a count");
                assert!(
                    (sent - owed).abs() <= 1,
                    "{bpm} BPM, {beats} beats, {tick}-sample tick: owes {owed}, \
                     audit says {sent} — {line}"
                );
            }
        }
        }
    }

    /// The regression this exists for: a Start after the transport has already been
    /// playing must phase the train from *now*, not from zero.
    ///
    /// The frame count is monotonic for the life of the daemon, so by the second
    /// Start it is large. A train rebuilt at origin 0 sees that as an enormous
    /// forward jump and emits every pulse in between — thousands at once — which is
    /// what a groovebox hears as engaging with a massive, unpredictable offset.
    #[test]
    fn a_start_after_playing_does_not_emit_a_burst() {
        let rate = 48_000u64;
        let mut c = ClockOut::new(&show(), 120.0, rate as u32);
        let mut midi = RecordingMidi::default();

        // Two minutes of frames have already been rendered this session.
        let origin = 120 * rate;
        c.start(origin);

        let before = midi.sent(1).len();
        let _ = c.tick(origin + 48, 0, &mut midi);
        let batch = midi.sent(1).split_off(before);
        assert_eq!(batch.len(), 2, "Start and one downbeat pulse, got {batch:?}");
        assert_eq!(batch[0].as_slice(), [START], "Start leads");
        assert_eq!(batch[1].as_slice(), [CLOCK], "then the downbeat, not a burst");

        // And the train then runs at the right rate from there.
        for t in 1..(10 * rate / 48) {
            let _ = c.tick(origin + t * 48, 0, &mut midi);
        }
        let pulses = midi.sent(1).iter().filter(|m| m.as_slice() == [CLOCK]).count() as f64;
        let ideal = 10.0 * 120.0 / 60.0 * 24.0;
        assert!((pulses - ideal).abs() <= 1.0, "{pulses} pulses in 10s, expected {ideal}");
    }

    /// The wrap must be recognised from musical time, not from watching the
    /// transport position — and the restart must be phased to the boundary itself.
    ///
    /// Observing the position cannot be timely: it is interpolated from an anchor
    /// published once per audio period, so a wrap is invisible for up to 21 ms after
    /// the audio actually wrapped. Start then arrived that late and a device takes
    /// Start as "the downbeat is now", landing 0-21 ms off, differently every loop.
    /// Here the tick that crosses the boundary is deliberately coarse — a whole
    /// audio period — and the downbeat must still be placed at the boundary.
    #[test]
    fn the_wrap_is_computed_from_musical_time_not_observed() {
        let rate = 48_000u64;
        let bpm = 120.0;
        let loop_frames = 16 * rate; // 32 beats
        let mut c = ClockOut::new(&show(), bpm, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(0);

        // Tick in whole audio periods, so the boundary is always crossed *inside* a
        // tick rather than landing on one — the worst case for observing it.
        let period = 1024u64;
        let mut elapsed = 0u64;
        let mut boundaries = 0;
        while elapsed < 3 * loop_frames {
            let before = midi.sent(2).len();
            let _ = c.tick(elapsed, loop_frames, &mut midi);
            let batch = midi.sent(2).split_off(before);
            if let Some(i) = batch.iter().position(|m| m.as_slice() == [START]) {
                boundaries += 1;
                // The pulses after the Start in this batch are the new pass's, and
                // the first of them is its downbeat. Since the boundary fell inside
                // this tick, the train must already be phased there: the number of
                // pulses owed is what elapsed-minus-boundary calls for, not a burst.
                let after = batch.len() - i - 1;
                assert!(
                    after <= 2,
                    "downbeat should be one pulse (or two at a period boundary), got {after}"
                );
            }
            elapsed += period;
        }
        assert!(boundaries >= 2, "expected a restart per loop, saw {boundaries}");
    }

    /// Boundaries are exact multiples of the loop from the run's origin, so a long
    /// show cannot accumulate error in *where* the restarts land.
    #[test]
    fn restart_points_do_not_drift_over_many_loops() {
        let rate = 48_000u64;
        let loop_frames = 16 * rate;
        let mut c = ClockOut::new(&show(), 120.0, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(0);

        let mut elapsed = 0u64;
        let mut starts_at = Vec::new();
        while elapsed < 20 * loop_frames {
            let before = midi.sent(2).len();
            let _ = c.tick(elapsed, loop_frames, &mut midi);
            if midi.sent(2)[before..].iter().any(|m| m.as_slice() == [START]) {
                starts_at.push(elapsed);
            }

            elapsed += 997; // deliberately not a divisor of anything
        }
        assert!(starts_at.len() >= 10, "need several loops: {}", starts_at.len());
        // The first Start opens the run; the rest are wraps, one loop apart.
        for (i, at) in starts_at.iter().skip(1).enumerate() {
            let ideal = (i as u64 + 1) * loop_frames;
            assert!(
                at.abs_diff(ideal) < 997,
                "restart {i} at {at}, expected within a tick of {ideal}"
            );
        }
    }

    /// A restarting port must deliver a full loop's pulses on every pass.
    ///
    /// The loop audit cannot check this: it counts the first clock port, which is
    /// not the restarting one, so a mis-phased restart is invisible to it. Phasing
    /// the new train from "now" rather than from the boundary loses whatever part of
    /// a pulse the tick overshot by — at 120 BPM with a period-sized tick, close to
    /// a whole pulse every loop, which is a device slipping a little each time round.
    #[test]
    fn a_restarting_port_delivers_a_full_loop_every_pass() {
        let rate = 48_000u64;
        let bpm = 120.0;
        let beats = 32u64;
        let loop_frames = (beats as f64 * 60.0 / bpm * rate as f64) as u64;
        let owed_per_loop = beats * 24;
        let passes = 5u64;

        let mut c = ClockOut::new(&show(), bpm, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(0);

        // Period-sized ticks: the boundary always falls inside a tick.
        let mut elapsed = 0u64;
        while elapsed < passes * loop_frames {
            let _ = c.tick(elapsed, loop_frames, &mut midi);
            elapsed += 1024;
        }

        let pulses = midi.sent(2).iter().filter(|m| m.as_slice() == [CLOCK]).count() as i64;
        let ideal = (passes * owed_per_loop) as i64;
        assert!(
            (pulses - ideal).abs() <= 1,
            "{pulses} pulses over {passes} passes, expected {ideal} — \
             a mis-phased restart loses part of a pulse each loop"
        );
    }

    /// After a restart, pulses must fall at the boundary plus whole pulse periods —
    /// phased to where the loop began, not to whenever the tick noticed it.
    ///
    /// The pulse *count* cannot show this, because flushing the outgoing pass makes
    /// the count come out right either way. What differs is placement: a train
    /// phased from "now" puts every pulse of the new pass late by however far the
    /// tick overshot the boundary, which is where a device puts its downbeat.
    #[test]
    fn a_restarted_train_is_phased_to_the_boundary() {
        let rate = 48_000u64;
        let bpm = 120.0;
        let spp = 60.0 * rate as f64 / (bpm * 24.0); // 1000 samples
        let loop_frames = 16 * rate;
        // Deliberately not a divisor of the loop, so boundaries fall mid-tick.
        let tick = 999u64;

        let mut c = ClockOut::new(&show(), bpm, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(0);

        let mut elapsed = 0u64;
        let mut boundary = 0u64;
        let mut index = 0u64;
        let mut checked = 0;
        let mut seen_first_start = false;
        while elapsed < 3 * loop_frames {
            let before = midi.sent(2).len();
            let _ = c.tick(elapsed, loop_frames, &mut midi);
            for m in midi.sent(2).split_off(before) {
                if m.as_slice() == [START] {
                    // The run's own Start opens the first pass; every later one
                    // marks a wrap.
                    if seen_first_start {
                        boundary += loop_frames;
                    }
                    seen_first_start = true;
                    index = 0;
                } else if m.as_slice() == [CLOCK] {
                    let ideal = boundary + (index as f64 * spp).round() as u64;
                    // Emitted on the first tick at or after its ideal time.
                    assert!(
                        elapsed >= ideal && elapsed - ideal < tick,
                        "pulse {index} of the pass at {elapsed}, ideal {ideal}"
                    );
                    index += 1;
                    checked += 1;
                }
            }
            elapsed += tick;
        }
        assert!(checked > 2000, "expected thousands of pulses, checked {checked}");
    }

    /// Start and the pulses it introduces must live in the same timeline.
    ///
    /// A port's dispatch offset delays its cues so they line up with *audible*
    /// audio, and the pulse train honours it. Start did not: it went out the moment
    /// the boundary was crossed, arriving a full offset — ~21 ms at a typical
    /// `output_latency_ms` — ahead of the pulses it introduces, and a device takes
    /// Start as the downbeat. This asserts they arrive together.
    #[test]
    fn start_is_delayed_by_the_ports_offset_like_its_pulses() {
        let rate = 48_000u64;
        // 20 ms of output latency, as a real show has once calibrated.
        let toml = SHOW.replace("[audio]\ndevice = \"hw:0\"", "[audio]\ndevice = \"hw:0\"\noutput_latency_ms = 20.0");
        let show = Show::from_toml_str(&toml).unwrap();
        let offset_samples = 20 * rate / 1000;

        let mut c = ClockOut::new(&show, 120.0, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(0);

        // Nothing at all until the offset has elapsed.
        let mut elapsed = 0u64;
        while elapsed < offset_samples {
            let _ = c.tick(elapsed, 0, &mut midi);
            assert!(
                midi.sent(1).is_empty(),
                "nothing is due inside the offset, got {:?} at {elapsed}",
                midi.sent(1)
            );
            elapsed += 48;
        }
        // And then Start arrives with the downbeat, not before it.
        let _ = c.tick(elapsed, 0, &mut midi);
        let batch = midi.sent(1);
        assert_eq!(batch.len(), 2, "Start and the downbeat together, got {batch:?}");
        assert_eq!(batch[0].as_slice(), [START]);
        assert_eq!(batch[1].as_slice(), [CLOCK]);
    }

    /// The drift reported at 88 BPM: a loop that is not an exact multiple of the
    /// pulse period made a restarting port emit ONE extra pulse per pass.
    ///
    /// The last pulse of the pass sits just inside the boundary, so flushing it and
    /// then restarting put two pulses a sample apart. One pulse per 43.6 s is 1/24
    /// of a beat, about a third of a beat after seven rounds — audibly out of sync,
    /// while the tempo looks correct on any single pass.
    #[test]
    fn a_restarting_port_emits_no_extra_pulse_at_an_awkward_loop_length() {
        let rate = 48_000u64;
        let bpm = 88.0;
        let spp = 60.0 * rate as f64 / (bpm * 24.0); // 1363.63... samples
        // 64 beats at 88 BPM is 2,094,545.45 samples: the boundary falls between
        // pulses however the file length is rounded, so test both sides of it.
        for loop_frames in [2_094_545u64, 2_094_546, 2_094_544] {
            let owed = (loop_frames as f64 / spp).round() as i64;
            let passes = 8u64;

            let mut c = ClockOut::new(&show(), bpm, rate as u32);
            let mut midi = RecordingMidi::default();
            c.start(0);

            let mut elapsed = 0u64;
            while elapsed < passes * loop_frames {
                let _ = c.tick(elapsed, loop_frames, &mut midi);
                elapsed += 48;
            }

            let pulses = midi.sent(2).iter().filter(|m| m.as_slice() == [CLOCK]).count() as i64;
            let ideal = passes as i64 * owed;
            assert!(
                (pulses - ideal).abs() <= 1,
                "loop {loop_frames}: {pulses} pulses over {passes} passes, expected \
                 {ideal} ({owed} per pass) — an extra pulse per loop is a third of a \
                 beat after seven rounds"
            );
        }
    }

    /// The per-loop figure alternates +1 / -1 when the loop is not a whole number of
    /// pulses, because the pulse nearest the boundary lands in one window or the
    /// next. That is not drift, and the running total is what says so — it returns
    /// to zero, where real drift would climb.
    #[test]
    fn the_audit_reports_a_running_total_that_stays_flat() {
        let rate = 48_000u64;
        let bpm = 88.0;
        // 64 beats at 88 BPM: 1535.99967 pulses, so the boundary pulse alternates.
        let loop_frames = 2_094_545u64;
        let mut c = ClockOut::new(&show(), bpm, rate as u32);
        let mut midi = RecordingMidi::default();
        c.start(0);

        let mut elapsed = 0u64;
        let mut totals = Vec::new();
        while elapsed < 30 * loop_frames {
            if let Some(line) = c.tick(elapsed, loop_frames, &mut midi) {
                let cum: i64 = line
                    .split("cumulative ")
                    .nth(1)
                    .and_then(|s| s.trim_end_matches(')').parse().ok())
                    .expect("audit reports a running total");
                totals.push(cum);
            }
            elapsed += 48;
        }

        assert!(totals.len() >= 20, "expected many loops, got {}", totals.len());
        // Individual loops may be off by one; the total must not walk away.
        assert!(
            totals.iter().all(|t| t.abs() <= 1),
            "the running total must stay flat, got {totals:?}"
        );
        assert_eq!(*totals.last().unwrap(), 0, "and end where it started");
    }

}
