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
//! The audio and MIDI threads request `SCHED_FIFO` priorities on startup, the
//! process locks its memory, and the audio thread is pinned to an
//! `isolcpus`-reserved core if there is one ([`sched`], §3/§12); `--rt-prio 0`
//! opts out of the first two and `--audio-cpu none` out of the third.
//! Under systemd it reports readiness and pings the watchdog ([`notify`], §12) —
//! started from a shell, that is a no-op.
//!
//! What is **not** here yet: GPIO (§8.1), and resolving logical MIDI port labels
//! to ALSA device names.

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
mod clock_out;
mod clock_probe;
mod control;
mod control_map;
mod engine;
mod mixer;
mod notes;
mod notify;
mod play;
mod retry;
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
            eprintln!("  -v, --verbose        log each dispatched MIDI event (bring-up diagnostics)");
            eprintln!(
                "  --clock-probe        measure MIDI-clock pulse jitter and log it (sends nothing)"
            );
            eprintln!(
                "  --socket <path>      control socket to bind (control only; default $TURTLE_SOCKET,"
            );
            eprintln!(
                "                       else {})",
                turtle_core::proto::DEFAULT_SOCKET_PATH
            );
            eprintln!(
                "  --rt-prio <n>        SCHED_FIFO priority for audio (default {}, 0 disables)",
                sched::AUDIO_PRIORITY
            );
            eprintln!(
                "  --wait-devices <s>   seconds to wait for audio/MIDI to appear (default {}, 0 off)",
                retry::DEFAULT_WAIT.as_secs()
            );
            eprintln!(
                "  --audio-cpu <n>      pin the audio thread to a CPU; 'auto' (default) uses an"
            );
            eprintln!(
                "                       isolcpus-reserved core if there is one, 'none' never pins"
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
/// an optional `--rt-prio <n>`, and an optional `--audio-cpu <n|auto|none>`, all
/// accepted in any position.
#[derive(Debug)]
struct CmdOpts {
    bundle: Option<String>,
    song: Option<String>,
    verbose: bool,
    socket: Option<String>,
    /// Audio-thread `SCHED_FIFO` priority; `None` = run at normal priority.
    /// The MIDI/control thread derives its own one step below (§3).
    rt_priority: Option<u8>,
    /// Which CPU to pin the audio thread to (§12).
    audio_cpu: CpuChoice,
    /// How long to wait for the audio/MIDI devices to appear before giving up
    /// (§12). `0` restores the old fail-immediately behaviour.
    wait_devices: std::time::Duration,
    /// Measure MIDI-clock pulse jitter and log it, sending nothing (§5). A
    /// diagnostic for deciding whether clock can be dispatched from the ~1 ms
    /// control loop or needs its own thread — see [`crate::clock_probe`].
    clock_probe: bool,
}

/// How the audio thread's CPU is chosen.
///
/// Three states rather than `Option<usize>` because "decide for me" and "don't
/// pin" are genuinely different intents, and collapsing them would make the
/// default unexpressible: `Auto` pins only when a core has actually been
/// reserved with `isolcpus`, whereas `None` is an explicit override for A/B
/// testing on a machine that *does* have one reserved.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CpuChoice {
    /// Pin to the top isolated CPU if any exist, else do not pin. The default.
    Auto,
    /// Never pin, even if CPUs are isolated.
    None,
    /// Pin to exactly this CPU.
    Fixed(usize),
}

impl CpuChoice {
    /// Resolve to an actual CPU, consulting the kernel only for `Auto`.
    fn resolve(self) -> Option<usize> {
        match self {
            CpuChoice::Auto => sched::audio_cpu_for(&sched::isolated_cpus()),
            CpuChoice::None => Option::None,
            CpuChoice::Fixed(cpu) => Some(cpu),
        }
    }

