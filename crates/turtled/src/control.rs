//! `turtled control <bundle> [song]` — drive playback from a live MIDI device.
//!
//! Any MIDI controller on the show's `[control] input_port` works: Program
//! Change selects a song, the mapped notes trigger start/stop/next/prev/panic
//! (spec §8), and the mute notes / `dsp_*` CCs act directly on the mixer
//! regardless of transport state. The decode + transport logic already lives
//! in [`crate::control_map`] and [`crate::engine`] (both unit-tested); this
//! module adds the pieces `turtle-core::transport` doesn't own:
//!
//!   * [`MidiParser`] — a **portable** MIDI byte-stream parser (running status,
//!     interleaved real-time bytes). Unit-tested on the dev Mac.
//!   * [`load_song_payload`]/[`spawn_load`] — the **portable** background
//!     loader (§3/§8): a `Command::Select`/`Next`/`Prev` armed mid-song
//!     doesn't block playback — it's loaded off-thread, and installed either
//!     immediately (re-arming while stopped) or held until a gapless
//!     `EndReached` auto-advance (armed *during* playback).
//!   * [`run`] — the **`cfg(linux)`** loop that reads ALSA rawmidi, polls the
//!     loader and the RT thread's `EndReached` events, and forwards the
//!     resulting RT commands to the audio thread. Verified on the Pi.

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

/// A song loaded off the RT/control threads (real file I/O + WAV decoding),
/// ready to install: swap `mixer` into the audio thread and replace
/// `schedulers` in the control thread (§3/§8).
struct LoadedSong {
    mixer: crate::mixer::Mixer,
    schedulers: Vec<crate::scheduler::PortScheduler>,
}

/// The portable half of a background load: reuses the same loader the
/// startup path and `turtled play` use. Kept separate from [`spawn_load`] so
/// the `Result` plumbing (this can fail: missing song dir, bad stem, …) is
/// testable without threads.
fn load_song_payload(
    bundle: &std::path::Path,
    song: &str,
    rate: u32,
) -> Result<LoadedSong, String> {
    let p = crate::play::load_playable(bundle, Some(song))?;
    let schedulers = crate::play::load_schedulers(&p.show, &p.song_dir, rate);
    Ok(LoadedSong {
        mixer: p.mixer,
        schedulers,
    })
}

/// Spawn a background thread to load `song` and send the result back over
/// `tx`, tagged with the song name so the receiver can tell a stale result
/// (superseded by a newer Select/Next/Prev before this one finished) from a
/// current one.
fn spawn_load(
    bundle: std::path::PathBuf,
    song: String,
    rate: u32,
    tx: std::sync::mpsc::Sender<(String, Result<LoadedSong, String>)>,
) {
    std::thread::spawn(move || {
        let result = load_song_payload(&bundle, &song, rate);
        let _ = tx.send((song, result));
    });
}

