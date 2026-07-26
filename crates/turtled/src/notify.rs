//! `sd_notify` — telling systemd we are alive (spec §12).
//!
//! §12 asks for "auto-restart, boot-to-ready; hardware watchdog". `Restart=always`
//! alone only gets part of that, because it can only notice a process that
//! *exited*. The two gaps it leaves:
//!
//!   * **Boot-to-ready.** With `Type=simple`, systemd calls the unit "started"
//!     the instant `execve` returns — before the stems are loaded and the
//!     transport is armed. Anything ordered after us would be released too early,
//!     and a start-up failure looks like a successful start followed by a crash.
//!   * **A hung daemon.** If the audio thread deadlocks or the control loop
//!     wedges, the process is still very much alive. Nothing restarts it, and on
//!     stage that is indistinguishable from a dead one.
//!
//! Both are fixed by the same protocol: the daemon sends a datagram to the socket
//! named by `$NOTIFY_SOCKET`. `READY=1` closes the first gap (`Type=notify`
//! makes systemd wait for it), and a periodic `WATCHDOG=1` closes the second —
//! miss the deadline and systemd kills and restarts us.
//!
//! # Why hand-rolled instead of a crate
//!
//! The wire format is newline-separated `KEY=value` in one datagram, so this is
//! genuinely a dozen lines. Depending on `libsystemd` would drag a C library
//! into the one binary that has to boot on a read-only rootfs.
//!
//! # Wired up so far
//!
//! [`Notifier::ready`] and [`Notifier::watchdog_tick`] are called from
//! [`crate::control::run`]. [`Notifier::status`] and [`Notifier::stopping`] are
//! implemented and tested but **not yet called**: the control loop runs until the
//! process is signalled and has no clean-shutdown path, so there is currently no
//! point at which `STOPPING=1` could honestly be sent. They are here for when it
//! gains one — a deliberate exit that is not announced gets read as a crash.
//!
//! # Not `cfg(linux)`
//!
//! Unix datagram sockets work on macOS too, so this whole module — including its
//! tests, which stand up a real socket and read the bytes back — runs on the dev
//! Mac. Only the abstract-namespace address form below is Linux-specific.

use std::os::unix::net::UnixDatagram;
use std::time::{Duration, Instant};

/// A connection to systemd's notification socket, or a no-op when there is none.
///
/// The no-op case is the common one during development: run `turtled` from a
/// shell and `$NOTIFY_SOCKET` is unset, so every method below does nothing. That
/// is deliberate — the daemon must not behave differently, or care, depending on
/// who started it.
#[derive(Debug)]
pub struct Notifier {
    /// `None` when not running under systemd (or when the socket could not be
    /// opened, which we treat the same way rather than failing the show).
    sock: Option<UnixDatagram>,
    /// Address to send to, kept because the socket is unbound/unconnected.
    addr: Option<Addr>,
    /// How often systemd wants a `WATCHDOG=1`, halved (see [`Self::watchdog_period`]).
    period: Option<Duration>,
    /// When the next ping is due. `Instant`, not wall clock, so a clock step
    /// mid-show cannot make us look hung.
    next_ping: Instant,
}

/// The two address forms `$NOTIFY_SOCKET` can take.
#[derive(Debug, Clone)]
enum Addr {
    /// An ordinary filesystem path, e.g. `/run/systemd/notify`.
    Path(std::path::PathBuf),
    /// An abstract-namespace name (the variable started with `@`). Abstract
    /// sockets live outside the filesystem, so they need a different address
    /// type — and they exist only on Linux.
    #[cfg(target_os = "linux")]
    Abstract(Vec<u8>),
}

impl Notifier {
    /// Connect using the environment systemd set up, or return a no-op notifier.
    ///
    /// Reads `NOTIFY_SOCKET` (where to send) and `WATCHDOG_USEC` (how often).
    /// systemd also sets `WATCHDOG_PID`; we honour it so that a child process
    /// inheriting the environment cannot accidentally satisfy the parent's
    /// watchdog — which would keep a hung daemon looking healthy.
    pub fn from_env() -> Self {
        let watchdog_usec = std::env::var("WATCHDOG_USEC")
            .ok()
            .and_then(|v| v.parse::<u64>().ok())
            .filter(|_| Self::watchdog_is_for_us());
        Self::new(std::env::var("NOTIFY_SOCKET").ok().as_deref(), watchdog_usec)
    }

