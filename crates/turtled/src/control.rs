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
    /// Song length. Carried alongside the mixer because song length is
    /// per-song: after a switch, `turtle status` would otherwise keep
    /// reporting the *previous* song's duration.
    frames: u64,
    /// Whether the incoming song loops (§14) — same reason as `frames`: it is a
    /// property of the song, so it has to travel with it across a switch.
    ///
    /// Now the **active section's** flag (§4.1), snapshotted when the song is armed.
    /// That is exact while playback stays on one section; live section switching
    /// will have to refresh it, since sections may differ.
    looping: bool,
    /// How many sections the song has (§4.1); `1` for a song without `[[sections]]`.
    sections: usize,
    /// The section that will play — the first one, until switching exists.
    first_section: String,
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
        looping: p.mixer.is_looping(),
        // Captured before the mixer moves into the struct; only used for the log
        // line, but on hardware "did my sections load?" is otherwise invisible.
        sections: p.mixer.section_count(),
        first_section: p.mixer.section_name().to_string(),
        mixer: p.mixer,
        schedulers,
        frames: p.frames,
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

/// If the engine armed a song, kick off its background load.
///
/// `Select`/`Next`/`Prev` produce no `RtCommand` — only an `Action::Preload`,
/// which surfaces through `take_pending_preload()`. So this must be called
/// after *every* command that could arm a song, from **both** the MIDI and the
/// socket path: miss it and `turtle arm second` would move the transport to
/// `Loading` and sit there forever, because nothing ever loads the song.
///
/// `expected_song` is updated to the newly-armed name, which is what makes a
/// superseded load (a second Select before the first finished) get dropped on
/// arrival rather than misapplied.
///
/// Portable, like [`dispatch_rt`] — no ALSA types — so it type-checks here.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn pump_preload(
    eng: &mut crate::engine::Engine,
    expected_song: &mut Option<String>,
    bundle: &std::path::Path,
    rate: u32,
    loader_tx: &std::sync::mpsc::Sender<(String, Result<LoadedSong, String>)>,
    epoch: std::time::Instant,
    verbose: bool,
) {
    let Some(song) = eng.take_pending_preload() else { return };
    if verbose {
        println!(
            "[preload] \"{song}\" wall={:.3}s",
            epoch.elapsed().as_secs_f64()
        );
    }
    *expected_song = Some(song.clone());
    spawn_load(bundle.to_path_buf(), song, rate, loader_tx.clone());
}

