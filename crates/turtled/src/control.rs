//! `turtled control <bundle> [song]` — drive playback from a live MIDI device.
//!
//! Any MIDI controller on the show's `[control] input_port` works: Program
//! Change selects a song, the mapped notes trigger start/stop/next/prev/panic
//! (spec §8), and the mute notes / `dsp_*` CCs act directly on the mixer
//! regardless of transport state. The decode + transport logic already lives
//! in [`crate::control_map`] and [`crate::engine`] (both unit-tested); this
//! module adds the two missing pieces:
//!
//!   * [`MidiParser`] — a **portable** MIDI byte-stream parser (running status,
//!     interleaved real-time bytes). Unit-tested on the dev Mac.
//!   * [`run`] — the **`cfg(linux)`** loop that reads ALSA rawmidi, feeds the
//!     parser into the engine, and forwards the resulting RT commands to the
//!     audio thread. Verified on the Pi.

/// Incremental parser for a raw MIDI byte stream.
///
/// ALSA rawmidi hands us bytes, not tidy messages: channel-voice messages can
/// use *running status* (a status byte omitted when it repeats), and single-byte
/// system real-time messages (clock, active-sensing…) can appear *between* the
/// data bytes of another message. [`push`](Self::push) folds one byte in at a
/// time and returns a complete `(status, d1, d2)` channel message when one lands.
pub struct MidiParser {
    /// The current (possibly running) status byte, or 0 if none is established.
    status: u8,
    /// Data bytes collected for the in-progress message.
    data: [u8; 2],
    /// How many data bytes we've collected so far.
    have: usize,
}

impl Default for MidiParser {
    fn default() -> Self {
        MidiParser { status: 0, data: [0; 2], have: 0 }
    }
}

impl MidiParser {
    pub fn new() -> Self {
        Self::default()
    }

    /// Feed one byte. Returns `Some((status, d1, d2))` once a channel-voice
    /// message completes (`d2` is 0 for one-data messages like Program Change).
    pub fn push(&mut self, byte: u8) -> Option<(u8, u8, u8)> {
        match byte {
            // System real-time (0xF8..=0xFF): a single byte that may interleave
            // with other messages. Pass it by without disturbing running status.
            0xF8..=0xFF => None,
            // System common (0xF0..=0xF7), incl. SysEx start/end: not handled;
            // just cancel running status so stray data bytes aren't misread.
            0xF0..=0xF7 => {
                self.status = 0;
                self.have = 0;
                None
            }
            // A channel-voice status byte (0x80..=0xEF): start a new message.
            0x80..=0xEF => {
                self.status = byte;
                self.have = 0;
                None
            }
            // A data byte (0x00..=0x7F).
            _ => {
                if self.status == 0 {
                    return None; // data with no status — ignore
                }
                self.data[self.have] = byte;
                self.have += 1;
                if self.have < data_bytes_for(self.status) {
                    return None; // more data still to come
                }
                // Message complete. Keep `status` set so the next data byte(s)
                // reuse it (running status); reset the data cursor.
                self.have = 0;
                let d1 = self.data[0];
                // One-data messages leave d2 as 0.
                let d2 = if data_bytes_for(self.status) == 2 { self.data[1] } else { 0 };
                Some((self.status, d1, d2))
            }
        }
    }
}

/// Number of data bytes a channel-voice status carries: Program Change (0xC0)
/// and Channel Pressure (0xD0) take one; everything else takes two.
fn data_bytes_for(status: u8) -> usize {
    match status & 0xF0 {
        0xC0 | 0xD0 => 1,
        _ => 2,
    }
}