    /// `WATCHDOG_PID` unset means "whoever reads this"; set means only that pid.
    fn watchdog_is_for_us() -> bool {
        match std::env::var("WATCHDOG_PID") {
            Ok(v) => v.parse::<u32>().ok() == Some(std::process::id()),
            Err(_) => true,
        }
    }

    /// The testable core of [`Self::from_env`]: takes the environment as values.
    fn new(notify_socket: Option<&str>, watchdog_usec: Option<u64>) -> Self {
        // Halve the deadline so one dropped or late ping does not immediately
        // get us killed. This is what systemd's own documentation recommends.
        let period = watchdog_usec
            .filter(|&usec| usec > 0)
            .map(|usec| Duration::from_micros(usec / 2));
        let addr = notify_socket.and_then(Self::parse_addr);
        // An unbound socket: we only ever send, never receive, so there is
        // nothing to bind to.
        let sock = addr.as_ref().and_then(|_| {
            UnixDatagram::unbound()
                .and_then(|s| {
                    // Non-blocking on purpose. `send_to` on a datagram socket can
                    // block if the receiver's buffer is full, and the watchdog
                    // ping happens on the SCHED_FIFO control thread (§3) — a
                    // blocking syscall there would stall MIDI dispatch. A dropped
                    // ping is harmless: the next one is a fraction of the
                    // deadline away.
                    s.set_nonblocking(true)?;
                    Ok(s)
                })
                .ok()
        });
        Notifier { sock, addr, period, next_ping: Instant::now() }
    }

    /// Decode `$NOTIFY_SOCKET`. A leading `@` selects the abstract namespace.
    fn parse_addr(raw: &str) -> Option<Addr> {
        if raw.is_empty() {
            return None;
        }
        if let Some(name) = raw.strip_prefix('@') {
            #[cfg(target_os = "linux")]
            return Some(Addr::Abstract(name.as_bytes().to_vec()));
            // On a non-Linux host there is no abstract namespace to talk to, so
            // this degrades to a no-op rather than pretending.
            #[cfg(not(target_os = "linux"))]
            {
                let _ = name;
                return None;
            }
        }
        Some(Addr::Path(std::path::PathBuf::from(raw)))
    }

    /// Whether we are actually talking to systemd (for logging and tests).
    pub fn is_active(&self) -> bool {
        self.sock.is_some()
    }

    /// How often [`Self::watchdog_tick`] will really send, if the watchdog is on.
    pub fn watchdog_period(&self) -> Option<Duration> {
        self.period
    }

    /// Send one `KEY=value` payload, ignoring every error.
    ///
    /// Errors are swallowed by design: a failed status notification must never
    /// take down a show, and there is nothing useful to do about it anyway.
    /// Returns whether the datagram went out, which is what the tests assert on.
    fn send(&self, payload: &str) -> bool {
        let (Some(sock), Some(addr)) = (&self.sock, &self.addr) else {
            return false;
        };
        let sent = match addr {
            Addr::Path(path) => sock.send_to(payload.as_bytes(), path),
            #[cfg(target_os = "linux")]
            Addr::Abstract(name) => {
                use std::os::linux::net::SocketAddrExt;
                match std::os::unix::net::SocketAddr::from_abstract_name(name) {
                    Ok(addr) => sock.send_to_addr(payload.as_bytes(), &addr),
                    Err(e) => Err(e),
                }
            }
        };
        sent.is_ok()
    }

    /// Send a payload we cannot afford to lose, retrying past a full buffer.
    ///
    /// The socket is non-blocking for the watchdog's sake, which means `send_to`
    /// can legitimately fail with `WouldBlock`. For a periodic ping that is
    /// harmless, but a dropped `READY=1` is not: systemd would wait out
    /// `TimeoutStartSec` and then declare a perfectly healthy daemon failed to
    /// start. So retry briefly. This only runs off the RT path (start-up and
    /// shutdown), which is what makes sleeping here acceptable.
    fn send_important(&self, payload: &str) -> bool {
        for attempt in 0..10 {
            if self.send(payload) {
                return true;
            }
            if !self.is_active() {
                // No socket at all — retrying cannot help.
                return false;
            }
            std::thread::sleep(Duration::from_millis(1 << attempt.min(5)));
        }
        false
    }