/// Open the audio device + MIDI input, arm the song, start the control socket,
/// and let the controller (or the `turtle` CLI) drive the transport until
/// Ctrl-C. Linux only (ALSA).
///
/// `socket_path` is where the §10 control socket binds. A bind failure is
/// fatal rather than a warning: it means either a stale path we cannot clean
/// up or a second `turtled` already running, and quietly playing a show that
/// the CLI cannot talk to is the worse outcome (§12's "fail loudly").
///
/// `tuning` carries the §3/§12 RT knobs: thread priority (the fused control/MIDI
/// thread derives its own one step below), whether memory is locked, and which
/// CPU the audio thread is pinned to. `--rt-prio 0` disables the lot and runs at
/// normal priority — the pre-§3 behaviour, kept for A/B comparison on the Pi.
#[cfg(target_os = "linux")]
pub fn run(
    bundle: &std::path::Path,
    song: Option<&str>,
    verbose: bool,
    socket_path: &std::path::Path,
    // Fully qualified: this function's `use` statements are inside its body (so
    // the ALSA imports stay out of the non-Linux build), which means the
    // signature cannot see the short `sched` path.
    tuning: crate::sched::Tuning,
    wait_devices: std::time::Duration,
) -> Result<(), String> {
    use std::io::Read;
    use std::sync::atomic::AtomicBool;
    use std::sync::{mpsc, Arc, Mutex};
    use std::time::{Duration, Instant};

    use alsa::rawmidi::Rawmidi;
    use alsa::Direction;

    use turtle_core::proto::{Event, Source, Status};
    use turtle_core::{Command, State};

    use crate::alsa_backend::{AlsaAudio, AlsaMidi};
    use crate::backend::MidiSink;
    use crate::clock::TransportClock;
    use crate::engine::{rt_channel, rt_event_channel, Engine, RtEvent};
    use crate::mixer::song_channel;
    use crate::play::{dispatch_pos, load_playable, load_schedulers, Playable};
    use crate::{notify, retry, rt, sched, socket};

    // Resolve both thread priorities up front, so the ordering invariant
    // (audio > midi) is established in one place rather than at two spawn sites.
    let audio_priority = tuning.rt_priority;
    let midi_priority = tuning.midi_priority();

    // Lock memory before anything large is allocated. Process-wide, so unlike the
    // per-thread priority calls this belongs here rather than at a spawn site;
    // `MCL_FUTURE` means the stems loaded below are covered as they arrive (§12).
    sched::lock_memory_or_warn(tuning.rt_enabled());

    // Report the CPU tuning actually in effect (§12) — the governor and whether
    // isolcpus reserved a core. Read-only: setting these is deployment's job, but
    // logging them is how you find out the tuning did not take.
    println!("[sched] cpu: {}", sched::describe_cpu_tuning());

    // systemd, if it started us. A no-op from a shell, so there is no branch on
    // "are we a service" anywhere below (§12).
    let mut sd = notify::Notifier::from_env();

    // Reuse the play path's loader: preload the chosen (or first) song's stems.
    let Playable {
        show,
        mut mixer,
        song_dir,
        frames,
    } = load_playable(bundle, song)?;
    let rate = show.show.playback_rate;
    // Song length in seconds, for `turtle status`. Reassigned on a song switch,
    // since it is a property of the song, not the show.
    let mut duration_s = frames as f64 / rate as f64;
    // Tracked next to `duration_s` for the same reason: both are per-song, so
    // both must change when a song does, or `turtle status` describes the
    // previous song.
    let mut looping = mixer.is_looping();

    // Wait for the device rather than exiting: at boot USB enumeration races the
    // service, and on a restart the outgoing process may still hold it (§12).
    let audio = retry::open_with_retry(
        &format!("audio device '{}'", show.audio.device),
        wait_devices,
        || AlsaAudio::open(&show.audio.device, rate, show.audio.buffer_frames as usize),
        |msg| {
            println!("{msg}");
            // Also the systemd status line, so a slow start reads as "waiting for
            // the interface" rather than an unexplained delay.
            sd.status(msg);
        },
    )
    .map_err(|e| format!("open audio '{}': {e}", show.audio.device))?;
    // Non-blocking rawmidi input (the `true`): the one control loop polls input
    // *and* dispatches timed MIDI output, so a blocking read would starve the
    // scheduler. Resolved first, so `input_port` may be either a logical label
    // (`"CME:1"`, via the `[ports]` table) or a raw ALSA name (§5/§7.1).
    let input_port = show
        .resolved_input_port()
        .map_err(|e| format!("control.input_port: {e}"))?;
    let midi_in = retry::open_with_retry(
        &format!("MIDI input '{input_port}'"),
        wait_devices,
        || Rawmidi::new(&input_port, Direction::Capture, true),
        |msg| {
            println!("{msg}");
            sd.status(msg);
        },
    )
        // Report both spellings: the label is what you wrote, the address is what
        // failed, and with a logical label those differ.
        .map_err(|e| {
            format!(
                "open midi in '{}' (resolved to '{input_port}'): {e}",
                show.control.input_port
            )
        })?;

    // MIDI output for the scheduler (best-effort, like the play path).
    let midi_names: Vec<String> = show
        .resolved_destination_ports()
        .map_err(|e| format!("destination port: {e}"))?;
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

    // The song currently armed/playing, and the one held for a gapless
    // advance — reported by `turtle status`, and kept in step with the
    // transport's own notion of them as songs are switched below.
    let mut current_song = Some(song_name.clone());
    let mut armed_next_song: Option<String> = None;

    // Start the control socket only once the transport is armed, so the very
    // first `turtle status` a client can possibly see is already truthful.
    // `status_handle`, not `status`: the MIDI byte loop below binds `status` to
    // an incoming status byte, and shadowing this behind it would be a trap.
    let status_handle: socket::StatusHandle = Arc::new(Mutex::new(Status {
        show: show.show.name.clone(),
        // Canonicalised so a client comparing paths is not defeated by `..`, a
        // symlink, or a relative argument. Falls back to the path as given if it
        // cannot be resolved — a best-effort field must never fail a startup.
        bundle: Some(
            bundle
                .canonicalize()
                .unwrap_or_else(|_| bundle.to_path_buf())
                .display()
                .to_string(),
        ),
        state: eng.state(),
        song: current_song.clone(),
        armed_next: None,
        position_s: 0.0,
        duration_s,
        looping: mixer.is_looping(),
    }));
    let server = socket::start(
        socket_path,
        Arc::clone(&status_handle),
        show.setlist.clone(),
    )
    .map_err(|e| format!("control socket {}: {e}", socket_path.display()))?;

    println!(
        "armed \"{}\" on {}; drive it from {} or `turtle` on {} (Ctrl-C to quit)",
        show.show.name,
        show.audio.device,
        input_port,
        socket_path.display()
    );

    // Only *now* is `READY=1` honest: stems loaded, audio device open, socket
    // bound, transport armed. Sending it from `main` would let systemd release
    // anything ordered after us while we were still loading, and would turn a
    // start-up failure into "started, then crashed" (§12).
    sd.ready(&format!("armed \"{}\"", show.show.name));
    println!("[systemd] {}", notify::describe(&sd));

    // The closure's value is this function's result: a failed audio device has to
    // propagate out as an error so `main` exits non-zero and systemd restarts us.
    let outcome: Result<(), String> = std::thread::scope(|s| {
        // Same ownership split as the play path: move the !Sync audio + mixer +
        // rx into the audio thread; share atomic-backed clock/running by ref.
        let clock = &clock;
        let running = &running;
        s.spawn(move || {
            // First thing on the new thread, before a single period is
            // rendered: ask for SCHED_FIFO (§3). Best-effort — a failure warns
            // and plays on at normal priority (see `crate::sched`).
            sched::apply_or_warn("audio", audio_priority);
            // Pin after the priority is set, so if pinning fails we at least
            // keep the priority we just got (§12).
            sched::pin_or_warn("audio", tuning.audio_cpu);
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

        // This thread *is* the MIDI scheduler in v1, so it takes RT priority
        // too — one step below audio, so an audio period always wins (§3).
        // Everything below this point runs at that priority, which is why the
        // status snapshot is republished with `try_lock` rather than `lock`.
        sched::apply_or_warn("control+midi", midi_priority);

        // The control + MIDI-scheduler loop (one thread for v1). Each ~1 ms it:
        //  1. polls MIDI input (non-blocking) -> transport commands -> RT queue;
        //  2. polls the control socket for CLI-injected commands (§10);
        //  3. polls the background loader for a finished (or failed) preload;
        //  4. polls the RT thread for an EndReached event;
        //  5. dispatches due MIDI output while playing;
        //  6. republishes the status snapshot the socket serves.
        let mut parser = MidiParser::new();
        let mut io = midi_in.io();
        let mut buf = [0u8; 64];
        // What we believe the RT thread is doing (play state + last seek).
        let mut view = RtView::default();
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
                    let rt_cmds = eng.handle_midi(status, d1, d2, &mut midi_out);
                    // Report the raw message and what it meant, before acting on
                    // it. Gated on `monitored()` because formatting this costs
                    // allocations on a 1 ms loop, and nobody is watching during
                    // an actual show. `take_last_decoded` must follow the
                    // `handle_midi` it belongs to.
                    if server.monitored() {
                        let decoded = eng.take_last_decoded();
                        let decoded = (!decoded.is_empty()).then(|| {
                            decoded
                                .iter()
                                .map(|d| d.to_string())
                                .collect::<Vec<_>>()
                                .join(", ")
                        });
                        // Report the real message length: Program Change carries
                        // one data byte, and printing a phantom third would be a
                        // lie in the one tool meant to explain the wire.
                        let bytes = if data_bytes_for(status) == 1 {
                            vec![status, d1]
                        } else {
                            vec![status, d1, d2]
                        };
                        server.publish(Event::Midi {
                            wall_s: epoch.elapsed().as_secs_f64(),
                            bytes,
                            decoded,
                        });
                    }
                    for cmd in rt_cmds {
                        dispatch_rt(cmd, &mut view, &mut schedulers, &mut tx, epoch, verbose);
                    }
                    // Select/Next/Prev arm a song without emitting any
                    // RtCommand (only `Action::Preload`), so this has to be
                    // checked unconditionally after every decoded message,
                    // not just when the loop above actually ran.
                    pump_preload(
                        &mut eng,
                        &mut expected_song,
                        &bundle_owned,
                        rate,
                        &loader_tx,
                        epoch,
                        verbose,
                    );
                }
            }

            // Commands injected over the control socket (§10). `try_recv` never
            // blocks, so a CLI client can never stall the scheduler. These take
            // exactly the same path as a MIDI-decoded command — including the
            // preload pump, without which `turtle arm` would sit in `Loading`
            // forever.
            while let Ok(cmd) = server.commands.try_recv() {
                let rt_cmds = eng.handle(cmd, &mut midi_out);
                if server.monitored() {
                    server.publish(Event::Command {
                        wall_s: epoch.elapsed().as_secs_f64(),
                        source: Source::Socket,
                        command: format!("{cmd:?}").to_lowercase(),
                        state: eng.state(),
                    });
                }
                for c in rt_cmds {
                    dispatch_rt(c, &mut view, &mut schedulers, &mut tx, epoch, verbose);
                }
                pump_preload(
                    &mut eng,
                    &mut expected_song,
                    &bundle_owned,
                    rate,
                    &loader_tx,
                    epoch,
                    verbose,
                );
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
                            // Read the length before `mixer`/`schedulers` are
                            // moved out of `loaded` below.
                            duration_s = loaded.frames as f64 / rate as f64;
                            looping = loaded.looping;
                            let sections = loaded.sections;
                            let first_section = loaded.first_section.clone();
                            current_song = Some(song.clone());
                            let _ = song_tx.push(loaded.mixer);
                            schedulers = loaded.schedulers;
                            if verbose {
                                println!(
                                    "[armed] \"{song}\" wall={:.3}s",
                                    epoch.elapsed().as_secs_f64()
                                );
                            }
                            // Only for a genuinely sectioned song: every other song
                            // is one implicit section, and saying so would be noise.
                            if sections > 1 {
                                println!(
                                    "[sections] {sections} loaded; starting at \"{first_section}\""
                                );
                            }
                        } else if was_playing {
                            // Held for the gapless advance: the *current* song
                            // is still the one playing, so only `armed_next`
                            // (and not `duration_s`) changes here.
                            armed_next_song = Some(song.clone());
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
                    // Recovery inside the RT thread already failed, so the device
                    // is gone rather than late. Exit deliberately: with no audio
                    // there is no show, and staying up would leave systemd, the
                    // watchdog and `turtle status` all reporting health (the
                    // watchdog ping comes from *this* loop, which is still fine).
                    // Restarting is the recovery — since PR #29 the daemon waits for
                    // the device to come back, so a yanked USB cable now costs a few
                    // seconds of silence instead of the rest of the set.
                    RtEvent::AudioFailed { errno } => {
                        let err = std::io::Error::from_raw_os_error(errno);
                        eprintln!(
                            "error: audio device '{}' failed and could not be recovered: {err}",
                            show.audio.device
                        );
                        eprintln!(
                            "       there is no audio from this point, so exiting rather than \
                             appearing healthy; systemd will restart and wait for the device."
                        );
                        // A deliberate exit, so systemd does not read it as a crash.
                        sd.stopping("audio device failed — restarting");
                        // The audio thread has already stopped, but be explicit:
                        // leaving the scope joins it, and a set flag would hang.
                        running.store(false, std::sync::atomic::Ordering::Release);
                        return Err(format!(
                            "audio device '{}' failed: {err}",
                            show.audio.device
                        ));
                    }
                    // A looping song wrapped: rewind every destination's cursor so
                    // the next pass dispatches its events again (§14). Reseeking to
                    // 0 rather than to the interpolated position deliberately —
                    // events between 0 and now fire immediately, which at ~1 ms of
                    // slack is inaudible, whereas skipping them would silently drop
                    // a cue at the top of the loop.
                    RtEvent::Looped => {
                        for sched in schedulers.iter_mut() {
                            sched.seek(0);
                        }
                        // Anything sounding from the previous pass is released, so
                        // a note held across the seam does not hang.
                        if verbose {
                            println!("[loop] wrapped; MIDI cursors rewound");
                        }
                    }
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
                                duration_s = held.frames as f64 / rate as f64;
                                looping = held.looping;
                                // The song armed next has just become current.
                                current_song = armed_next_song.take();
                                let _ = song_tx.push(held.mixer);
                                schedulers = held.schedulers;
                            }
                        }
                        if server.monitored() {
                            server.publish(Event::Command {
                                wall_s: epoch.elapsed().as_secs_f64(),
                                source: Source::Internal,
                                command: "endreached".into(),
                                state: eng.state(),
                            });
                        }
                        for cmd in cmds {
                            dispatch_rt(cmd, &mut view, &mut schedulers, &mut tx, epoch, verbose);
                        }
                    }
                }
            }

            // While playing, the interpolated clock is the live position; once
            // stopped it would drift, so the last seek we dispatched stands in.
            let position = if view.playing {
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
                pos
            } else {
                view.position
            };

            // Republish what `turtle status` reads (§10). This is the only
            // place that writes the snapshot.
            //
            // `try_lock`, not `lock`: this thread runs at SCHED_FIFO (§3, see
            // `crate::sched`) while the socket threads that *read* the snapshot
            // are normal priority. A blocking `lock()` here would be a textbook
            // priority inversion — an RT thread waiting on a lower-priority
            // thread that the scheduler may not run promptly, stalling MIDI
            // dispatch for as long as it takes. Skipping the update instead is
            // free: the snapshot is "latest value wins", and the next iteration
            // is ~1 ms away, so a contended tick costs a client nothing
            // observable.
            if let Some(mut snap) = socket::try_lock_status(&status_handle) {
                snap.state = eng.state();
                snap.position_s = position as f64 / rate as f64;
                snap.duration_s = duration_s;
                snap.looping = looping;
                // Only clone the names when they actually change — this runs
                // ~1000x a second, and in the steady state they don't.
                if snap.song.as_deref() != current_song.as_deref() {
                    snap.song = current_song.clone();
                }
                if snap.armed_next.as_deref() != armed_next_song.as_deref() {
                    snap.armed_next = armed_next_song.clone();
                }
            }

            // Tell systemd the loop is still turning (§12). Deliberately from
            // *this* loop and not a timer thread: a timer would keep pinging
            // while the loop that actually dispatches MIDI was wedged, proving
            // only that the timer lives. Cheap enough for a 1 ms tick — with no
            // watchdog configured it is one `Instant` comparison.
            sd.watchdog_tick();

            std::thread::sleep(Duration::from_millis(1));
        }
        // Otherwise the loop runs until the process is signalled (Ctrl-C).
    });

    outcome
}

