//! The `turtle` control-socket client (spec §10).
//!
//! One request, one reply, one connection — except `monitor`, which reads the
//! daemon's event stream until interrupted. The wire types live in
//! [`turtle_core::proto`], shared with the daemon so the two can't disagree.

use std::io::{BufRead, BufReader, Write};
use std::os::unix::net::UnixStream;
use std::path::Path;

use turtle_core::proto::{Request, Response, Status};

/// Send one request and return the daemon's single-line reply.
///
/// The error `String` is printed straight to stderr, so it is phrased for a
/// human at a terminal ("is turtled running?"), not for a machine.
pub fn request(socket: &Path, req: &Request) -> Result<Response, String> {
    let mut stream = connect(socket)?;
    stream
        .write_all(req.to_line().as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|e| format!("send: {e}"))?;

    let mut reader = BufReader::new(stream);
    let mut line = String::new();
    // A daemon that accepted the connection but sent nothing (0 bytes read)
    // has effectively hung up mid-reply — report that rather than a confusing
    // "bad response" parse error on an empty string.
    if reader
        .read_line(&mut line)
        .map_err(|e| format!("receive: {e}"))?
        == 0
    {
        return Err("turtled closed the connection without replying".into());
    }
    Response::from_line(&line)
}

/// Connect, translating the two failures a user actually hits into advice.
fn connect(socket: &Path) -> Result<UnixStream, String> {
    UnixStream::connect(socket).map_err(|e| match e.kind() {
        // No socket file, or a stale one no one is listening on: the daemon
        // isn't running. This is the common case, so name the fix.
        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused => {
            format!(
                "no turtled listening on {} (is it running?)",
                socket.display()
            )
        }
        _ => format!("connect {}: {e}", socket.display()),
    })
}

/// Print a [`Status`] the way a performer wants to read it at a glance, not as
/// JSON. `state` first because it is the thing you check mid-set.
pub fn print_status(s: &Status) {
    let state = format!("{:?}", s.state).to_lowercase();
    let song = s.song.as_deref().unwrap_or("(none)");
    println!("show:   {}", s.show);
    println!("state:  {state}");
    println!("song:   {song}");
    if let Some(next) = &s.armed_next {
        println!("next:   {next}");
    }
    // mm:ss / mm:ss reads better than raw seconds for anything over a minute.
    println!("pos:    {} / {}", mmss(s.position_s), mmss(s.duration_s));
}

/// Seconds as `m:ss`.
fn mmss(secs: f64) -> String {
    let secs = secs.max(0.0) as u64;
    format!("{}:{:02}", secs / 60, secs % 60)
}

/// Stream `monitor` events until the connection closes (the daemon exits) or
/// the user interrupts (Ctrl-C). Each line is one [`Response::Event`].
pub fn monitor(socket: &Path) -> Result<(), String> {
    let mut stream = connect(socket)?;
    stream
        .write_all(Request::Monitor.to_line().as_bytes())
        .and_then(|()| stream.flush())
        .map_err(|e| format!("send: {e}"))?;

    println!("monitoring {} (Ctrl-C to stop)", socket.display());
    let reader = BufReader::new(stream);
    for line in reader.lines() {
        let line = line.map_err(|e| format!("receive: {e}"))?;
        match Response::from_line(&line) {
            Ok(Response::Event(event)) => println!("{}", render_event(&event)),
            // A non-event on the monitor stream (e.g. an Error the daemon sent
            // before upgrading us) is still worth showing verbatim.
            Ok(other) => println!("{other:?}"),
            Err(e) => eprintln!("warning: {e}"),
        }
    }
    Ok(())
}

/// One monitor line, formatted for a human debugging a controller map.
fn render_event(event: &turtle_core::proto::Event) -> String {
    use turtle_core::proto::Event;
    match event {
        Event::Midi {
            wall_s,
            bytes,
            decoded,
        } => {
            let hex: Vec<String> = bytes.iter().map(|b| format!("{b:02X}")).collect();
            // The whole point of `monitor` is spotting a message that matched
            // nothing, so say so loudly rather than leaving a blank.
            let meaning = decoded.as_deref().unwrap_or("(unmapped)");
            format!("{wall_s:8.3}  midi   {:12} {meaning}", hex.join(" "))
        }
        Event::Command {
            wall_s,
            source,
            command,
            state,
        } => {
            let source = format!("{source:?}").to_lowercase();
            let state = format!("{state:?}").to_lowercase();
            format!("{wall_s:8.3}  {source:7} {command:12} -> {state}")
        }
    }
}