    fn parse(raw: &str) -> Result<Self, String> {
        match raw {
            "auto" => Ok(CpuChoice::Auto),
            // Both spellings: "none" reads naturally, "-1" matches how
            // `--rt-prio 0` disables its feature.
            "none" | "-1" => Ok(CpuChoice::None),
            _ => raw
                .parse::<usize>()
                .map(CpuChoice::Fixed)
                .map_err(|_| format!("--audio-cpu: '{raw}' is not a CPU number, 'auto', or 'none'")),
        }
    }
}

impl CmdOpts {
    /// Collapse the parsed flags into the tuning the RT threads actually use.
    /// This is where `--audio-cpu auto` becomes a concrete CPU (or nothing).
    fn tuning(&self) -> sched::Tuning {
        sched::Tuning {
            rt_priority: self.rt_priority,
            audio_cpu: self.audio_cpu.resolve(),
        }
    }

    fn parse(mut args: impl Iterator<Item = String>) -> Result<Self, String> {
        let mut positionals = Vec::new();
        let mut verbose = false;
        let mut socket = None;
        // Default to the spec's RT behaviour (§3); `--rt-prio 0` opts out.
        let mut rt_priority = Some(sched::AUDIO_PRIORITY);
        // Auto: use a reserved core if `isolcpus` gave us one, otherwise leave
        // placement to the scheduler (§12).
        let mut audio_cpu = CpuChoice::Auto;
        let mut wait_devices = retry::DEFAULT_WAIT;
        let mut clock_probe = false;
        while let Some(arg) = args.next() {
            match arg.as_str() {
                "-v" | "--verbose" => verbose = true,
                "--clock-probe" => clock_probe = true,
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
                "--wait-devices" => {
                    let raw = args
                        .next()
                        .ok_or("--wait-devices needs a number of seconds (0 to disable)")?;
                    let secs: u64 = raw.parse().map_err(|_| {
                        format!("--wait-devices: '{raw}' is not a number of seconds")
                    })?;
                    wait_devices = std::time::Duration::from_secs(secs);
                }
                "--audio-cpu" => {
                    let raw = args
                        .next()
                        .ok_or("--audio-cpu needs a CPU number, 'auto', or 'none'")?;
                    audio_cpu = CpuChoice::parse(&raw)?;
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
            audio_cpu,
            wait_devices,
            clock_probe,
        })
    }
}

/// `turtled control <bundle> [song]`: drive a song's transport from live MIDI.
fn control_command(opts: CmdOpts) -> ExitCode {
    // Resolve `--audio-cpu auto` against the kernel here, before any thread is
    // spawned, so the decision is made once and both threads see the same answer.
    // Must precede the `opts.bundle` move below.
    let tuning = opts.tuning();
    let Some(bundle) = opts.bundle else {
        eprintln!("usage: turtled control <bundle-dir> [song] [-v]");
        return ExitCode::FAILURE;
    };
    // The socket path: the `--socket` override, else the resolved default. The
    // daemon *creates* the socket, so unlike the CLI it must not prefer an
    // existing path — a leftover `/run/turtle/control.sock` would silently move
    // where a hand-started daemon listens. Hence `$TURTLE_SOCKET` (which the unit
    // sets) or the plain `/tmp` default, never the exists-check.
    let socket = opts.socket.clone().unwrap_or_else(|| {
        std::env::var(turtle_core::proto::SOCKET_ENV)
            .ok()
            .filter(|p| !p.is_empty())
            .unwrap_or_else(|| turtle_core::proto::DEFAULT_SOCKET_PATH.to_string())
    });
    #[cfg(target_os = "linux")]
    {
        match control::run(
            std::path::Path::new(&bundle),
            opts.song.as_deref(),
            opts.verbose,
            std::path::Path::new(&socket),
            tuning,
            opts.wait_devices,
            opts.clock_probe,
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
        let _ =
            (&bundle, &opts.song, opts.verbose, &socket, tuning, opts.wait_devices, opts.clock_probe);
        eprintln!(
            "control requires Linux/ALSA (this host is {})",
            std::env::consts::OS
        );
        ExitCode::FAILURE
    }
}

/// `turtled play <bundle> [song]`: play a bundle's song to the audio device.
fn play_command(opts: CmdOpts) -> ExitCode {
    let tuning = opts.tuning();
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
            tuning,
            opts.wait_devices,
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
        let _ = (&bundle, &opts.song, opts.verbose, tuning, opts.wait_devices);
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

    /// Pinning must default to `Auto` — using a reserved core if the operator set
    /// one up, and doing nothing if they did not. Neither "always pin" nor "never
    /// pin" is a sensible default.
    #[test]
    fn audio_cpu_defaults_to_auto() {
        assert_eq!(parse(&["bundle"]).unwrap().audio_cpu, CpuChoice::Auto);
    }

    /// All three spellings of the flag, including both opt-outs.
    #[test]
    fn audio_cpu_accepts_a_number_auto_or_none() {
        assert_eq!(
            parse(&["b", "--audio-cpu", "3"]).unwrap().audio_cpu,
            CpuChoice::Fixed(3)
        );
        assert_eq!(
            parse(&["b", "--audio-cpu", "auto"]).unwrap().audio_cpu,
            CpuChoice::Auto
        );
        assert_eq!(
            parse(&["b", "--audio-cpu", "none"]).unwrap().audio_cpu,
            CpuChoice::None
        );
        // `-1` mirrors how `--rt-prio 0` disables its feature.
        assert_eq!(
            parse(&["b", "--audio-cpu", "-1"]).unwrap().audio_cpu,
            CpuChoice::None
        );
    }

    /// A typo must be a loud usage error, not a silent fall back to `auto` —
    /// otherwise a deliberate pinning attempt fails invisibly.
    #[test]
    fn a_bad_audio_cpu_is_an_error() {
        let err = parse(&["b", "--audio-cpu", "core3"]).unwrap_err();
        assert!(err.contains("not a CPU number"), "{err}");
        assert!(parse(&["b", "--audio-cpu"]).is_err());
    }

    /// `CpuChoice::None` must resolve to no pinning even on a machine that *does*
    /// have isolated CPUs — that is the whole point of the explicit override, and
    /// it must not consult the kernel at all.
    #[test]
    fn explicit_none_never_pins_and_fixed_never_consults_the_kernel() {
        assert_eq!(CpuChoice::None.resolve(), None);
        assert_eq!(CpuChoice::Fixed(2).resolve(), Some(2));
    }

    /// Waiting for devices is the default (§12): a USB interface that enumerates a
    /// second after boot must not fail the show.
    #[test]
    fn waiting_for_devices_is_on_by_default() {
        assert_eq!(parse(&["bundle"]).unwrap().wait_devices, retry::DEFAULT_WAIT);
    }

    /// `0` must restore the exact pre-existing fail-immediately behaviour, so the
    /// change can be A/B'd and scripted around.
    #[test]
    fn wait_devices_zero_disables_waiting() {
        let opts = parse(&["bundle", "--wait-devices", "0"]).unwrap();
        assert_eq!(opts.wait_devices, std::time::Duration::ZERO);
    }

    #[test]
    fn wait_devices_accepts_an_explicit_number_of_seconds() {
        let opts = parse(&["bundle", "--wait-devices", "45"]).unwrap();
        assert_eq!(opts.wait_devices, std::time::Duration::from_secs(45));
    }

    /// A typo must be loud rather than silently falling back to the default — the
    /// whole point of passing it is to change the behaviour.
    #[test]
    fn a_non_numeric_wait_devices_is_an_error() {
        let err = parse(&["bundle", "--wait-devices", "soon"]).unwrap_err();
        assert!(err.contains("not a number of seconds"), "{err}");
        assert!(parse(&["bundle", "--wait-devices"]).is_err());
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
