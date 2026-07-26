//! The `turtled` control socket server (spec §10).
//!
//! A Unix-domain listener speaking the JSON line protocol in
//! [`turtle_core::proto`]. `turtle` (the CLI) is the client.
//!
//! # Why this module is portable (not `cfg(linux)`)
//!
//! `std::os::unix::net` exists on macOS too, and nothing here touches ALSA — so
//! unlike [`crate::control`], this module **compiles and unit-tests on the dev
//! Mac** against a real socket in a temp dir. That is deliberate: the last
//! socket-adjacent change that could only be checked by hand shipped a compile
//! error to the Pi. Only the *wiring* into the control loop stays Linux-gated.
//!
//! # Threading, and why the control loop never blocks
//!
//! The control loop ([`crate::control::run`]) polls every ~1 ms and dispatches
//! timed MIDI; if it stalls, MIDI is late. So **no socket I/O happens on it**:
//!
//! ```text
//!   listener thread ──accept──> connection thread (one per client)
//!                                  │
//!    Status  ─── reads the shared snapshot, replies directly ── never touches
//!                                  │                            the control loop
//!    verbs   ─── Sender<Command> ──┼──> control loop drains with try_recv()
//!                                  │
//!    Monitor ─── registers its stream in `subscribers`
//!                                  │
//!   control loop ── Sender<Event> ─┴──> fanout thread ──write──> subscribers
//! ```
//!
//! Both directions are `mpsc` channels, which are unbounded and never block the
//! sender. A client that connects and then stops reading can therefore stall
//! only the fanout thread, never the transport.
//!
//! `status` is answered straight from an `Arc<Mutex<Status>>` the control loop
//! republishes each iteration — so it needs no round-trip through the loop at
//! all. That mutex is touched only by the control and socket threads, never by
//! the audio RT thread, so it is not an RT-safety violation; the lock is held
//! for a clone and released.