/// Open the audio device + MIDI input, arm the song, and let the controller
/// drive the transport until Ctrl-C. Linux only (ALSA).
#[cfg(target_os = "linux")]
pub fn run(bundle: &std::path::Path, song: Option<&str>, verbose: bool) -> Result<(), String> {
    use std::io::Read;
    use std::sync::atomic::AtomicBool;
    use std::time::{Duration, Instant};

    use alsa::rawmidi::Rawmidi;
    use alsa::Direction;

    use turtle_core::Command;

    use crate::alsa_backend::{AlsaAudio, AlsaMidi};
    use crate::backend::MidiSink;
    use crate::clock::TransportClock;
    use crate::engine::{rt_channel, Engine, RtCommand};
    use crate::play::{dispatch_pos, load_playable, load_schedulers, Playable};
    use crate::rt;

    // Reuse the play path's loader: preload the chosen (or first) song's stems.
    let Playable { show, mut mixer, song_dir, .. } = load_playable(bundle, song)?;
    let rate = show.show.playback_rate;

    let audio = AlsaAudio::open(&show.audio.device, rate, show.audio.buffer_frames as usize)
        .map_err(|e| format!("open audio '{}': {e}", show.audio.device))?;
    // Non-blocking rawmidi input (the `true`): the one control loop polls input
    // *and* dispatches timed MIDI output, so a blocking read would starve the
    // scheduler. `input_port` must be a real ALSA name (`amidi -l`).
    let midi_in = Rawmidi::new(&show.control.input_port, Direction::Capture, true)
        .map_err(|e| format!("open midi in '{}': {e}", show.control.input_port))?;

    // MIDI output for the scheduler (best-effort, like the play path).
    let midi_names: Vec<String> = show.destinations.iter().map(|d| d.port.clone()).collect();
    let (mut midi_out, failed) = AlsaMidi::open(&midi_names);
    for name in &failed {
        eprintln!("warning: MIDI out '{name}' unavailable; its events will be logged only");
    }
    let mut schedulers = load_schedulers(&show, &song_dir, rate);
    let dest_offsets: Vec<f64> = show
        .destinations
        .iter()
        .map(|d| show.audio.output_latency_ms + d.offset_ms)
        .collect();

    let clock = TransportClock::new(rate);
    let (mut tx, mut rx) = rt_channel(64);
    let running = AtomicBool::new(true);
    let epoch = Instant::now();

    // The transport engine. It shares `midi_out` with the scheduler: on Stop it
    // emits clean-release note-offs, on double-Stop/Panic all-notes-off (§5/§8).
    let mut eng = Engine::new(&show);
    // The song is already preloaded, so arm it up front: Select its setlist PC,
    // then feed the loader's "Loaded" so the state machine reaches ARMED.
    let pc = show.setlist.first().map(|e| e.pc).ok_or("empty setlist")?;
    // Arming emits no MIDI, but `handle` needs a sink; pass the shared one.
    eng.handle(Command::Select(pc), &mut midi_out);
    eng.handle(Command::Loaded, &mut midi_out);

    println!(
        "armed \"{}\" on {}; drive it from {} (Ctrl-C to quit)",
        show.show.name, show.audio.device, show.control.input_port
    );

    std::thread::scope(|s| {
        // Same ownership split as the play path: move the !Sync audio + mixer +
        // rx into the audio thread; share atomic-backed clock/running by ref.
        let clock = &clock;
        let running = &running;
        s.spawn(move || rt::run_audio(&audio, &mut mixer, clock, &mut rx, epoch, running));

        // The control + MIDI-scheduler loop (one thread for v1). Each ~1 ms it:
        //  1. polls MIDI input (non-blocking) -> transport commands -> RT queue;
        //  2. dispatches due MIDI output while playing.
        let mut parser = MidiParser::new();
        let mut io = midi_in.io();
        let mut buf = [0u8; 64];
        // Track play state locally so the scheduler only fires while running —
        // when stopped, the interpolated clock would otherwise drift and emit.
        let mut playing = false;

        loop {
            // Non-blocking read: `Ok(n)` gives pending bytes; an `Err` means no
            // data right now (WouldBlock/EAGAIN) — or a transient device hiccup —
            // so we simply keep polling. Ctrl-C is the exit.
            if let Ok(n) = io.read(&mut buf) {
                for &byte in &buf[..n] {
                    let Some((status, d1, d2)) = parser.push(byte) else { continue };
                    // The engine may emit clean-release/panic MIDI to `midi_out`.
                    for cmd in eng.handle_midi(status, d1, d2, &mut midi_out) {
                        match cmd {
                            RtCommand::Start => {
                                playing = true;
                                if verbose {
                                    println!("[start] wall={:.3}s", epoch.elapsed().as_secs_f64());
                                }
                            }
                            RtCommand::Stop => {
                                playing = false;
                                if verbose {
                                    println!("[stop] wall={:.3}s", epoch.elapsed().as_secs_f64());
                                }
                            }
                            // Rewind: realign the output cursors with the audio.
                            RtCommand::Seek(pos) => {
                                for sched in schedulers.iter_mut() {
                                    sched.seek(pos);
                                }
                            }
                            RtCommand::ToggleMute(pair) => {
                                if verbose {
                                    println!(
                                        "[mute] pair {pair} wall={:.3}s",
                                        epoch.elapsed().as_secs_f64()
                                    );
                                }
                            }
                            RtCommand::SetDsp(pair, param, value) => {
                                if verbose {
                                    println!(
                                        "[dsp] pair {pair} {param:?}={value} wall={:.3}s",
                                        epoch.elapsed().as_secs_f64()
                                    );
                                }
                            }
                        }
                        let _ = tx.push(cmd);
                    }
                }
            }

            if playing {
                let wall_s = epoch.elapsed().as_secs_f64();
                let pos = clock.interpolate(epoch.elapsed().as_nanos() as u64);
                for (port, sched) in schedulers.iter_mut().enumerate() {
                    // `None` = within the offset of the start; nothing due yet.
                    let Some(pos_adj) = dispatch_pos(pos, dest_offsets[port], rate) else { continue };
                    for ev in sched.drain_due(pos_adj) {
                        let bytes = ev.message.as_bytes();
                        midi_out.send(port, bytes);
                        // Track sounding notes so a later Stop cleanly releases them.
                        eng.observe_output(port, bytes);
                        if verbose {
                            println!(
                                "  midi transport={:.3}s wall={wall_s:.3}s port{port} {bytes:02X?}",
                                pos_adj as f64 / rate as f64
                            );
                        }
                    }
                }
            }

            std::thread::sleep(Duration::from_millis(1));
        }
        // The loop runs until the process is signalled (Ctrl-C); there is no
        // clean-exit path yet, so the audio thread's `running` flag stays set.
    });

    #[allow(unreachable_code)]
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Feed a whole byte slice, collecting every completed message.
    fn parse_all(bytes: &[u8]) -> Vec<(u8, u8, u8)> {
        let mut p = MidiParser::new();
        bytes.iter().filter_map(|&b| p.push(b)).collect()
    }

    #[test]
    fn parses_note_on() {
        assert_eq!(parse_all(&[0x90, 60, 100]), vec![(0x90, 60, 100)]);
    }

    #[test]
    fn program_change_has_one_data_byte() {
        // 0xC0 5 completes immediately; d2 is 0.
        assert_eq!(parse_all(&[0xC0, 5]), vec![(0xC0, 5, 0)]);
    }

    #[test]
    fn running_status_reuses_last_status() {
        // One 0x90, then two note pairs: both decode as note-on.
        assert_eq!(
            parse_all(&[0x90, 60, 100, 62, 80]),
            vec![(0x90, 60, 100), (0x90, 62, 80)]
        );
    }

    #[test]
    fn realtime_byte_interleaved_is_ignored() {
        // A 0xF8 clock byte lands between the note and velocity; the note still
        // parses correctly and the clock is dropped.
        assert_eq!(parse_all(&[0x90, 60, 0xF8, 100]), vec![(0x90, 60, 100)]);
    }

    #[test]
    fn data_byte_without_status_is_ignored() {
        assert_eq!(parse_all(&[100, 100]), vec![]);
    }

    #[test]
    fn system_common_cancels_running_status() {
        // After a SysEx-ish 0xF0/0xF7, a lone data byte must not be misread.
        assert_eq!(parse_all(&[0x90, 60, 100, 0xF7, 62]), vec![(0x90, 60, 100)]);
    }
}
