//! `turtled` — the Turtle daemon (spec §3).
//!
//! This skeleton contains the platform-independent core of the daemon: the
//! decoupled transport clock (§3.1), the per-port MIDI scheduler (§5), active-
//! note tracking for clean Stop (§8), foot-controller decoding (§8), and the
//! control-thread engine that wires the transport state machine to a lock-free
//! RT command queue (§3) and a MIDI sink.
//!
//! It now also has the offline audio path and live control: the stem loader
//! ([`stems`]), the RT mixer ([`mixer`]), the audio RT loop ([`rt`]) driving the
//! ALSA backend, and MIDI-input transport control ([`control`]).
//! `turtled play <bundle>` plays a song to the device; `turtled control <bundle>`
//! drives its transport from a live MIDI controller. The default
//! `turtled <show.toml>` still just loads + validates.
//!
//! What is **not** here yet: the MIDI scheduler thread (§5, timed output to
//! destinations), clean-release / panic MIDI *output*, the control socket, GPIO,
//! `SCHED_FIFO` thread priorities (v1 uses a normal thread with big xrun-proof
//! buffers, §3.1), and resolving logical MIDI port labels to ALSA device names.

// The RT modules below (clock, scheduler, engine, ...) are unit-tested but not
// yet driven by `main`: their consumer is the ALSA RT loop, which is Linux-only
// and not part of this skeleton. Allow dead code until that loop is written so
// the intentionally-ahead API surface doesn't warn.
#![allow(dead_code)]

mod backend;
// Linux-only concrete backends (ALSA PCM + rawmidi). Compiled on the Pi; on the
// dev Mac this is skipped so the portable core still builds. Not yet driven by
// `main` — the RT loop that consumes it is the next step (hence `dead_code`).
#[cfg(target_os = "linux")]
mod alsa_backend;
mod clock;
mod control;
mod control_map;
mod engine;
mod mixer;
mod notes;
mod play;
mod rt;
mod scheduler;
mod stems;

use std::process::ExitCode;

use backend::{AudioBackend, NullAudio, NullMidi};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        // `play` runs the real audio path (Linux/ALSA). Everything else is
        // treated as a show path and takes the unchanged load+validate path.
        Some("play") => {
            let opts = CmdOpts::parse(args);
            play_command(opts)
        }
        Some("control") => {
            let opts = CmdOpts::parse(args);
            control_command(opts)
        }
        Some(show_path) => run_show(show_path),
        None => {
            eprintln!("usage: turtled <path/to/show.toml>          load + validate a show");
            eprintln!("       turtled play <bundle> [song] [-v]    play a song to the device (Linux)");
            eprintln!("       turtled control <bundle> [song] [-v] drive playback from MIDI (Linux)");
            eprintln!("  -v, --verbose   log each dispatched MIDI event (bring-up diagnostics)");
            ExitCode::FAILURE
        }
    }
}

/// Parsed args for the `play` / `control` subcommands: two positionals (bundle,
/// song) plus a `-v`/`--verbose` flag accepted in any position.
struct CmdOpts {
    bundle: Option<String>,
    song: Option<String>,
    verbose: bool,
}

impl CmdOpts {
    fn parse(args: impl Iterator<Item = String>) -> Self {
        let mut positionals = Vec::new();
        let mut verbose = false;
        for arg in args {
            match arg.as_str() {
                "-v" | "--verbose" => verbose = true,
                _ => positionals.push(arg),
            }
        }
        let mut it = positionals.into_iter();
        CmdOpts { bundle: it.next(), song: it.next(), verbose }
    }
}

/// `turtled control <bundle> [song]`: drive a song's transport from live MIDI.
fn control_command(opts: CmdOpts) -> ExitCode {
    let Some(bundle) = opts.bundle else {
        eprintln!("usage: turtled control <bundle-dir> [song] [-v]");
        return ExitCode::FAILURE;
    };
    #[cfg(target_os = "linux")]
    {
        match control::run(std::path::Path::new(&bundle), opts.song.as_deref(), opts.verbose) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("control: {e}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (&bundle, &opts.song, opts.verbose);
        eprintln!("control requires Linux/ALSA (this host is {})", std::env::consts::OS);
        ExitCode::FAILURE
    }
}

/// `turtled play <bundle> [song]`: play a bundle's song to the audio device.
fn play_command(opts: CmdOpts) -> ExitCode {
    let Some(bundle) = opts.bundle else {
        eprintln!("usage: turtled play <bundle-dir> [song] [-v]");
        return ExitCode::FAILURE;
    };
    #[cfg(target_os = "linux")]
    {
        match play::run(std::path::Path::new(&bundle), opts.song.as_deref(), opts.verbose) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("play: {e}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        // The audio runtime is Linux-only; keep the args "used" so the dev-Mac
        // build stays warning-free.
        let _ = (&bundle, &opts.song, opts.verbose);
        eprintln!("play requires Linux/ALSA (this host is {})", std::env::consts::OS);
        ExitCode::FAILURE
    }
}

/// The original load + validate + wiring path (unchanged, drives the smoke test).
fn run_show(show_path: &str) -> ExitCode {
    let show = match turtle_core::Show::load(show_path) {
        Ok(show) => show,
        Err(e) => {
            eprintln!("could not load {show_path}: {e}");
            return ExitCode::FAILURE;
        }
    };
    if let Err(e) = show.validate() {
        eprintln!("show {show_path} is invalid: {e}");
        return ExitCode::FAILURE;
    }

    // Non-RT host: wire the engine to no-op backends. On a Pi, these become the
    // ALSA PCM device and the CME rawmidi fan-out.
    let audio = NullAudio {
        sample_rate: show.show.playback_rate,
        buffer_frames: show.audio.buffer_frames as usize,
    };
    let mut eng = engine::Engine::new(&show, NullMidi);
    let (_rt_tx, _rt_rx) = engine::rt_channel(256);

    println!(
        "loaded {:?}: {} destination(s), {} song(s); audio {} Hz / {} frames; state {:?}",
        show.show.name,
        show.destinations.len(),
        show.setlist.len(),
        audio.sample_rate(),
        audio.buffer_frames(),
        eng.state(),
    );
    println!("RT runtime not started (requires Linux/ALSA). Engine wiring OK.");
    // Touch the engine so the pending-preload path is exercised in the skeleton.
    let _ = eng.take_pending_preload();
    ExitCode::SUCCESS
}
