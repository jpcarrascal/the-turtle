//! Waiting for devices to appear at startup (spec §12).
//!
//! # The 1697 restarts
//!
//! `turtled` treated a failed device open as fatal, and the unit sets
//! `Restart=always` with no ceiling. Together those turn a device that is merely
//! *late* into an unbounded crash-loop at one attempt per second. On the dev rig
//! that counter reached **1697** — mostly from a misconfigured MIDI port, but the
//! journal also caught the transient case:
//!
//! ```text
//! 13:09:47  Cannot get card index for L6 ... No such device (19)
//! 13:09:48  Scheduled restart job, restart counter is at 1697.
//! 13:09:48  armed "Tone Test" on hw:L6            <- succeeded one second later
//! ```
//!
//! Two ways that happens for real:
//!
//!   * **At boot**, USB enumeration races the service. `After=sound.target` says
//!     *some* sound device exists, not that the USB interface has appeared.
//!   * **On restart**, the outgoing process may not have released the device yet.
//!
//! Crash-looping does eventually work, which is why it went unnoticed. But it
//! floods the journal, and each loop is a failed `READY=1`, so `systemctl start`
//! reports a failure for a daemon that is about to be fine.
//!
//! Waiting a few seconds instead costs nothing and turns that into one clean
//! start. §12's failure policy is "never refuse to play over something
//! recoverable", and a device that is one second late is exactly that.
//!
//! # Why retry every error, not just the plausible ones
//!
//! It is tempting to retry only `ENODEV`/`EBUSY` and fail fast on the rest. But at
//! startup almost anything can be a not-ready-yet condition — udev may still be
//! applying permissions, so even `EACCES` can be transient. The window is bounded,
//! the first failure is logged immediately so the delay is never mysterious, and
//! the error reported at the end is the *last* one, so a genuine misconfiguration
//! still surfaces with its real message.
//!
//! # What this looks like in the journal
//!
//! One line from us, however many attempts it takes:
//!
//! ```text
//! waiting up to 15s for audio device 'hw:L6': ... 'No such device (19)'
//! ```
//!
//! plus one alsa-lib diagnostic per attempt, which is why [`INTERVAL`] is a whole
//! second. A device that is merely late therefore costs a couple of lines; a device
//! that is genuinely absent is loud, which is correct.

use std::fmt::Display;
use std::time::{Duration, Instant};

/// How long to wait for a device before giving up (§12). Chosen to comfortably
/// cover USB enumeration at boot while staying well inside systemd's default
/// 90-second `TimeoutStartSec`.
pub const DEFAULT_WAIT: Duration = Duration::from_secs(15);

/// How often to retry.
///
/// One second rather than something snappier for a reason that only shows up in
/// practice: **alsa-lib prints its own diagnostic on every failed open**, which we
/// cannot suppress from here, so each attempt costs a line in the journal. At
/// 500 ms a 15-second wait produced 31 of them. Retry latency is irrelevant at boot
/// — nothing is waiting on us but systemd — so fewer, quieter attempts is the
/// better trade.
const INTERVAL: Duration = Duration::from_secs(1);

/// Try `open` until it succeeds or `total` elapses; return the last error.
///
/// `what` names the thing being waited for, and `on_wait` is called **once**, on
/// the first failure, with a human-readable message. That callback exists so the
/// caller can both log it and push it into `systemd`'s status line without this
/// module knowing about either.
///
/// Always attempts at least once, so `total = 0` means "no waiting" rather than
/// "never try" — that is what makes `--wait-devices 0` the old behaviour exactly.
pub fn open_with_retry<T, E: Display>(
    what: &str,
    total: Duration,
    mut open: impl FnMut() -> Result<T, E>,
    mut on_wait: impl FnMut(&str),
) -> Result<T, E> {
    let deadline = Instant::now() + total;
    let mut announced = false;

    loop {
        match open() {
            Ok(v) => return Ok(v),
            Err(e) => {
                // Past the deadline: report the most recent error, which is the
                // one that describes the real problem.
                if Instant::now() >= deadline {
                    return Err(e);
                }
                if !announced {
                    announced = true;
                    on_wait(&format!(
                        "waiting up to {}s for {what}: {e}",
                        total.as_secs()
                    ));
                }
                // Never sleep past the deadline — otherwise a long interval could
                // overshoot a short window and skip the final attempt.
                let left = deadline.saturating_duration_since(Instant::now());
                std::thread::sleep(INTERVAL.min(left));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::Cell;

    /// The common case must not be slowed down at all.
    #[test]
    fn a_device_that_is_already_there_opens_on_the_first_try() {
        let calls = Cell::new(0);
        let mut waited = false;
        let r: Result<&str, String> = open_with_retry(
            "audio",
            DEFAULT_WAIT,
            || {
                calls.set(calls.get() + 1);
                Ok("device")
            },
            |_| waited = true,
        );
        assert_eq!(r.unwrap(), "device");
        assert_eq!(calls.get(), 1, "must not retry a success");
        assert!(!waited, "must not announce a wait that did not happen");
    }

    /// The actual bug: a device that is late must be picked up rather than
    /// becoming a crash-loop.
    #[test]
    fn a_late_device_is_waited_for_and_then_opened() {
        let calls = Cell::new(0);
        let mut announcements = Vec::new();
        let r: Result<&str, String> = open_with_retry(
            "audio device hw:L6",
            Duration::from_secs(5),
            || {
                calls.set(calls.get() + 1);
                if calls.get() < 3 {
                    Err("No such device (19)".to_string())
                } else {
                    Ok("device")
                }
            },
            |m| announcements.push(m.to_string()),
        );
        assert_eq!(r.unwrap(), "device");
        assert_eq!(calls.get(), 3);
        // Announced exactly once, however many attempts it took — one line in the
        // journal, not one per retry.
        assert_eq!(announcements.len(), 1, "{announcements:?}");
        assert!(announcements[0].contains("hw:L6"), "{announcements:?}");
        assert!(announcements[0].contains("No such device"), "{announcements:?}");
    }

    /// A genuinely absent device must still fail, reporting the real error rather
    /// than a generic timeout — the last error is the informative one.
    #[test]
    fn a_missing_device_gives_up_and_reports_the_underlying_error() {
        let start = Instant::now();
        let r: Result<(), String> = open_with_retry(
            "audio",
            Duration::from_millis(600),
            || Err("No such device (19)".to_string()),
            |_| {},
        );
        assert_eq!(r.unwrap_err(), "No such device (19)");
        // Bounded: it gave up rather than looping forever.
        assert!(start.elapsed() < Duration::from_secs(5), "took {:?}", start.elapsed());
        assert!(start.elapsed() >= Duration::from_millis(500), "gave up too early");
    }

    /// `--wait-devices 0` must restore the exact pre-existing behaviour: one
    /// attempt, no sleeping.
    #[test]
    fn a_zero_window_tries_once_and_fails_immediately() {
        let calls = Cell::new(0);
        let start = Instant::now();
        let r: Result<(), String> = open_with_retry(
            "audio",
            Duration::ZERO,
            || {
                calls.set(calls.get() + 1);
                Err("nope".to_string())
            },
            |_| {},
        );
        assert!(r.is_err());
        assert_eq!(calls.get(), 1, "zero must still mean one attempt");
        assert!(start.elapsed() < Duration::from_millis(200), "must not sleep");
    }
}