/// The control thread's view of what the audio RT thread is doing, kept in
/// sync by [`dispatch_rt`] as commands are forwarded.
///
/// Not authoritative — the audio thread owns the real transport — but it is
/// what gates the MIDI scheduler and what `turtle status` reports.
#[derive(Debug, Default, Clone, Copy)]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
struct RtView {
    /// Whether the transport is running. The scheduler only dispatches while
    /// it is: stopped, the interpolated clock would drift and emit.
    playing: bool,
    /// The last position we *told* the RT thread to be at, via `Seek`.
    ///
    /// While playing, the interpolated clock is the better answer and this is
    /// stale — but once stopped the clock keeps drifting, so this is what
    /// `status` must report. `Stop` with `rewind_on_stop` emits `Seek(0)` right
    /// behind it, so tracking seeks is what makes a stopped position honest.
    position: u64,
}

/// Apply one `RtCommand`'s control-thread side effects (verbose logging,
/// local [`RtView`]/scheduler-cursor bookkeeping) and forward it to the RT
/// audio thread. A plain function rather than a closure over `run`'s locals:
/// it's called from three places (the MIDI-driven path, the socket-driven
/// path, and the EndReached-driven gapless-advance path), and a long-lived
/// closure capturing `schedulers` by `&mut` would conflict with the plain
/// reassignments (`schedulers = loaded.schedulers`) elsewhere in the loop.
/// Portable (no ALSA types) so it's type-checked on the dev Mac even though
/// its only caller, `run`, is `cfg(linux)`.
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn dispatch_rt(
    cmd: crate::engine::RtCommand,
    view: &mut RtView,
    schedulers: &mut [crate::scheduler::PortScheduler],
    tx: &mut crate::engine::RtProducer,
    epoch: std::time::Instant,
    verbose: bool,
) {
    use crate::engine::RtCommand;

    match cmd {
        RtCommand::Start => {
            view.playing = true;
            if verbose {
                println!("[start] wall={:.3}s", epoch.elapsed().as_secs_f64());
            }
        }
        RtCommand::Stop => {
            view.playing = false;
            if verbose {
                println!("[stop] wall={:.3}s", epoch.elapsed().as_secs_f64());
            }
        }
        // Nothing for the control thread to track: the RT thread just empties the
        // delay. Logged because "why did my tail stop?" is worth being able to
        // answer from a -v capture.
        RtCommand::ClearDelay => {
            if verbose {
                println!("[panic] delay cleared wall={:.3}s", epoch.elapsed().as_secs_f64());
            }
        }
        // Rewind: realign the output cursors with the audio.
        RtCommand::Seek(pos) => {
            view.position = pos;
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
             [ports]\nCME = \"H4MIDIWC\"\n\n[[destinations]]\nname = \"lights\"\nport = \"CME:1\"\n\
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
