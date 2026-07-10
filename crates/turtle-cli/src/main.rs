//! `turtle` — thin control-socket client (spec §10).
//!
//! Scaffolding only. Eventually this speaks the JSON line protocol to `turtled`
//! over a Unix-domain socket (`load`/`status`/`arm`/`start`/`stop`/`panic`, plus
//! `doctor`, `validate`, `calibrate`, `test`, `monitor`). For now it offers the
//! one command that needs no daemon: offline bundle validation.

use std::process::ExitCode;

mod gen;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
        Some("gen-tone") => match args.next() {
            Some(out) => {
                // Optional positional args: seconds (default 5), Hz (default 440).
                // `unwrap_or` supplies the default when the arg is absent or unparsable.
                let seconds = args.next().and_then(|s| s.parse().ok()).unwrap_or(5.0);
                let hz = args.next().and_then(|s| s.parse().ok()).unwrap_or(440.0);
                match gen::gen_tone(std::path::Path::new(&out), seconds, hz) {
                    Ok(()) => {
                        println!("wrote tone bundle to {out} ({seconds}s @ {hz} Hz)");
                        ExitCode::SUCCESS
                    }
                    Err(e) => {
                        eprintln!("gen-tone {out}: {e}");
                        ExitCode::FAILURE
                    }
                }
            }
            None => {
                eprintln!("usage: turtle gen-tone <out-dir> [seconds] [hz]");
                ExitCode::FAILURE
            }
        },
        Some("validate") => match args.next() {
            Some(path) => match turtle_core::Show::load(&path).and_then(|s| s.validate()) {
                Ok(()) => {
                    println!("{path}: ok");
                    ExitCode::SUCCESS
                }
                Err(e) => {
                    eprintln!("{path}: {e}");
                    ExitCode::FAILURE
                }
            },
            None => {
                eprintln!("usage: turtle validate <path/to/show.toml>");
                ExitCode::FAILURE
            }
        },
        _ => {
            eprintln!("turtle <command>");
            eprintln!("  validate <show.toml>          offline bundle validation (no daemon)");
            eprintln!("  gen-tone <out-dir> [s] [hz]   write a playable test bundle (sine tone)");
            eprintln!("(control-socket commands not yet implemented)");
            ExitCode::FAILURE
        }
    }
}
