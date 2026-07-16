//! The `turtled` control socket's JSON line protocol (spec §10).
//!
//! This lives in `turtle-core` because **both ends need it**: `turtled` serves
//! the socket and the `turtle` CLI is a thin client. It is pure data — no RT,
//! ALSA, or OS concerns — so it compiles and unit-tests on any host, including
//! the dev Mac.
//!
//! **Wire format:** one JSON object per line, `\n`-terminated; the client sends
//! a [`Request`] line, the daemon answers with a [`Response`] line. Line-
//! delimited (rather than length-prefixed) so the socket stays debuggable by
//! hand:
//!
//! ```text
//! $ nc -U /tmp/turtle.sock
//! {"cmd":"status"}
//! {"reply":"status","show":"Tone","state":"playing","song":"tone",...}
//! ```

use serde::{Deserialize, Serialize};

use crate::transport::State;

/// Where the daemon listens unless `--socket <path>` overrides it.
///
/// `/tmp` (not `/run/turtle`) so an unprivileged `turtled` started by hand on
/// the Pi works with no setup; the systemd unit (§12) will pass an explicit
/// `--socket /run/turtle/control.sock` with a `RuntimeDirectory=`. The socket
/// is created mode `0600`, so "world-writable /tmp" does not mean anyone on
/// the box can stop your show.
pub const DEFAULT_SOCKET_PATH: &str = "/tmp/turtle.sock";

/// A request from the `turtle` CLI to the daemon.
///
/// `#[serde(tag = "cmd")]` makes this an *internally tagged* enum: the variant
/// name rides in a `"cmd"` field alongside that variant's own fields, so we get
/// `{"cmd":"arm","song":"second"}` rather than serde's default externally
/// tagged `{"Arm":{"song":"second"}}` — friendlier to hand-write over `nc` and
/// to read in a log.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "lowercase")]
pub enum Request {
    /// Report transport state, current song, and position.
    Status,
    /// Arm a setlist entry by its song-directory name.
    Arm { song: String },
    /// Start / continue / restart the transport.
    Start,
    /// Stop (first = clean release + rewind; a second = panic, per §8).
    Stop,
    /// Arm the next setlist entry.
    Next,
    /// Arm the previous setlist entry.
    Prev,
    /// All-notes-off + reset-all-controllers on every port.
    Panic,
    /// Stream decoded incoming commands until the client disconnects
    /// (`turtle monitor`). The only request with a multi-line response.
    Monitor,
}

/// The daemon's answer to a [`Request`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "reply", rename_all = "lowercase")]
pub enum Response {
    /// The command was accepted. For the transport verbs this means "handed to
    /// the control loop", **not** "its effect is already audible" — they are
    /// fire-and-forget, so the socket never blocks waiting on the loop.
    Ok,
    /// Answer to [`Request::Status`].
    Status(Status),
    /// One line of a [`Request::Monitor`] stream.
    Event(Event),
    /// The request was understood but could not be served (unknown song,
    /// malformed line, ...).
    Error { message: String },
}

/// A snapshot of what the daemon is doing, for `turtle status`.
///
/// Deliberately a plain owned struct: the control loop republishes one of these
/// each iteration into a mutex the socket thread reads, so a `status` request
/// is answered without a round-trip through (or any blocking of) the loop.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Status {
    /// `show.toml`'s show name.
    pub show: String,
    /// Transport state (§8).
    pub state: State,
    /// Song-directory name of the current (armed/playing) song.
    pub song: Option<String>,
    /// A song armed to start next via gapless auto-advance, if any.
    pub armed_next: Option<String>,
    /// Transport position, seconds.
    pub position_s: f64,
    /// Current song's length, seconds.
    pub duration_s: f64,
}

/// Where a command came from, for `monitor`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    /// Decoded from the foot controller (§8).
    Midi,
    /// Injected over this control socket.
    Socket,
    /// Raised by the daemon itself (loader finished, RT hit the song end).
    Internal,
}

