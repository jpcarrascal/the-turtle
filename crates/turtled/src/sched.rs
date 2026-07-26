//! Real-time tuning: `SCHED_FIFO` thread priorities and `mlockall` (spec §3, §12).
//!
//! Two independent things can stall an audio period, and this module does both:
//! [`apply_or_warn`] stops *other tasks* from taking the CPU, and
//! [`lock_memory_or_warn`] stops the *kernel* from making us wait on a page
//! fault. Priority alone is only half the job — see [`lock_memory`].
//!
//! The spec's thread table asks for the audio loop and the MIDI scheduler to run
//! under `SCHED_FIFO` while the control and loader threads stay at normal
//! priority. Under Linux's default `SCHED_OTHER` (CFS), our threads are just
//! ordinary work: a busy `apt upgrade`, a browser, or a kernel housekeeping
//! thread can delay them by tens of milliseconds. `SCHED_FIFO` means "run me
//! ahead of every normal task, and don't preempt me until I block" — which for
//! an audio period loop is the difference between a clean show and an xrun.
//!
//! # v1 reality: two threads, not three
//!
//! `turtled` fuses the MIDI scheduler *into* the control loop (one thread —
//! see [`crate::control::run`]). So the spec's split assignment can't be applied
//! literally, and the fused thread is what actually determines MIDI timing.
//! It therefore gets `SCHED_FIFO` too, but at [`MIDI_PRIORITY`], **below**
//! [`AUDIO_PRIORITY`]: an audio underrun is instantly audible, whereas a MIDI
//! event a millisecond late is not, so audio must be able to preempt it.
//!
//! # Failing to get RT priority is not fatal
//!
//! Setting `SCHED_FIFO` needs privilege (`CAP_SYS_NICE`, or an `rtprio` limit
//! in `/etc/security/limits.conf` — see §12). When it is unavailable we log and
//! carry on at normal priority: v1's deliberately large, xrun-proof buffers
//! (§3.1) make that work, and per §12's failure policy the show must never
//! refuse to play for a tuning issue. `turtle doctor` is where "RT priority
//! available" becomes a preflight *check* rather than a runtime surprise.

/// Priority for the audio RT thread.
///
/// 80 sits above every normal task and above the default `threadirqs` IRQ
/// threads (50), while staying clear of the kernel's own 99-priority watchdog
/// and migration threads — taking those over is how you hard-lock a Pi.
pub const AUDIO_PRIORITY: u8 = 80;

/// Priority for the fused control + MIDI-scheduler thread: below the audio loop
/// (see the module docs), above everything normal.
pub const MIDI_PRIORITY: u8 = 75;

/// The highest priority we will ever ask for, regardless of what the caller
/// passes. 99 is reserved for kernel threads that must not be starved.
pub const MAX_PRIORITY: u8 = 95;

// Compile-time guard on the invariant the whole design rests on: an audio
// period must be able to preempt a MIDI dispatch. A `const` block is evaluated
// during compilation, so editing either constant into the wrong order breaks
// the build rather than merely failing a test someone might not run.
const _: () = assert!(AUDIO_PRIORITY > MIDI_PRIORITY);
const _: () = assert!(AUDIO_PRIORITY <= MAX_PRIORITY);

/// Clamp a requested priority into the range we consider safe to request.
///
/// Separated from the syscall so the policy is unit-testable on the dev Mac.
/// `0` is not clamped up: it is the caller's "disable RT" sentinel, handled
/// before this is ever reached.
pub fn clamp_priority(requested: u8) -> u8 {
    requested.clamp(1, MAX_PRIORITY)
}

/// The MIDI/control priority derived from an audio priority.
///
/// Kept a fixed step below so a custom `--rt-prio` preserves the ordering that
/// matters (audio preempts MIDI) instead of silently flattening it. Saturates at
/// 1 rather than wrapping when the audio priority is already tiny.
pub fn midi_priority_for(audio_priority: u8) -> u8 {
    clamp_priority(audio_priority.saturating_sub(5).max(1))
}