use std::io::{self, BufRead, BufReader, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::sync::{Arc, Mutex};

use turtle_core::model::SetlistEntry;
use turtle_core::proto::{Event, Request, Response, Status};
use turtle_core::Command;

/// The status snapshot shared between the control loop (writer) and socket
/// connection threads (readers).
pub type StatusHandle = Arc<Mutex<Status>>;

/// The live server: the control loop's ends of the two channels, plus the
/// handle it needs to decide whether building monitor events is worth it.
#[derive(Debug)]
pub struct Server {
    /// Transport commands injected by socket clients. Drain with `try_recv()`.
    pub commands: Receiver<Command>,
    /// Monitor events out. Sending is non-blocking and lossless (unbounded).
    monitor_tx: Sender<Event>,
    /// How many `monitor` clients are attached, so the control loop can skip
    /// building events (which allocate a `String`/`Vec`) when nobody is
    /// watching — the common case in a show.
    subscribers: Arc<AtomicUsize>,
    /// Kept so `Drop` can unlink it.
    path: PathBuf,
}

impl Server {
    /// Is anyone running `turtle monitor`? Check before building an [`Event`];
    /// [`Server::publish`] is a no-op otherwise, but the *caller's* formatting
    /// cost is the part worth skipping.
    pub fn monitored(&self) -> bool {
        self.subscribers.load(Ordering::Relaxed) > 0
    }

    /// Hand a monitor event to the fanout thread. Never blocks. Dropping the
    /// event when no one is listening (or the fanout thread is gone) is
    /// correct: `monitor` is a debugging aid, never a source of truth.
    pub fn publish(&self, event: Event) {
        let _ = self.monitor_tx.send(event);
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Best-effort tidy-up. A SIGINT'd daemon never runs this, which is
        // exactly why `bind` below also handles a stale socket file.
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Bind the listener, handling a **stale socket file** left by a daemon that
/// was killed before it could unlink (the normal case on the Pi: Ctrl-C).
///
/// `AddrInUse` is ambiguous — it means either "another `turtled` is live" or
/// "a dead one left its file behind". Connecting tells them apart: a live
/// listener accepts, a corpse refuses.
fn bind(path: &Path) -> io::Result<UnixListener> {
    match UnixListener::bind(path) {
        Ok(listener) => Ok(listener),
        Err(e) if e.kind() == io::ErrorKind::AddrInUse => {
            if UnixStream::connect(path).is_ok() {
                return Err(io::Error::new(
                    io::ErrorKind::AddrInUse,
                    format!("another turtled is already listening on {}", path.display()),
                ));
            }
            std::fs::remove_file(path)?;
            UnixListener::bind(path)
        }
        Err(e) => Err(e),
    }
}

/// Start the control socket: bind `path`, spawn the listener and monitor-fanout
/// threads, and return the [`Server`] the control loop talks to.
///
/// The threads are detached — they live as long as the process, which for a
/// daemon whose only exit is a signal is the whole point.
pub fn start(path: &Path, status: StatusHandle, setlist: Vec<SetlistEntry>) -> io::Result<Server> {
    // A relative/hidden parent that doesn't exist yet (e.g. a systemd
    // RuntimeDirectory that hasn't been created) is a clearer error here than
    // a bare ENOENT out of bind().
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() && !parent.exists() {
            return Err(io::Error::new(
                io::ErrorKind::NotFound,
                format!("socket directory {} does not exist", parent.display()),
            ));
        }
    }

    let listener = bind(path)?;

    // Owner-only. There is a brief window between bind() and this chmod where
    // the socket sits at the process umask — the airtight fix is to set the
    // umask around bind(), which is process-global and not worth the footgun
    // here. On a single-user show box this is the right trade.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }

    let (cmd_tx, cmd_rx) = mpsc::channel::<Command>();
    let (monitor_tx, monitor_rx) = mpsc::channel::<Event>();
    let subscribers: Arc<Mutex<Vec<UnixStream>>> = Arc::new(Mutex::new(Vec::new()));
    let subscriber_count = Arc::new(AtomicUsize::new(0));
    let setlist = Arc::new(setlist);

    // Fanout thread: the only place that writes to monitor clients, so a client
    // that stops reading blocks this thread and nothing else.
    {
        let subscribers = Arc::clone(&subscribers);
        let subscriber_count = Arc::clone(&subscriber_count);
        std::thread::spawn(move || fanout(monitor_rx, subscribers, subscriber_count));
    }

    // Listener thread: accept forever, one thread per client. A show has a
    // handful of clients at most, so thread-per-connection beats the
    // complexity of a poll loop here.
    {
        let subscribers = Arc::clone(&subscribers);
        let subscriber_count = Arc::clone(&subscriber_count);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(stream) = stream else { continue };
                let status = Arc::clone(&status);
                let setlist = Arc::clone(&setlist);
                let cmd_tx = cmd_tx.clone();
                let subscribers = Arc::clone(&subscribers);
                let subscriber_count = Arc::clone(&subscriber_count);
                std::thread::spawn(move || {
                    // A client dying mid-request is routine, not an error.
                    let _ = serve_client(
                        stream,
                        &status,
                        &setlist,
                        &cmd_tx,
                        &subscribers,
                        &subscriber_count,
                    );
                });
            }
        });
    }

    Ok(Server {
        commands: cmd_rx,
        monitor_tx,
        subscribers: subscriber_count,
        path: path.to_path_buf(),
    })
}

/// Write each published event to every monitor client, dropping those that have
/// gone away (a disconnected peer surfaces as `EPIPE` on write).
fn fanout(rx: Receiver<Event>, subscribers: Arc<Mutex<Vec<UnixStream>>>, count: Arc<AtomicUsize>) {
    // `for` over a Receiver ends when every Sender is dropped — i.e. when the
    // Server (and so the daemon) is gone.
    for event in rx {
        let line = Response::Event(event).to_line();
        let mut subs = lock(&subscribers);
        // `retain` doubles as the reaper: keep only the clients we could write
        // to. `write_all` + `flush` so a client sees each line immediately
        // rather than at some buffer boundary.
        subs.retain_mut(|s| {
            s.write_all(line.as_bytes())
                .and_then(|()| s.flush())
                .is_ok()
        });
        count.store(subs.len(), Ordering::Relaxed);
    }
}

