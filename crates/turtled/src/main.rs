//! `turtled` — the Turtle daemon (spec §3).
//!
//! This skeleton contains the platform-independent core of the daemon: the
//! decoupled transport clock (§3.1), the per-port MIDI scheduler (§5), active-
//! note tracking for clean Stop (§8), foot-controller decoding (§8), and the
//! control-thread engine that wires the transport state machine to a lock-free
//! RT command queue (§3) and a MIDI sink.
//!
//! What is **not** here yet: the ALSA PCM loop and rawmidi I/O, thread spawning
//! with `SCHED_FIFO`, and the background stem loader. Those are Linux/ALSA-only
//! (§2) and cannot be built or run on this host, so the concrete backends are
//! left behind the `backend` traits. `main` currently loads + validates a show
//! and constructs the engine to prove the wiring compiles end to end.

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
mod control_map;
mod engine;
mod notes;
mod scheduler;
mod stems;

use std::process::ExitCode;

use backend::{AudioBackend, NullAudio, NullMidi};

fn main() -> ExitCode {
    let Some(show_path) = std::env::args().nth(1) else {
        eprintln!("usage: turtled <path/to/show.toml>");
        eprintln!("(RT audio/MIDI runtime is Linux/ALSA-only; this loads + validates)");
        return ExitCode::FAILURE;
    };

    let show = match turtle_core::Show::load(&show_path) {
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