/// Put the **calling** thread on `SCHED_FIFO` at `priority`.
///
/// Applies to the current thread by design: it is called as the first thing a
/// freshly-spawned RT thread does, which avoids needing to name or hand around
/// another thread's handle.
///
/// # Why `unsafe`
///
/// There is no safe Rust API for thread scheduling, so this calls
/// `libc::pthread_setschedparam`. The `unsafe` is quarantined to the one call:
/// we pass a pointer to a local `sched_param` that outlives the call, and
/// `pthread_self()` is always a valid handle for the current thread, so the
/// preconditions hold by construction.
#[cfg(target_os = "linux")]
pub fn set_current_thread_fifo(priority: u8) -> Result<(), String> {
    let priority = clamp_priority(priority);

    // The kernel, not us, is the authority on the legal range for a policy;
    // ask it rather than hardcoding 1..=99.
    // SAFETY: a pure query with no pointer arguments.
    let (min, max) = unsafe {
        (
            libc::sched_get_priority_min(libc::SCHED_FIFO),
            libc::sched_get_priority_max(libc::SCHED_FIFO),
        )
    };
    if min < 0 || max < 0 {
        return Err("kernel reports no SCHED_FIFO priority range".into());
    }
    let target = (priority as libc::c_int).clamp(min, max);

    // `sched_param` has one meaningful field for FIFO. `zeroed` rather than a
    // struct literal because the layout is libc's and may carry padding fields
    // we have no business naming.
    // SAFETY: `sched_param` is a plain C struct of integers; all-zero is a
    // valid bit pattern for it.
    let mut param: libc::sched_param = unsafe { std::mem::zeroed() };
    param.sched_priority = target;

    // SAFETY: `pthread_self()` is a valid handle for the calling thread, and
    // `&param` is valid for the duration of the call.
    let rc = unsafe { libc::pthread_setschedparam(libc::pthread_self(), libc::SCHED_FIFO, &param) };
    if rc == 0 {
        return Ok(());
    }

    // `pthread_setschedparam` returns the errno directly rather than setting
    // the global `errno` — unlike most libc calls.
    let hint = match rc {
        libc::EPERM => {
            " (need CAP_SYS_NICE, or an rtprio limit in /etc/security/limits.conf)"
        }
        libc::EINVAL => " (kernel rejected the policy or priority)",
        _ => "",
    };
    Err(format!(
        "pthread_setschedparam(SCHED_FIFO, {target}) failed: {}{hint}",
        std::io::Error::from_raw_os_error(rc)
    ))
}

/// Non-Linux stub so callers need no `cfg`. Real-time scheduling is a
/// deployment concern for the Pi; the dev Mac only ever runs the portable
/// tests, and macOS RT scheduling works differently enough that pretending
/// otherwise would be misleading.
#[cfg(not(target_os = "linux"))]
pub fn set_current_thread_fifo(_priority: u8) -> Result<(), String> {
    Err(format!(
        "SCHED_FIFO is Linux-only (this host is {})",
        std::env::consts::OS
    ))
}

/// Pin the whole process's memory into RAM with `mlockall` (§12).
///
/// # Why this is not the same thing as `SCHED_FIFO`
///
/// Priority decides *who gets the CPU*. A **page fault** is an orthogonal stall:
/// if a page of code or data is not resident — swapped out, or lazily mapped and
/// never yet touched — touching it traps into the kernel and may go to the SD
/// card. Being priority 80 does not help at all, because the thread is not
/// waiting for CPU, it is blocked on I/O. That is exactly the tens of
/// milliseconds an xrun is made of, so RT scheduling without this is only half
/// the job.
///
/// `MCL_CURRENT | MCL_FUTURE` locks what is mapped now *and* keeps locking new
/// mappings, which is what makes it safe to call before the stems are loaded:
/// their buffers are covered as they are allocated.
///
/// # The cost, and the failure mode to know about
///
/// Locked pages are committed RAM that the kernel can never reclaim — fine for a
/// single-purpose appliance, and the reason the unit sets
/// `LimitMEMLOCK=infinity`. With a *finite* limit, `MCL_FUTURE` turns a large
/// later allocation (a background stem preload) into a hard `ENOMEM` rather than
/// a slow one, which surfaces as a stem-load failure. That is a defined
/// degradation per §12 — refuse to arm and signal the error — but it is why the
/// limit must not be left at its small default.
#[cfg(target_os = "linux")]
pub fn lock_memory() -> Result<(), String> {
    // SAFETY: a flags-only call with no pointer arguments; it either succeeds or
    // sets errno, and cannot invalidate anything Rust is holding.
    let rc = unsafe { libc::mlockall(libc::MCL_CURRENT | libc::MCL_FUTURE) };
    if rc == 0 {
        return Ok(());
    }
    // Unlike `pthread_setschedparam`, this is a conventional syscall wrapper:
    // it returns -1 and puts the reason in `errno`.
    let err = std::io::Error::last_os_error();
    let hint = match err.raw_os_error() {
        Some(libc::ENOMEM) | Some(libc::EPERM) => {
            " (need a memlock rlimit — LimitMEMLOCK=infinity, or memlock in limits.conf)"
        }
        _ => "",
    };
    Err(format!("mlockall failed: {err}{hint}"))
}