/// Read requests from one client until it disconnects.
fn serve_client(
    stream: UnixStream,
    status: &StatusHandle,
    setlist: &[SetlistEntry],
    cmd_tx: &Sender<Command>,
    subscribers: &Arc<Mutex<Vec<UnixStream>>>,
    count: &Arc<AtomicUsize>,
) -> io::Result<()> {
    // Two handles to the same socket: one buffered for reading, one for
    // writing. `try_clone` dups the fd — both refer to the same connection.
    let write_half = stream.try_clone()?;
    let mut writer = write_half;
    let reader = BufReader::new(stream);

    for line in reader.lines() {
        let line = line?;
        if line.trim().is_empty() {
            continue;
        }
        match Request::from_line(&line) {
            Err(message) => {
                writer.write_all(Response::error(message).to_line().as_bytes())?;
                writer.flush()?;
            }
            Ok(Request::Monitor) => {
                // Hand our write end to the fanout thread and stop reading:
                // `monitor` is a one-way stream from here on. The clone keeps
                // the fd open after this thread returns; the fanout thread
                // reaps it when the client hangs up.
                let mut subs = lock(subscribers);
                subs.push(writer.try_clone()?);
                count.store(subs.len(), Ordering::Relaxed);
                return Ok(());
            }
            Ok(req) => {
                let response = handle_request(req, status, setlist, cmd_tx);
                writer.write_all(response.to_line().as_bytes())?;
                writer.flush()?;
            }
        }
    }
    Ok(())
}

/// Serve one non-`Monitor` request.
///
/// Pure enough to unit-test directly: everything it needs is passed in, and the
/// only side effect is a `Command` on the channel.
fn handle_request(
    req: Request,
    status: &StatusHandle,
    setlist: &[SetlistEntry],
    cmd_tx: &Sender<Command>,
) -> Response {
    let command = match req {
        Request::Status => return Response::Status(lock(status).clone()),
        Request::Monitor => unreachable!("handled by serve_client, which owns the stream"),
        Request::Start => Command::Start,
        Request::Stop => Command::Stop,
        Request::Next => Command::Next,
        Request::Prev => Command::Prev,
        Request::Panic => Command::Panic,
        // The transport selects by Program Change number, but a human (and the
        // spec's `turtle arm <song>`) says the name — so resolve it here, where
        // we can give a useful error, rather than pushing a bad pc at the
        // transport and having it silently do nothing.
        Request::Arm { song } => match setlist.iter().find(|e| e.song == song) {
            Some(entry) => Command::Select(entry.pc),
            None => {
                let known: Vec<&str> = setlist.iter().map(|e| e.song.as_str()).collect();
                return Response::error(format!(
                    "no song '{song}' in the setlist (have: {})",
                    known.join(", ")
                ));
            }
        },
    };
    // Fire-and-forget: `Ok` means the control loop has it, not that it has run.
    // A send error means the loop is gone, i.e. the daemon is shutting down.
    match cmd_tx.send(command) {
        Ok(()) => Response::Ok,
        Err(_) => Response::error("daemon is shutting down"),
    }
}

