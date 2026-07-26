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
//! ALSA backend, MIDI-input transport control ([`control`]), and the §10 control
//! socket ([`socket`]) that the `turtle` CLI drives.
//! `turtled play <bundle>` plays a song to the device; `turtled control <bundle>`
//! drives its transport from a live MIDI controller *and* the control socket.
//! The default `turtled <show.toml>` still just loads + validates.
//!
//! The audio and MIDI threads request `SCHED_FIFO` priorities on startup
//! ([`sched`], §3); `--rt-prio 0` opts out.
//!
//! What is **not** here yet: GPIO (§8.1), the systemd integration (§12), and
//! resolving logical MIDI port labels to ALSA device names.

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
mod sched;
mod scheduler;
mod socket;
mod stems;

use std::process::ExitCode;

use backend::{AudioBackend, NullAudio};

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        // `play` runs the real audio path (Linux/ALSA). Everything else is
        // treated as a show path and takes the unchanged load+validate path.
        Some("play") => match CmdOpts::parse(args) {
            Ok(opts) => play_command(opts),
            Err(e) => arg_error(e),
        },
        Some("control") => match CmdOpts::parse(args) {
            Ok(opts) => control_command(opts),
            Err(e) => arg_error(e),
        },
        Some(show_path) => run_show(show_path),
        None => {
            eprintln!("usage: turtled <path/to/show.toml>            load + validate a show");
            eprintln!(
                "       turtled play <bundle> [song] [-v]      play a song to the device (Linux)"
            );
            eprintln!("       turtled control <bundle> [song] [-v]   drive playback from MIDI + socket (Linux)");
            eprintln!(
                "  -v, --verbose        log each dispatched MIDI event (bring-up diagnostics)"
            );
            eprintln!(
                "  --socket <path>      control socket to bind (control only; default {})",
                turtle_core::proto::DEFAULT_SOCKET_PATH
            );
            eprintln!(
                "  --rt-prio <n>        SCHED_FIFO priority for audio (default {}, 0 disables)",
                sched::AUDIO_PRIORITY
            );
            ExitCode::FAILURE
        }
    }
}

/// Report an argument-parse error uniformly.
fn arg_error(e: String) -> ExitCode {
    eprintln!("turtled: {e}");
    ExitCode::FAILURE
}

/// Parsed args for the `play` / `control` subcommands: two positionals (bundle,
/// song), a `-v`/`--verbose` flag, an optional `--socket <path>` (control only),
/// and an optional `--rt-prio <n>`, all accepted in any position.
#[derive(Debug)]
struct CmdOpts {
    bundle: Option<String>,
    song: Option<String>,
    verbose: bool,
    socket: Option<String>,
    /// Audio-thread `SCHED_FIFO` priority; `None` = run at normal priority.
    /// The MIDI/control thread derives its own one step below (§3).
    rt_priority: Option<u8>,
}

impl CmdOpts {
    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut positionals = Vec::new();
        let mut verbose = false;
        let mut socket = None;
        // Default to the spec's RT behaviour (§3); `--rt-prio 0` opts out.
        let mut rt_priority = Some(sched::AUDIO_PRIORITY);
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-v" | "--verbose" => verbose = true,
                // Consumes the next arg as its value; a trailing `--socket`
                // with nothing after it is a usage error, not a silent default.
                "--socket" | "-s" => {
                    socket = Some(args.next().ok_or("--socket needs a path")?);
                }
                "--rt-prio" => {
                    let raw = args.next().ok_or("--rt-prio needs a number (0 disables)")?;
                    let n: u8 = raw
                        .parse()
                        .map_err(|_| format!("--rt-prio: '{raw}' is not a number 0..=255"))?;
                    // 0 is the documented "don't touch scheduling" sentinel, so
                    // it maps to None rather than to an invalid priority.
                    rt_priority = (n > 0).then_some(n);
                }
                _ => positionals.push(arg),
            }
        }
        let mut it = positionals.into_iter();
        Ok(CmdOpts {
            bundle: it.next(),
            song: it.next(),
            verbose,
            socket,
            rt_priority,
        })
    }
}

/// `turtled control <bundle> [song]`: drive a song's transport from live MIDI.
fn control_command(opts: CmdOpts) -> ExitCode {
    let Some(bundle) = opts.bundle else {
        eprintln!("usage: turtled control <bundle-dir> [song] [-v]");
        return ExitCode::FAILURE;
    };
    // The socket path: the `--socket` override, else the protocol default.
    let socket = opts
        .socket
        .clone()
        .unwrap_or_else(|| turtle_core::proto::DEFAULT_SOCKET_PATH.to_string());
    #[cfg(target_os = "linux")]
    {
        match control::run(
            std::path::Path::new(&bundle),
            opts.song.as_deref(),
            opts.verbose,
            std::path::Path::new(&socket),
            opts.rt_priority,
        ) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("control: {e}");
                ExitCode::FAILURE
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (&bundle, &opts.song, opts.verbose, &socket, opts.rt_priority);
        eprintln!(
            "control requires Linux/ALSA (this host is {})",
            std::env::consts::OS
        );
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
        match play::run(
            std::path::Path::new(&bundle),
            opts.song.as_deref(),
            opts.verbose,
            opts.rt_priority,
        ) {
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
        let _ = (&bundle, &opts.song, opts.verbose, opts.rt_priority);
        eprintln!(
            "play requires Linux/ALSA (this host is {})",
            std::env::consts::OS
        );
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
    let mut eng = engine::Engine::new(&show);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<CmdOpts, String> {
        CmdOpts::parse(args.iter().map(|s| s.to_string()))
    }

    /// RT scheduling is the spec's default (§3), not opt-in: forgetting the flag
    /// must still give the show a real-time audio thread.
    #[test]
    fn rt_priority_defaults_on() {
        let opts = parse(&["bundle"]).unwrap();
        assert_eq!(opts.rt_priority, Some(sched::AUDIO_PRIORITY));
    }

    /// `0` is the documented opt-out, and must become `None` rather than an
    /// invalid priority the kernel would reject.
    #[test]
    fn rt_prio_zero_disables_rather_than_requesting_zero() {
        let opts = parse(&["bundle", "--rt-prio", "0"]).unwrap();
        assert_eq!(opts.rt_priority, None);
    }

    #[test]
    fn rt_prio_accepts_an_explicit_priority() {
        let opts = parse(&["bundle", "--rt-prio", "60"]).unwrap();
        assert_eq!(opts.rt_priority, Some(60));
    }

    /// A typo'd priority must be a loud usage error — silently falling back to
    /// the default would hide a deliberate tuning attempt.
    #[test]
    fn a_non_numeric_rt_prio_is_an_error() {
        let err = parse(&["bundle", "--rt-prio", "high"]).unwrap_err();
        assert!(err.contains("not a number"), "{err}");
        assert!(parse(&["bundle", "--rt-prio"]).is_err());
    }

    /// Flags are position-independent and must not be mistaken for positionals.
    #[test]
    fn flags_do_not_consume_positional_slots() {
        let opts = parse(&["--rt-prio", "50", "bundle", "-v", "song", "--socket", "/x.sock"])
            .unwrap();
        assert_eq!(opts.bundle.as_deref(), Some("bundle"));
        assert_eq!(opts.song.as_deref(), Some("song"));
        assert!(opts.verbose);
        assert_eq!(opts.socket.as_deref(), Some("/x.sock"));
        assert_eq!(opts.rt_priority, Some(50));
    }
}