/// Non-Linux stub, mirroring [`set_current_thread_fifo`]: memory locking is a
/// deployment concern for the Pi.
#[cfg(not(target_os = "linux"))]
pub fn lock_memory() -> Result<(), String> {
    Err(format!(
        "mlockall is Linux-only (this host is {})",
        std::env::consts::OS
    ))
}

/// Lock memory and report the outcome, never failing the caller.
///
/// Best-effort for the same reason as [`apply_or_warn`]: paging is a *tuning*
/// concern, and §12 says the show must never refuse to play over one. Tied to
/// the same `rt` switch, so `--rt-prio 0` gives a completely untuned process to
/// A/B against rather than a half-tuned one.
pub fn lock_memory_or_warn(enabled: bool) {
    if !enabled {
        println!("[sched] memory not locked (RT disabled)");
        return;
    }
    match lock_memory() {
        Ok(()) => println!("[sched] memory locked (mlockall)"),
        Err(e) => eprintln!(
            "warning: memory stays pageable: {e}\n\
             \x20        audio may glitch if pages are evicted under memory pressure"
        ),
    }
}

/// Apply `SCHED_FIFO` and report the outcome, never failing the caller.
///
/// The one place the "RT priority is best-effort" policy lives, so the audio and
/// control threads can't drift apart on how they handle it. `what` names the
/// thread in the log line ("audio", "control+midi").
///
/// `None` means RT scheduling was switched off deliberately (`--rt-prio 0`), so
/// it reports plainly rather than warning — a chosen configuration is not a
/// problem to flag.
pub fn apply_or_warn(what: &str, priority: Option<u8>) {
    let Some(priority) = priority else {
        println!("[sched] {what} thread: normal priority (RT disabled)");
        return;
    };
    match set_current_thread_fifo(priority) {
        Ok(()) => println!("[sched] {what} thread: SCHED_FIFO priority {priority}"),
        // Deliberately a warning, not an error: see the module docs.
        Err(e) => eprintln!(
            "warning: {what} thread stays at normal priority: {e}\n\
             \x20        audio may glitch under load; run `turtle doctor` to check RT availability"
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The constants' ordering is guarded at compile time (see the `const _`
    /// asserts above); what this pins is that the *derivation* agrees with them,
    /// so `--rt-prio` and the defaults can't disagree about the MIDI priority.
    #[test]
    fn the_derived_midi_priority_matches_the_declared_one() {
        assert_eq!(midi_priority_for(AUDIO_PRIORITY), MIDI_PRIORITY);
    }

    /// Never request 96..=99: those belong to kernel watchdog/migration threads.
    #[test]
    fn requests_are_clamped_below_the_kernel_reserved_band() {
        assert_eq!(clamp_priority(99), MAX_PRIORITY);
        assert_eq!(clamp_priority(200), MAX_PRIORITY);
        assert_eq!(clamp_priority(80), 80);
    }

    /// A custom `--rt-prio` must keep audio above midi rather than flattening
    /// them together, which would let a MIDI dispatch delay an audio period.
    #[test]
    fn a_custom_priority_preserves_the_ordering() {
        for audio in [10u8, 40, 80, 95] {
            assert!(
                midi_priority_for(audio) < audio,
                "midi must stay below audio at {audio}"
            );
        }
    }

    /// A tiny requested priority must not underflow into a huge one.
    #[test]
    fn a_minimal_priority_saturates_instead_of_wrapping() {
        assert_eq!(midi_priority_for(1), 1);
        assert_eq!(midi_priority_for(3), 1);
    }

    /// `mlockall` either works or fails for one of the two documented reasons —
    /// and the message must say how to fix it, because the whole point of the
    /// best-effort policy is that the operator can act on the warning.
    ///
    /// Deliberately asserts the *contract* rather than success, so it passes both
    /// with the privilege (a tuned Pi, a privileged container) and without it (a
    /// plain container, an unprivileged user) instead of being a test that only
    /// holds on one machine.
    #[cfg(target_os = "linux")]
    #[test]
    fn locking_memory_either_succeeds_or_explains_itself() {
        // Printed (visible under `--nocapture`) so it is possible to tell which
        // branch a given machine actually took — otherwise a test that accepts
        // both outcomes cannot be shown to have exercised either.
        match lock_memory() {
            Ok(()) => {
                eprintln!("mlockall: succeeded on this host");
                // Undo it immediately: this test process is not the daemon, and
                // leaving MCL_FUTURE set could make a later allocation in another
                // test fail under a small memlock limit.
                // SAFETY: a no-argument call that only relaxes what we just did.
                unsafe { libc::munlockall() };
            }
            Err(e) => {
                eprintln!("mlockall: refused on this host: {e}");
                assert!(
                    e.contains("memlock"),
                    "the failure must name the limit to raise, got: {e}"
                );
            }
        }
    }
}
