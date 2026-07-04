//! `turtled` — the Turtle daemon (spec §3).
//!
//! Scaffolding only. The real implementation is three long-lived threads (audio
//! RT, MIDI scheduler, control) plus a background loader, communicating over
//! lock-free SPSC queues. None of that exists yet; this binary currently just
//! loads and validates a show bundle so the data model is exercised end-to-end.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    let Some(show_path) = args.next() else {
        eprintln!("usage: turtled <path/to/show.toml>");
        eprintln!("(daemon threads not yet implemented — this only loads + validates)");
        return ExitCode::FAILURE;
    };

    match turtle_core::Show::load(&show_path) {
        Ok(show) => match show.validate() {
            Ok(()) => {
                println!(
                    "loaded show {:?}: {} destination(s), {} song(s) in setlist",
                    show.show.name,
                    show.destinations.len(),
                    show.setlist.len()
                );
                ExitCode::SUCCESS
            }
            Err(e) => {
                eprintln!("show {show_path} is invalid: {e}");
                ExitCode::FAILURE
            }
        },
        Err(e) => {
            eprintln!("could not load {show_path}: {e}");
            ExitCode::FAILURE
        }
    }
}