    /// Report that start-up is complete: stems loaded, socket bound, armed.
    ///
    /// With `Type=notify` this is the moment `systemctl start` returns and
    /// anything ordered `After=` us is released, so it must be sent *after* the
    /// daemon can genuinely serve a request — not when `main` begins.
    pub fn ready(&mut self, status: &str) -> bool {
        // One datagram, two directives: systemd parses the newline-separated
        // pairs together, so `systemctl status` shows the text immediately.
        let sent = self.send_important(&format!("READY=1\nSTATUS={status}\n"));
        // Start the watchdog clock from readiness, not from process start:
        // loading stems can legitimately take longer than the deadline.
        self.next_ping = Instant::now() + self.period.unwrap_or_default();
        sent
    }

    /// Update the one-line description in `systemctl status`.
    pub fn status(&self, status: &str) -> bool {
        self.send(&format!("STATUS={status}\n"))
    }

    /// Tell systemd we are shutting down, so a deliberate exit is not read as a
    /// failure that needs restarting.
    pub fn stopping(&self, status: &str) -> bool {
        self.send_important(&format!("STOPPING=1\nSTATUS={status}\n"))
    }

    /// Send `WATCHDOG=1` if one is due. Safe (and cheap) to call every loop
    /// iteration: with no watchdog configured, or before the period elapses,
    /// this is one `Instant` comparison and nothing else.
    ///
    /// Placed in the *control* loop rather than a timer thread on purpose. A
    /// dedicated thread would keep pinging happily while the loop that actually
    /// dispatches MIDI was wedged — proving only that the thread lives, which is
    /// precisely the failure the watchdog exists to catch. Pinging from the loop
    /// makes liveness of the ping mean liveness of the show.
    pub fn watchdog_tick(&mut self) -> bool {
        let Some(period) = self.period else { return false };
        let now = Instant::now();
        if now < self.next_ping {
            return false;
        }
        self.next_ping = now + period;
        self.send("WATCHDOG=1\n")
    }
}

