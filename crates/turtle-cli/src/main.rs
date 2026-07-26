//! `turtle` — the thin control-socket client (spec §10).
//!
//! Two kinds of subcommand:
//!   * **offline** (`validate`, `gen-tone`) need no daemon — they act on bundle
//!     files directly;
//!   * **socket** (`status`, `arm`, `start`, `stop`, `next`, `prev`, `panic`,
//!     `monitor`) speak the JSON line protocol in [`turtle_core::proto`] to a
//!     running `turtled` over its Unix socket.
//!
//! Still to come per §10: `doctor` (mostly-offline preflight), `calibrate`,
//! `test` — each its own workflow, deferred to a later pass.

use std::path::{Path, PathBuf};
use std::process::ExitCode;

use turtle_core::proto::{Request, Response, DEFAULT_SOCKET_PATH};

mod client;
mod gen;

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // A global `--socket <path>` may appear anywhere; pull it out first so each
    // command parser sees only its own positionals.
    let (socket, rest) = match extract_socket(&args) {
        Ok(pair) => pair,
        Err(e) => {
            eprintln!("turtle: {e}");
            return ExitCode::FAILURE;
        }
    };

    match rest.first().map(String::as_str) {
        Some("gen-tone") => gen_tone(&rest[1..]),
        Some("validate") => validate(&rest[1..]),
        Some("status") => socket_status(&socket),
        Some("monitor") => match client::monitor(&socket) {
            Ok(()) => ExitCode::SUCCESS,
            Err(e) => {
                eprintln!("turtle: {e}");
                ExitCode::FAILURE
            }
        },
        Some("arm") => match rest.get(1) {
            Some(song) => send(&socket, Request::Arm { song: song.clone() }),
            None => {
                eprintln!("usage: turtle arm <song>");
                ExitCode::FAILURE
            }
        },
        Some("start") => send(&socket, Request::Start),
        Some("stop") => send(&socket, Request::Stop),
        Some("next") => send(&socket, Request::Next),
        Some("prev") => send(&socket, Request::Prev),
        Some("panic") => send(&socket, Request::Panic),
        _ => {
            usage();
            ExitCode::FAILURE
        }
    }
}

/// Pull an optional `--socket <path>` out of the args, returning the socket to
/// use (the override or the default) and the remaining args. A trailing
/// `--socket` with no value is a usage error rather than a silent default.
fn extract_socket(args: &[String]) -> Result<(PathBuf, Vec<String>), String> {
    let mut socket = PathBuf::from(DEFAULT_SOCKET_PATH);
    let mut rest = Vec::new();
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--socket" | "-s" => {
                let path = it.next().ok_or("--socket needs a path")?;
                socket = PathBuf::from(path);
            }
            _ => rest.push(arg.clone()),
        }
    }
    Ok((socket, rest))
}

/// Send one fire-and-forget request; map the reply to an exit code.
fn send(socket: &Path, req: Request) -> ExitCode {
    match client::request(socket, &req) {
        Ok(Response::Ok) => ExitCode::SUCCESS,
        Ok(Response::Error { message }) => {
            eprintln!("turtle: {message}");
            ExitCode::FAILURE
        }
        // A transport verb should only ever get Ok or Error back; anything else
        // is a protocol mismatch worth surfacing rather than swallowing.
        Ok(other) => {
            eprintln!("turtle: unexpected reply: {other:?}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("turtle: {e}");
            ExitCode::FAILURE
        }
    }
}

fn socket_status(socket: &Path) -> ExitCode {
    match client::request(socket, &Request::Status) {
        Ok(Response::Status(status)) => {
            client::print_status(&status);
            ExitCode::SUCCESS
        }
        Ok(Response::Error { message }) => {
            eprintln!("turtle: {message}");
            ExitCode::FAILURE
        }
        Ok(other) => {
            eprintln!("turtle: unexpected reply: {other:?}");
            ExitCode::FAILURE
        }
        Err(e) => {
            eprintln!("turtle: {e}");
            ExitCode::FAILURE
        }
    }
}

fn gen_tone(args: &[String]) -> ExitCode {
    let Some(out) = args.first() else {
        eprintln!("usage: turtle gen-tone <out-dir> [seconds] [hz]");
        return ExitCode::FAILURE;
    };
    // Optional positionals: seconds (default 5), Hz (default 440). `unwrap_or`
    // supplies the default when absent or unparsable.
    let seconds = args.get(1).and_then(|s| s.parse().ok()).unwrap_or(5.0);
    let hz = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(440.0);
    match gen::gen_tone(Path::new(out), seconds, hz) {
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

fn validate(args: &[String]) -> ExitCode {
    let Some(path) = args.first() else {
        eprintln!("usage: turtle validate <path/to/show.toml>");
        return ExitCode::FAILURE;
    };
    match turtle_core::Show::load(path).and_then(|s| s.validate()) {
        Ok(()) => {
            println!("{path}: ok");
            ExitCode::SUCCESS
        }
        Err(e) => {
            eprintln!("{path}: {e}");
            ExitCode::FAILURE
        }
    }
}

fn usage() {
    eprintln!("turtle <command> [--socket <path>]");
    eprintln!();
    eprintln!("offline (no daemon):");
    eprintln!("  validate <show.toml>          bundle validation");
    eprintln!("  gen-tone <out-dir> [s] [hz]   write a playable test bundle");
    eprintln!();
    eprintln!("control socket (needs a running turtled):");
    eprintln!("  status                        transport state, song, position");
    eprintln!("  arm <song>                    arm a setlist entry by name");
    eprintln!("  start | stop | next | prev    drive the transport");
    eprintln!("  panic                         all-notes-off on every port");
    eprintln!("  monitor                       stream incoming commands");
    eprintln!();
    eprintln!("  --socket <path>  control socket to use (default {DEFAULT_SOCKET_PATH})");
}
