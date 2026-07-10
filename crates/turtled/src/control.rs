//! `turtled control <bundle> [song]` — drive playback from a live MIDI device.
//!
//! Any MIDI controller on the show's `[control] input_port` works: Program
//! Change selects a song, the mapped notes trigger start/stop/next/prev/panic
//! (spec §8). The decode + transport logic already lives in [`crate::control_map`]
//! and [`crate::engine`] (both unit-tested); this module adds the two missing
//! pieces:
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
pub fn run(bundle: &std::path::Path, song: Option<&str>) -> Result<(), String> {
    use std::io::Read;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::Instant;

    use alsa::rawmidi::Rawmidi;
    use alsa::Direction;

    use turtle_core::Command;

    use crate::alsa_backend::AlsaAudio;
    use crate::backend::NullMidi;
    use crate::clock::TransportClock;
    use crate::engine::{rt_channel, Engine};
    use crate::play::{load_playable, Playable};
    use crate::rt;

    // Reuse the play path's loader: preload the chosen (or first) song's stems.
    let Playable { show, mut mixer, .. } = load_playable(bundle, song)?;
    let rate = show.show.playback_rate;

    let audio = AlsaAudio::open(&show.audio.device, rate, show.audio.buffer_frames as usize)
        .map_err(|e| format!("open audio '{}': {e}", show.audio.device))?;
    // Blocking rawmidi input. NOTE: `input_port` must be a real ALSA name (e.g.
    // `hw:1,0,0` from `amidi -l`); resolving logical labels like "CME:in" is a
    // later step — set it in show.toml for now, as with the audio device.
    let midi_in = Rawmidi::new(&show.control.input_port, Direction::Capture, false)
        .map_err(|e| format!("open midi in '{}': {e}", show.control.input_port))?;

    let clock = TransportClock::new(rate);
    let (mut tx, mut rx) = rt_channel(64);
    let running = AtomicBool::new(true);
    let epoch = Instant::now();

    // The transport engine. NullMidi for now: clean-release / panic MIDI output
    // to destinations is a later step; here we only care about transport control.
    let mut eng = Engine::new(&show, NullMidi);
    // The song is already preloaded, so arm it up front: Select its setlist PC,
    // then feed the loader's "Loaded" so the state machine reaches ARMED.
    let pc = show.setlist.first().map(|e| e.pc).ok_or("empty setlist")?;
    eng.handle(Command::Select(pc));
    eng.handle(Command::Loaded);

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

        // Control loop: read MIDI, decode to transport commands, forward RT
        // commands to the audio thread. Blocking reads; Ctrl-C ends the process.
        let mut parser = MidiParser::new();
        let mut io = midi_in.io();
        let mut buf = [0u8; 64];
        loop {
            match io.read(&mut buf) {
                Ok(0) => break, // input closed
                Ok(n) => {
                    for &byte in &buf[..n] {
                        if let Some((status, d1, d2)) = parser.push(byte) {
                            for cmd in eng.handle_midi(status, d1, d2) {
                                let _ = tx.push(cmd);
                            }
                        }
                    }
                }
                Err(_) => break, // device error — stop cleanly
            }
        }
        // Only reached on read error/EOF (Ctrl-C kills the process directly).
        running.store(false, Ordering::Release);
    });

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