/// Open the audio device + MIDI input, arm the song, and let the controller
/// drive the transport until Ctrl-C. Linux only (ALSA).
#[cfg(target_os = "linux")]
pub fn run(bundle: &std::path::Path, song: Option<&str>, verbose: bool) -> Result<(), String> {
    use std::io::Read;
    use std::sync::atomic::AtomicBool;
    use std::sync::mpsc;
    use std::time::{Duration, Instant};

    use alsa::rawmidi::Rawmidi;
    use alsa::Direction;

    use turtle_core::{Command, State};

    use crate::alsa_backend::{AlsaAudio, AlsaMidi};
    use crate::backend::MidiSink;
    use crate::clock::TransportClock;
    use crate::engine::{rt_channel, rt_event_channel, Engine, RtEvent};
    use crate::mixer::song_channel;
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
    let (mut song_tx, mut song_rx) = song_channel(2);
    let (mut events_tx, mut events_rx) = rt_event_channel(8);
    let (loader_tx, loader_rx) = mpsc::channel::<(String, Result<LoadedSong, String>)>();
    let running = AtomicBool::new(true);
    let epoch = Instant::now();
    let bundle_owned = bundle.to_path_buf();

    // The transport engine. It shares `midi_out` with the scheduler: on Stop it
    // emits clean-release note-offs, on double-Stop/Panic all-notes-off (§5/§8).
    let mut eng = Engine::new(&show);
    // The song is already preloaded above, so arm it up front through the
    // *real* Select/Loaded flow (not a synthetic shortcut) — that keeps the
    // transport's `current` index in sync with whatever `load_playable`
    // actually picked (the caller's `--song` override, or the first setlist
    // entry). Deriving `pc` from the loaded song's directory name, rather
    // than always assuming the setlist's first entry, matters once `--song`
    // can pick something else.
    let song_name = song_dir
        .file_name()
        .and_then(|n| n.to_str())
        .ok_or("bad song directory name")?
        .to_string();
    let pc = show
        .setlist
        .iter()
        .find(|e| e.song == song_name)
        .map(|e| e.pc)
        .ok_or_else(|| format!("song '{song_name}' not in the setlist"))?;
    // Arming emits no MIDI, but `handle` needs a sink; pass the shared one.
    eng.handle(Command::Select(pc), &mut midi_out);
    // We already have this song's data (loaded synchronously above), so
    // there's nothing for a background thread to do — just drop the
    // preload request `Select` queued and go straight to Loaded.
    let _ = eng.take_pending_preload();
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
        s.spawn(move || {
            rt::run_audio(
                &audio,
                &mut mixer,
                clock,
                &mut rx,
                &mut song_rx,
                &mut events_tx,
                epoch,
                running,
            )
        });

        // The control + MIDI-scheduler loop (one thread for v1). Each ~1 ms it:
        //  1. polls MIDI input (non-blocking) -> transport commands -> RT queue;
        //  2. polls the background loader for a finished (or failed) preload;
        //  3. polls the RT thread for an EndReached event;
        //  4. dispatches due MIDI output while playing.
        let mut parser = MidiParser::new();
        let mut io = midi_in.io();
        let mut buf = [0u8; 64];
        // Track play state locally so the scheduler only fires while running —
        // when stopped, the interpolated clock would otherwise drift and emit.
        let mut playing = false;
        // Which song name the next accepted loader result must match — a
        // fresh Select/Next/Prev before the previous one finished loading
        // supersedes it; the stale result is dropped when it arrives (§8).
        let mut expected_song: Option<String> = None;
        // A song armed *during* playback (`armed_next`), loaded and waiting
        // for the gapless auto-advance at EndReached to actually swap it in.
        let mut held_next: Option<LoadedSong> = None;

        loop {
            // Non-blocking read: `Ok(n)` gives pending bytes; an `Err` means no
            // data right now (WouldBlock/EAGAIN) — or a transient device hiccup —
            // so we simply keep polling. Ctrl-C is the exit.
            if let Ok(n) = io.read(&mut buf) {
                for &byte in &buf[..n] {
                    let Some((status, d1, d2)) = parser.push(byte) else { continue };
                    // The engine may emit clean-release/panic MIDI to `midi_out`.
                    for cmd in eng.handle_midi(status, d1, d2, &mut midi_out) {
                        dispatch_rt(cmd, &mut playing, &mut schedulers, &mut tx, epoch, verbose);
                    }
                    // Select/Next/Prev arm a song without emitting any
                    // RtCommand (only `Action::Preload`), so this has to be
                    // checked unconditionally after every decoded message,
                    // not just when the loop above actually ran.
                    if let Some(song) = eng.take_pending_preload() {
                        if verbose {
                            println!(
                                "[preload] \"{song}\" wall={:.3}s",
                                epoch.elapsed().as_secs_f64()
                            );
                        }
                        expected_song = Some(song.clone());
                        spawn_load(bundle_owned.clone(), song, rate, loader_tx.clone());
                    }
                }
            }

            // A finished (or failed) background load. `try_recv` never blocks.
            while let Ok((song, result)) = loader_rx.try_recv() {
                if expected_song.as_deref() != Some(song.as_str()) {
                    // Superseded by a later Select/Next/Prev; drop it.
                    continue;
                }
                match result {
                    Err(e) => {
                        // Per spec decision: log and stay armed-Loading; the
                        // performer retries with another Select rather than
                        // the transport growing a dedicated failure state.
                        eprintln!("warning: failed to load '{song}': {e}");
                    }
                    Ok(loaded) => {
                        // `Command::Loaded`'s effect depends on the state
                        // *before* applying it (Loading -> Armed installs
                        // now; Playing just marks armed-next ready) — so
                        // read the state first, per `turtle_core::transport`.
                        let was_loading = eng.state() == State::Loading;
                        let was_playing = eng.state() == State::Playing;
                        eng.handle(Command::Loaded, &mut midi_out); // always emits no RtCommand
                        if was_loading {
                            let _ = song_tx.push(loaded.mixer);
                            schedulers = loaded.schedulers;
                            if verbose {
                                println!(
                                    "[armed] \"{song}\" wall={:.3}s",
                                    epoch.elapsed().as_secs_f64()
                                );
                            }
                        } else if was_playing {
                            held_next = Some(loaded);
                            if verbose {
                                println!(
                                    "[armed next] \"{song}\" wall={:.3}s",
                                    epoch.elapsed().as_secs_f64()
                                );
                            }
                        }
                        // Any other state (Stopped/Ended/Armed/Idle): the
                        // transport itself treats Loaded as a no-op there
                        // too (see `Transport::loaded`), so this result is
                        // simply dropped rather than held indefinitely.
                    }
                }
            }

            // The RT thread reached the end of the current song.
            while let Ok(event) = events_rx.pop() {
                match event {
                    RtEvent::EndReached => {
                        let was_playing = eng.state() == State::Playing;
                        let cmds = eng.handle(Command::EndReached, &mut midi_out);
                        let now_playing = eng.state() == State::Playing;
                        // Still Playing on both sides of the call = the
                        // gapless-auto-advance branch fired (the only way
                        // EndReached doesn't move to Ended); install the
                        // held next song before its Seek(0)/Start land.
                        if was_playing && now_playing {
                            if let Some(held) = held_next.take() {
                                let _ = song_tx.push(held.mixer);
                                schedulers = held.schedulers;
                            }
                        }
                        for cmd in cmds {
                            dispatch_rt(
                                cmd,
                                &mut playing,
                                &mut schedulers,
                                &mut tx,
                                epoch,
                                verbose,
                            );
                        }
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

/// Apply one `RtCommand`'s control-thread side effects (verbose logging,
/// local `playing`/scheduler-cursor bookkeeping) and forward it to the RT
/// audio thread. A plain function rather than a closure over `run`'s locals:
/// it's called from two places (the MIDI-driven path and the
/// EndReached-driven gapless-advance path), and a long-lived closure
/// capturing `schedulers` by `&mut` would conflict with the plain
/// reassignments (`schedulers = loaded.schedulers`) elsewhere in the loop.
/// Portable (no ALSA types) so it's type-checked on the dev Mac even though
/// its only caller, `run`, is `cfg(linux)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn dispatch_rt(
    cmd: crate::engine::RtCommand,
    playing: &mut bool,
    schedulers: &mut [crate::scheduler::PortScheduler],
    tx: &mut crate::engine::RtProducer,
    epoch: std::time::Instant,
    verbose: bool,
) {
    use crate::engine::RtCommand;

    match cmd {
        RtCommand::Start => {
            *playing = true;
            if verbose {
                println!("[start] wall={:.3}s", epoch.elapsed().as_secs_f64());
            }
        }
        RtCommand::Stop => {
            *playing = false;
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

    use std::sync::atomic::{AtomicU32, Ordering};

    /// Write a tiny valid bundle (mirrors `play::tests::write_bundle`) so
    /// `load_song_payload` has something real to load.
    fn write_bundle(frames: u32) -> std::path::PathBuf {
        static N: AtomicU32 = AtomicU32::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!("turtle-control-{}-{}", std::process::id(), n));
        let stems = dir.join("songs/opener/stems");
        std::fs::create_dir_all(&stems).unwrap();

        let spec = hound::WavSpec {
            channels: 2,
            sample_rate: 48_000,
            bits_per_sample: 24,
            sample_format: hound::SampleFormat::Int,
        };
        let mut w = hound::WavWriter::create(stems.join("pair1.wav"), spec).unwrap();
        for _ in 0..frames {
            w.write_sample(1000i32).unwrap(); // L
            w.write_sample(1000i32).unwrap(); // R
        }
        w.finalize().unwrap();

        std::fs::write(
            dir.join("show.toml"),
            "[show]\nname = \"B\"\nplayback_rate = 48000\n\
             [audio]\ndevice = \"hw:0\"\n\
             [[destinations]]\nname = \"lights\"\nport = \"CME:1\"\n\
             [control]\ninput_port = \"x\"\nselect_channel = 1\n\
             start = { type = \"note\", note = 60 }\n\
             stop = { type = \"note\", note = 61 }\n\
             next = { type = \"note\", note = 62 }\n\
             prev = { type = \"note\", note = 63 }\n\
             panic = { type = \"note\", note = 65 }\n\
             mute = { type = \"note\", notes = [72, 73, 74, 75] }\n\
             [[setlist]]\npc = 0\nsong = \"opener\"\n",
        )
        .unwrap();
        std::fs::write(
            dir.join("songs/opener/song.toml"),
            format!(
                "[song]\nname = \"O\"\nbpm = 120.0\nlength_samples = {frames}\n\
                 [[pairs]]\nindex = 0\nfile = \"stems/pair1.wav\"\n"
            ),
        )
        .unwrap();
        dir
    }

    #[test]
    fn load_song_payload_loads_stems_and_schedulers() {
        let dir = write_bundle(64);
        let loaded = load_song_payload(&dir, "opener", 48_000).unwrap();
        assert_eq!(loaded.mixer.position(), 0);
        // One destination in this bundle, no MIDI file for it — an empty
        // (not missing) scheduler, matching `load_schedulers`' contract.
        assert_eq!(loaded.schedulers.len(), 1);
        std::fs::remove_dir_all(&dir).ok();
    }

    #[test]
    fn load_song_payload_errors_on_missing_song() {
        let dir = write_bundle(64);
        // `unwrap_err` would require `LoadedSong: Debug` (it isn't, same
        // reason `play::tests` avoids it for `Playable`); match instead.
        let err = match load_song_payload(&dir, "nope", 48_000) {
            Err(e) => e,
            Ok(_) => panic!("expected an error for a missing song"),
        };
        assert!(err.contains("nope"), "error should name the song: {err}");
        std::fs::remove_dir_all(&dir).ok();
    }
}
