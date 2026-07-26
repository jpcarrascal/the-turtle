//! The Linux-only half of `turtle doctor`: ALSA devices and RT limits (§10).
//!
//! Split from [`crate::doctor`] so that module stays portable and unit-testable
//! on the dev Mac. Every function here has a non-Linux twin returning
//! [`Level::Unknown`] rather than a guess — a preflight that claims a device is
//! fine on a host that cannot see devices is worse than one that admits it.
//!
//! # Why this does not reuse `turtled`'s ALSA code
//!
//! It looks like duplication and is not. `turtled`'s `AlsaAudio::open` opens a
//! device *for use*: one attempt, succeed or fail. Diagnosis needs different
//! behaviour — distinguish "missing" from "present but wrong rate" from "present
//! but busy", and enumerate what *is* available so the message can suggest a fix.
//! Those are opposite goals, and `turtled` is a separate binary whose modules are
//! private, so sharing would mean restructuring it into a library for the benefit
//! of its diagnostics.
//!
//! # The one false alarm worth designing around
//!
//! If `turtled` is already running it holds the audio device, so doctor's open
//! attempt fails with `EBUSY`. Reporting that as "device broken" would be exactly
//! wrong — it means the device works *and is in use by the show*. It is reported
//! as such below.

use crate::doctor::Check;
use turtle_core::model::Destination;

// ---------------------------------------------------------------------------
// Linux
// ---------------------------------------------------------------------------

/// Enumerate ALSA device names of a given kind (`"pcm"` or `"rawmidi"`).
///
/// This is what `aplay -l` / `amidi -l` show, via the same hint API.
#[cfg(target_os = "linux")]
fn available(kind: &str) -> Vec<String> {
    let mut out = Vec::new();
    if let Ok(hints) = alsa::device_name::HintIter::new_str(None, kind) {
        for h in hints {
            if let Some(name) = h.name {
                out.push(name);
            }
        }
    }
    out
}

/// Is the show's audio device present, and does it accept the show's rate?
///
/// The rate is checked by actually configuring it rather than by reading a
/// capability table: the HX Stomp is 48 kHz-only, and "the driver advertises it"
/// and "the driver accepts it" are not always the same claim.
#[cfg(target_os = "linux")]
pub fn check_audio(device: &str, rate: u32) -> Vec<Check> {
    use alsa::pcm::PCM;
    use alsa::Direction;

    let mut checks = Vec::new();

    match PCM::new(device, Direction::Playback, false) {
        Ok(pcm) => {
            checks.push(Check::ok(format!("device \"{device}\" opens")));
            // A fresh hw_params from the device, then ask for the show's rate.
            match alsa::pcm::HwParams::any(&pcm).and_then(|hw| {
                hw.set_rate(rate, alsa::ValueOr::Nearest)?;
                let got = hw.get_rate()?;
                Ok(got)
            }) {
                Ok(got) if got == rate => {
                    checks.push(Check::ok(format!("supports {rate} Hz")))
                }
                // `Nearest` means ALSA may hand back a different rate rather than
                // refusing — silently playing at the wrong speed is the failure
                // this catches.
                Ok(got) => checks.push(Check::fail(
                    format!("device wants {got} Hz, show is {rate} Hz"),
                    "set [show] playback_rate to the device's rate and re-convert the stems",
                )),
                Err(e) => checks.push(Check::fail(
                    format!("device rejected {rate} Hz: {e}"),
                    "check [show] playback_rate against what the interface supports",
                )),
            }
        }
        // Compared numerically against `libc` rather than via the `alsa` crate's
        // errno type, whose path has moved between releases.
        Err(e) if e.errno() == libc::EBUSY => {
            // The good case that looks like a bad one — see the module docs.
            checks.push(Check::ok(format!(
                "device \"{device}\" present but busy (turtled is probably using it)"
            )));
            checks.push(Check::unknown(
                "rate not checked while the device is in use".to_string(),
            ));
        }
        Err(e) => {
            let avail = available("pcm");
            checks.push(Check::fail(
                format!("cannot open audio device \"{device}\": {e}"),
                if avail.is_empty() {
                    "no ALSA playback devices found at all — is the interface plugged in?".to_string()
                } else {
                    format!("available: {}", avail.join(", "))
                },
            ));
        }
    }
    checks
}

/// Are all the show's MIDI ports present — outputs and the control input?
#[cfg(target_os = "linux")]
pub fn check_midi(destinations: &[Destination], input_port: &str) -> Vec<Check> {
    let mut checks = Vec::new();
    let avail = available("rawmidi");

    // One shared hint so a show full of logical labels does not repeat it per
    // destination.
    let mut saw_unresolved = false;

    for d in destinations {
        if avail.iter().any(|a| a == &d.port) {
            checks.push(Check::ok(format!(
                "destination \"{}\" -> \"{}\" present",
                d.name, d.port
            )));
        } else {
            if looks_logical(&d.port) {
                saw_unresolved = true;
            }
            checks.push(Check::fail(
                format!("destination \"{}\" -> \"{}\" not found", d.name, d.port),
                if avail.is_empty() {
                    "no ALSA MIDI ports found at all — is the interface plugged in?".to_string()
                } else {
                    format!("available: {}", avail.join(", "))
                },
            ));
        }
    }

    if avail.iter().any(|a| a == input_port) {
        checks.push(Check::ok(format!("control input \"{input_port}\" present")));
    } else {
        if looks_logical(input_port) {
            saw_unresolved = true;
        }
        checks.push(Check::fail(
            format!("control input \"{input_port}\" not found"),
            "without it the foot controller cannot drive the transport".to_string(),
        ));
    }

    // The spec describes logical labels like "CME:1", but resolving them to ALSA
    // names is not implemented yet, so today these fields must hold real device
    // names. Saying so turns a confusing failure into an actionable one.
    if saw_unresolved {
        checks.push(Check::warn(
            "some ports look like spec-style logical labels (e.g. \"CME:1\")",
            "logical-label resolution is not implemented yet — use the real ALSA name from `amidi -l`",
        ));
    }
    checks
}

