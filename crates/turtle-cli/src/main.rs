//! `turtle` — thin control-socket client (spec §10).
//!
//! Scaffolding only. Eventually this speaks the JSON line protocol to `turtled`
//! over a Unix-domain socket (`load`/`status`/`arm`/`start`/`stop`/`panic`, plus
//! `doctor`, `validate`, `calibrate`, `test`, `monitor`). For now it offers the
//! one command that needs no daemon: offline bundle validation.

use std::process::ExitCode;

fn main() -> ExitCode {
    let mut args = std::env::args().skip(1);
    match args.next().as_deref() {
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
            eprintln!("  validate <show.toml>   offline bundle validation (no daemon)");
            eprintln!("(control-socket commands not yet implemented)");
            ExitCode::FAILURE
        }
    }
}