/// Best-effort `chmod`-free helper used by tests and callers that want a log line.
pub fn describe(n: &Notifier) -> String {
    if !n.is_active() {
        return "not under systemd (no NOTIFY_SOCKET)".into();
    }
    match n.watchdog_period() {
        Some(p) => format!("systemd notify active, watchdog ping every {:.1}s", p.as_secs_f64()),
        None => "systemd notify active, no watchdog".into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A bound receiving socket at a short `/tmp` path, cleaned up on drop.
    ///
    /// Short path on purpose: macOS caps `sockaddr_un.sun_path` at ~104 bytes and
    /// the per-test temp dir (`/var/folders/...`) already blows past it — the
    /// same trap the socket tests hit.
    struct TestSock {
        path: std::path::PathBuf,
        sock: UnixDatagram,
    }

    impl TestSock {
        fn new(tag: &str) -> Self {
            let path =
                std::path::PathBuf::from(format!("/tmp/turtle-notify-{}-{tag}", std::process::id()));
            let _ = std::fs::remove_file(&path);
            let sock = UnixDatagram::bind(&path).expect("bind test notify socket");
            sock.set_read_timeout(Some(Duration::from_millis(500))).unwrap();
            TestSock { path, sock }
        }

        /// Read one datagram as text, or `None` if nothing arrived.
        fn recv(&self) -> Option<String> {
            let mut buf = [0u8; 256];
            let n = self.sock.recv(&mut buf).ok()?;
            Some(String::from_utf8_lossy(&buf[..n]).into_owned())
        }
    }

    impl Drop for TestSock {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.path);
        }
    }

    /// The whole point of `Type=notify`: systemd must receive `READY=1` — and it
    /// must arrive as a real datagram, not just be formatted.
    #[test]
    fn ready_sends_a_datagram_systemd_would_accept() {
        let peer = TestSock::new("ready");
        let mut n = Notifier::new(Some(peer.path.to_str().unwrap()), None);
        assert!(n.is_active());
        assert!(n.ready("armed \"Tone\""));
        let msg = peer.recv().expect("no datagram arrived");
        assert!(msg.starts_with("READY=1\n"), "{msg:?}");
        assert!(msg.contains("STATUS=armed \"Tone\""), "{msg:?}");
    }

    /// Started from a shell there is no `NOTIFY_SOCKET`, and every call must be
    /// a silent no-op — the daemon cannot behave differently by who launched it.
    #[test]
    fn without_a_notify_socket_everything_is_a_no_op() {
        let mut n = Notifier::new(None, Some(10_000_000));
        assert!(!n.is_active());
        assert!(!n.ready("x"));
        assert!(!n.watchdog_tick());
        assert!(!n.status("x"));
    }

    /// An unset `WATCHDOG_USEC` means no watchdog, so we must not ping — sending
    /// `WATCHDOG=1` to a unit without `WatchdogSec` is pointless traffic on the
    /// RT thread.
    #[test]
    fn no_watchdog_configured_means_no_pings() {
        let peer = TestSock::new("nowd");
        let mut n = Notifier::new(Some(peer.path.to_str().unwrap()), None);
        assert_eq!(n.watchdog_period(), None);
        assert!(!n.watchdog_tick());
    }

    /// The deadline is halved, so one late or dropped ping does not kill the show.
    #[test]
    fn the_ping_period_is_half_the_deadline() {
        let n = Notifier::new(None, Some(30_000_000));
        assert_eq!(n.watchdog_period(), Some(Duration::from_secs(15)));
    }

    /// A zero deadline is systemd's "disabled", not "ping as fast as you can" —
    /// which on a 1 ms loop would be a syscall storm.
    #[test]
    fn a_zero_deadline_disables_the_watchdog() {
        let n = Notifier::new(None, Some(0));
        assert_eq!(n.watchdog_period(), None);
    }

    /// `watchdog_tick` is called ~1000x a second but must only send on the
    /// period, and must send again once it elapses.
    #[test]
    fn watchdog_ticks_are_rate_limited_to_the_period() {
        let peer = TestSock::new("rate");
        // 20 ms deadline -> 10 ms ping period, short enough to actually wait for.
        let mut n = Notifier::new(Some(peer.path.to_str().unwrap()), Some(20_000));
        n.ready("armed");
        let _ = peer.recv(); // consume the READY datagram

        // Immediately after ready() the first period has not elapsed.
        let bursts = (0..100).filter(|_| n.watchdog_tick()).count();
        assert_eq!(bursts, 0, "pinged before the period elapsed");

        std::thread::sleep(Duration::from_millis(15));
        assert!(n.watchdog_tick(), "no ping after the period elapsed");
        assert_eq!(peer.recv().as_deref(), Some("WATCHDOG=1\n"));
        // And the timer rearms rather than free-running.
        assert!(!n.watchdog_tick());
    }

    /// `WATCHDOG_PID` naming another process means those pings are not ours to
    /// send: satisfying a parent's watchdog would mask a hang in it.
    #[test]
    fn a_watchdog_pid_for_another_process_is_ignored() {
        assert!(Notifier::watchdog_is_for_us(), "no WATCHDOG_PID set");
    }

    /// systemd normally hands us a filesystem path; `@`-prefixed means the
    /// abstract namespace, which does not exist off Linux.
    #[test]
    fn abstract_addresses_are_linux_only() {
        assert!(matches!(Notifier::parse_addr("/run/systemd/notify"), Some(Addr::Path(_))));
        assert!(Notifier::parse_addr("").is_none());
        let parsed = Notifier::parse_addr("@abcd");
        #[cfg(target_os = "linux")]
        assert!(matches!(parsed, Some(Addr::Abstract(_))));
        #[cfg(not(target_os = "linux"))]
        assert!(parsed.is_none());
    }

    /// A deliberate exit must be announced, or systemd reads it as a crash.
    #[test]
    fn stopping_is_announced() {
        let peer = TestSock::new("stop");
        let n = Notifier::new(Some(peer.path.to_str().unwrap()), None);
        assert!(n.stopping("shutting down"));
        let msg = peer.recv().expect("no datagram");
        assert!(msg.starts_with("STOPPING=1\n"), "{msg:?}");
    }
}