/// A `"CME:1"`-shaped label: a colon, and not already an ALSA `hw:`/`plughw:` name.
///
/// Deliberately a heuristic used only to *improve a hint*, never to decide
/// pass/fail, so a wrong guess costs nothing.
///
/// Only reachable from the Linux MIDI probe, but kept available to `cfg(test)` so
/// its logic is still unit-tested on the dev Mac.
#[cfg(any(target_os = "linux", test))]
fn looks_logical(port: &str) -> bool {
    port.contains(':') && !port.starts_with("hw:") && !port.starts_with("plughw:")
}

/// Are the RT privileges available — `rtprio` for `SCHED_FIFO`, `memlock` for
/// `mlockall`?
///
/// Reads the limits rather than attempting the operations, because the limits are
/// what an operator can actually change, and because attempting `mlockall` here
/// would lock this short-lived process's memory for no reason.
#[cfg(target_os = "linux")]
pub fn check_realtime() -> Vec<Check> {
    const AUDIO_PRIORITY: u64 = 80; // mirrors turtled's sched::AUDIO_PRIORITY
    let mut checks = Vec::new();

    match getrlimit(libc::RLIMIT_RTPRIO) {
        Some((soft, _)) if soft >= AUDIO_PRIORITY => checks.push(Check::ok(format!(
            "RT priority available (rtprio limit {soft})"
        ))),
        Some((0, _)) => checks.push(Check::warn(
            "no RT priority (rtprio limit 0) — audio will run at normal priority".to_string(),
            "as a service this is LimitRTPRIO=95; by hand, see docs/pi-setup.md",
        )),
        Some((soft, _)) => checks.push(Check::warn(
            format!("rtprio limit {soft} is below the {AUDIO_PRIORITY} turtled asks for"),
            "raise it to 95, or run with --rt-prio <= the limit",
        )),
        None => checks.push(Check::unknown("could not read the rtprio limit")),
    }

    match getrlimit(libc::RLIMIT_MEMLOCK) {
        // "Enough to lock a song's stems" is the real question; a few hundred MB
        // is the working set, so anything under 64 MiB will fail in practice.
        Some((soft, _)) if soft == u64::MAX => {
            checks.push(Check::ok("memory locking available (memlock unlimited)"))
        }
        Some((soft, _)) if soft >= 64 * 1024 * 1024 => checks.push(Check::ok(format!(
            "memory locking available (memlock {} MiB)",
            soft / 1_048_576
        ))),
        Some((soft, _)) => checks.push(Check::warn(
            format!("memlock limit {} KiB is too small — mlockall will fail", soft / 1024),
            "as a service this is LimitMEMLOCK=infinity; by hand, see docs/pi-setup.md",
        )),
        None => checks.push(Check::unknown("could not read the memlock limit")),
    }
    checks
}

/// Read one rlimit as `(soft, hard)`.
#[cfg(target_os = "linux")]
fn getrlimit(resource: libc::__rlimit_resource_t) -> Option<(u64, u64)> {
    // SAFETY: `rlimit` is a plain struct of integers; all-zero is a valid bit
    // pattern, and `getrlimit` fully initialises it on success.
    let mut lim: libc::rlimit = unsafe { std::mem::zeroed() };
    // SAFETY: `&mut lim` is valid for the duration of the call.
    let rc = unsafe { libc::getrlimit(resource, &mut lim) };
    (rc == 0).then_some((lim.rlim_cur as u64, lim.rlim_max as u64))
}

// ---------------------------------------------------------------------------
// Everything else (the dev Mac)
// ---------------------------------------------------------------------------

/// Non-Linux stub: report honestly rather than guessing.
#[cfg(not(target_os = "linux"))]
pub fn check_audio(device: &str, rate: u32) -> Vec<Check> {
    vec![Check::unknown(format!(
        "cannot probe audio device \"{device}\" at {rate} Hz — ALSA is Linux-only (this host is {})",
        std::env::consts::OS
    ))]
}

/// Non-Linux stub. Still reports the *count*, which is a real (if small) check of
/// the show file, and keeps the section from looking empty.
#[cfg(not(target_os = "linux"))]
pub fn check_midi(destinations: &[Destination], input_port: &str) -> Vec<Check> {
    vec![Check::unknown(format!(
        "cannot probe {} MIDI destination(s) or input \"{input_port}\" — ALSA is Linux-only (this host is {})",
        destinations.len(),
        std::env::consts::OS
    ))]
}

/// Non-Linux stub.
#[cfg(not(target_os = "linux"))]
pub fn check_realtime() -> Vec<Check> {
    vec![Check::unknown(format!(
        "cannot check RT limits — SCHED_FIFO/mlockall are Linux-only (this host is {})",
        std::env::consts::OS
    ))]
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The heuristic must catch the spec's own example and leave real ALSA names
    /// alone, since its only job is to add a hint about unimplemented resolution.
    #[test]
    fn logical_labels_are_told_apart_from_alsa_names() {
        assert!(looks_logical("CME:1"));
        assert!(looks_logical("CME:in"));
        assert!(!looks_logical("hw:1,0,0"));
        assert!(!looks_logical("plughw:CARD=CME"));
        // No colon at all: not a logical label, just a name that was not found.
        assert!(!looks_logical("bogus"));
    }
}