/// One `turtle monitor` line (spec §10: "print incoming commands").
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "event", rename_all = "lowercase")]
pub enum Event {
    /// A MIDI message arrived on the control port. `decoded` is `None` for an
    /// unmapped message — which is exactly what you need to see when debugging
    /// a controller map that isn't firing.
    Midi {
        wall_s: f64,
        bytes: Vec<u8>,
        decoded: Option<String>,
    },
    /// A transport command was applied, from any [`Source`]. `state` is the
    /// state *after* applying it.
    Command {
        wall_s: f64,
        source: Source,
        command: String,
        state: State,
    },
}

impl Request {
    /// Encode as one `\n`-terminated wire line.
    ///
    /// `expect` rather than `Result`: these are plain owned data types with no
    /// map keys that could fail to serialize, so an error here would be a bug
    /// in this module, not a runtime condition a caller could handle.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("Request always serializes");
        s.push('\n');
        s
    }

    /// Decode one wire line. The `String` error is the message we hand straight
    /// back to the client in a [`Response::Error`].
    pub fn from_line(line: &str) -> Result<Self, String> {
        serde_json::from_str(line.trim()).map_err(|e| format!("bad request: {e}"))
    }
}

impl Response {
    /// Encode as one `\n`-terminated wire line. See [`Request::to_line`] on the
    /// `expect`.
    pub fn to_line(&self) -> String {
        let mut s = serde_json::to_string(self).expect("Response always serializes");
        s.push('\n');
        s
    }

    /// Decode one wire line (the CLI side).
    pub fn from_line(line: &str) -> Result<Self, String> {
        serde_json::from_str(line.trim()).map_err(|e| format!("bad response: {e}"))
    }

    /// Shorthand for [`Response::Error`] from anything printable.
    pub fn error(message: impl std::fmt::Display) -> Self {
        Response::Error {
            message: message.to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The wire form is a stable contract between two separately-built
    /// binaries, so pin the exact bytes rather than only round-tripping —
    /// a rename that silently changed the tag would otherwise pass.
    #[test]
    fn request_wire_format_is_internally_tagged() {
        assert_eq!(Request::Status.to_line(), "{\"cmd\":\"status\"}\n");
        assert_eq!(
            Request::Arm {
                song: "second".into()
            }
            .to_line(),
            "{\"cmd\":\"arm\",\"song\":\"second\"}\n"
        );
    }

    #[test]
    fn requests_round_trip() {
        for req in [
            Request::Status,
            Request::Start,
            Request::Stop,
            Request::Next,
            Request::Prev,
            Request::Panic,
            Request::Monitor,
            Request::Arm { song: "x".into() },
        ] {
            let line = req.to_line();
            assert_eq!(
                Request::from_line(&line).unwrap(),
                req,
                "round-trip {req:?}"
            );
        }
    }

    #[test]
    fn status_response_flattens_next_to_its_tag() {
        let status = Status {
            show: "Tone".into(),
            state: State::Playing,
            song: Some("tone".into()),
            armed_next: None,
            position_s: 1.5,
            duration_s: 10.0,
        };
        let line = Response::Status(status.clone()).to_line();
        // `state` serializes lowercase, and the payload sits beside the tag
        // rather than nested under it.
        assert!(line.contains("\"reply\":\"status\""), "{line}");
        assert!(line.contains("\"state\":\"playing\""), "{line}");
        assert_eq!(
            Response::from_line(&line).unwrap(),
            Response::Status(status)
        );
    }

    #[test]
    fn monitor_event_round_trips() {
        let ev = Event::Midi {
            wall_s: 0.25,
            bytes: vec![0x90, 60, 100],
            decoded: Some("Start".into()),
        };
        let line = Response::Event(ev.clone()).to_line();
        assert_eq!(Response::from_line(&line).unwrap(), Response::Event(ev));
    }

    /// An unmapped message is reported with `decoded: null`, not omitted —
    /// "this arrived and matched nothing" is the whole point of `monitor`.
    #[test]
    fn unmapped_midi_event_keeps_an_explicit_null() {
        let line = Response::Event(Event::Midi {
            wall_s: 0.0,
            bytes: vec![0xB0, 99, 1],
            decoded: None,
        })
        .to_line();
        assert!(line.contains("\"decoded\":null"), "{line}");
    }

    #[test]
    fn a_garbage_line_is_a_reportable_error_not_a_panic() {
        assert!(Request::from_line("not json").is_err());
        assert!(Request::from_line("{\"cmd\":\"nope\"}").is_err());
    }
}