/// Lock past poisoning.
///
/// A `Mutex` is poisoned if a thread panicked while holding it. Here the guarded
/// data is a status snapshot and a subscriber list — neither can be left in a
/// state that makes a later reader unsound — so recovering the data beats taking
/// the whole control socket down with an `unwrap`. `pub(crate)` because the
/// control loop republishes the snapshot through the same door.
pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::{Duration, Instant};
    use turtle_core::State;

    /// A socket path in a fresh directory, removed when the test ends.
    ///
    /// Deliberately short and under `/tmp` rather than `std::env::temp_dir()`:
    /// a Unix socket path must fit in `sockaddr_un.sun_path` (~104 bytes on
    /// macOS), and macOS's `TMPDIR` (`/var/folders/…`) alone eats most of that.
    struct TempSock(PathBuf);

    impl TempSock {
        fn new() -> Self {
            // A process-wide counter keeps parallel tests from colliding
            // without spending path budget on a thread-id debug string.
            static N: AtomicUsize = AtomicUsize::new(0);
            let dir = PathBuf::from(format!(
                "/tmp/turtled-t{}-{}",
                std::process::id(),
                N.fetch_add(1, Ordering::Relaxed)
            ));
            std::fs::create_dir_all(&dir).unwrap();
            TempSock(dir.join("s"))
        }
        fn path(&self) -> &Path {
            &self.0
        }
    }

    impl Drop for TempSock {
        fn drop(&mut self) {
            if let Some(dir) = self.0.parent() {
                let _ = std::fs::remove_dir_all(dir);
            }
        }
    }

    fn test_status() -> Status {
        Status {
            show: "Tone".into(),
            state: State::Armed,
            song: Some("tone".into()),
            armed_next: None,
            position_s: 0.0,
            duration_s: 10.0,
        }
    }

    fn test_setlist() -> Vec<SetlistEntry> {
        vec![
            SetlistEntry {
                pc: 0,
                song: "tone".into(),
            },
            SetlistEntry {
                pc: 1,
                song: "second".into(),
            },
        ]
    }

    /// Send one request over a fresh connection and read the one-line reply.
    fn round_trip(path: &Path, req: Request) -> Response {
        let mut stream = UnixStream::connect(path).unwrap();
        stream.write_all(req.to_line().as_bytes()).unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        Response::from_line(&line).unwrap()
    }

    #[test]
    fn status_is_served_from_the_snapshot() {
        let sock = TempSock::new();
        let path = sock.path();
        let status: StatusHandle = Arc::new(Mutex::new(test_status()));
        let server = start(path, Arc::clone(&status), test_setlist()).unwrap();

        assert_eq!(
            round_trip(path, Request::Status),
            Response::Status(test_status())
        );

        // The control loop republishes; the next status must reflect it.
        lock(&status).state = State::Playing;
        lock(&status).position_s = 2.5;
        let Response::Status(s) = round_trip(path, Request::Status) else {
            panic!("expected a status reply")
        };
        assert_eq!(s.state, State::Playing);
        assert_eq!(s.position_s, 2.5);
        drop(server);
    }

    #[test]
    fn transport_verbs_reach_the_control_loop() {
        let sock = TempSock::new();
        let path = sock.path();
        let server = start(path, Arc::new(Mutex::new(test_status())), test_setlist()).unwrap();

        for (req, want) in [
            (Request::Start, Command::Start),
            (Request::Stop, Command::Stop),
            (Request::Next, Command::Next),
            (Request::Prev, Command::Prev),
            (Request::Panic, Command::Panic),
        ] {
            assert_eq!(round_trip(path, req.clone()), Response::Ok, "{req:?}");
            assert_eq!(
                server
                    .commands
                    .recv_timeout(Duration::from_secs(1))
                    .unwrap(),
                want
            );
        }
    }

    /// `arm <name>` must resolve to the setlist entry's *pc*, not assume the
    /// name's position — the two are independent in show.toml.
    #[test]
    fn arm_resolves_a_song_name_to_its_program_change() {
        let sock = TempSock::new();
        let path = sock.path();
        let server = start(path, Arc::new(Mutex::new(test_status())), test_setlist()).unwrap();

        assert_eq!(
            round_trip(
                path,
                Request::Arm {
                    song: "second".into()
                }
            ),
            Response::Ok
        );
        assert_eq!(
            server
                .commands
                .recv_timeout(Duration::from_secs(1))
                .unwrap(),
            Command::Select(1)
        );
    }

    #[test]
    fn arming_an_unknown_song_errors_and_lists_the_real_ones() {
        let sock = TempSock::new();
        let path = sock.path();
        let server = start(path, Arc::new(Mutex::new(test_status())), test_setlist()).unwrap();

        let Response::Error { message } = round_trip(
            path,
            Request::Arm {
                song: "nope".into(),
            },
        ) else {
            panic!("expected an error reply")
        };
        assert!(message.contains("nope"), "{message}");
        assert!(
            message.contains("tone") && message.contains("second"),
            "{message}"
        );
        // ...and nothing was injected into the transport.
        assert!(server.commands.try_recv().is_err());
    }

    #[test]
    fn a_garbage_line_gets_an_error_and_the_connection_survives() {
        let sock = TempSock::new();
        let path = sock.path();
        let _server = start(path, Arc::new(Mutex::new(test_status())), test_setlist()).unwrap();

        let mut stream = UnixStream::connect(path).unwrap();
        stream.write_all(b"not json\n").unwrap();
        stream.flush().unwrap();
        let mut reader = BufReader::new(stream.try_clone().unwrap());
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            Response::from_line(&line).unwrap(),
            Response::Error { .. }
        ));

        // Same connection still serves a good request afterwards.
        stream
            .write_all(Request::Status.to_line().as_bytes())
            .unwrap();
        stream.flush().unwrap();
        line.clear();
        reader.read_line(&mut line).unwrap();
        assert!(matches!(
            Response::from_line(&line).unwrap(),
            Response::Status(_)
        ));
    }

    #[test]
    fn monitor_streams_published_events() {
        let sock = TempSock::new();
        let path = sock.path();
        let server = start(path, Arc::new(Mutex::new(test_status())), test_setlist()).unwrap();

        // Nobody watching yet: the loop should be able to skip the work.
        assert!(!server.monitored());

        let mut stream = UnixStream::connect(path).unwrap();
        stream
            .write_all(Request::Monitor.to_line().as_bytes())
            .unwrap();
        stream.flush().unwrap();

        // Registration happens on the connection thread; wait for it to land
        // rather than sleeping a fixed amount and hoping.
        let deadline = Instant::now() + Duration::from_secs(2);
        while !server.monitored() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(server.monitored(), "monitor client never registered");

        server.publish(Event::Command {
            wall_s: 1.0,
            source: turtle_core::proto::Source::Midi,
            command: "Start".into(),
            state: State::Playing,
        });

        let mut reader = BufReader::new(stream);
        let mut line = String::new();
        reader.read_line(&mut line).unwrap();
        let Response::Event(Event::Command {
            command, source, ..
        }) = Response::from_line(&line).unwrap()
        else {
            panic!("expected a command event, got {line}")
        };
        assert_eq!(command, "Start");
        assert_eq!(source, turtle_core::proto::Source::Midi);
    }

    /// A monitor client that hangs up must not wedge the fanout thread or the
    /// subscriber list — the next publish reaps it.
    #[test]
    fn a_departed_monitor_client_is_reaped() {
        let sock = TempSock::new();
        let path = sock.path();
        let server = start(path, Arc::new(Mutex::new(test_status())), test_setlist()).unwrap();

        let stream = UnixStream::connect(path).unwrap();
        let mut s = stream.try_clone().unwrap();
        s.write_all(Request::Monitor.to_line().as_bytes()).unwrap();
        s.flush().unwrap();
        let deadline = Instant::now() + Duration::from_secs(2);
        while !server.monitored() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(5));
        }
        assert!(server.monitored());

        drop(s);
        drop(stream);

        // Publishing to a hung-up peer eventually reaps it. The socket buffer
        // absorbs the first writes, so publish until the count drops.
        let deadline = Instant::now() + Duration::from_secs(5);
        while server.monitored() && Instant::now() < deadline {
            server.publish(Event::Command {
                wall_s: 0.0,
                source: turtle_core::proto::Source::Internal,
                command: "Stop".into(),
                state: State::Stopped,
            });
            std::thread::sleep(Duration::from_millis(10));
        }
        assert!(!server.monitored(), "dead monitor client was never reaped");
    }

    /// The Pi's normal exit is Ctrl-C, which never runs `Drop` — so a stale
    /// socket file must not stop the next start.
    #[test]
    fn a_stale_socket_file_is_replaced() {
        let sock = TempSock::new();
        let path = sock.path();
        // Simulate a killed daemon: a socket file with nothing listening.
        drop(UnixListener::bind(path).unwrap());
        assert!(path.exists());

        let server = start(path, Arc::new(Mutex::new(test_status())), test_setlist()).unwrap();
        assert_eq!(
            round_trip(path, Request::Status),
            Response::Status(test_status())
        );
        drop(server);
    }

    /// ...but a *live* daemon must not be silently hijacked.
    #[test]
    fn a_live_listener_is_not_stolen() {
        let sock = TempSock::new();
        let path = sock.path();
        let _first = start(path, Arc::new(Mutex::new(test_status())), test_setlist()).unwrap();

        let err = start(path, Arc::new(Mutex::new(test_status())), test_setlist()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::AddrInUse);
        assert!(err.to_string().contains("already listening"), "{err}");
    }

    #[test]
    fn the_socket_is_owner_only() {
        use std::os::unix::fs::PermissionsExt;
        let sock = TempSock::new();
        let path = sock.path();
        let _server = start(path, Arc::new(Mutex::new(test_status())), test_setlist()).unwrap();
        let mode = std::fs::metadata(path).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "socket mode was {:o}", mode & 0o777);
    }
}
