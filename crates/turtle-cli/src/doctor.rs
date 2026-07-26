//! `turtle doctor` — preflight (spec §10).
//!
//! §10 asks for: "audio device present + supports show rate; all mapped
//! destinations present; RT priority available; SSD mounted." The motivation is
//! that every one of those has become a paragraph of `docs/pi-setup.md` telling
//! you to run something and read the output carefully. `doctor` is that reading,
//! done for you, in one command — the difference between checking a box before a
//! show and discovering it during one.
//!
//! # Design
//!
//! Every check produces a [`Check`] with a [`Level`], and the whole run is a
//! [`Report`]. Two properties matter:
//!
//!   * **It never stops at the first problem.** A preflight that reports one
//!     fault, gets fixed, and then reports the next is a bad preflight; you want
//!     the whole list before you start working.
//!   * **`Warn` and `Fail` are genuinely different.** `Fail` means the show
//!     cannot play (missing device, invalid bundle). `Warn` means it will play
//!     but something is untuned or unusual (no isolated CPUs, no memlock limit).
//!     Only `Fail` sets a non-zero exit code, so `doctor` is usable in a script
//!     without tripping over optional tuning.
//!
//! The report model and everything that reads files are portable and unit-tested
//! on the dev Mac. The ALSA and `getrlimit` probes are Linux-only, in
//! [`crate::probe`], and report [`Level::Unknown`] elsewhere rather than lying.

use std::fmt;
use std::path::Path;

use turtle_core::proto::Request;

/// How bad a finding is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    /// Checked and fine.
    Ok,
    /// The show will play, but something is untuned, unusual, or unverifiable in
    /// a way worth knowing about.
    Warn,
    /// The show cannot play until this is fixed.
    Fail,
    /// Could not be checked on this host (e.g. an ALSA probe on macOS). Reported
    /// rather than silently skipped, so the output never implies more assurance
    /// than it has.
    Unknown,
}

impl Level {
    /// Fixed-width tag so the messages line up in a terminal.
    fn tag(self) -> &'static str {
        match self {
            Level::Ok => "ok  ",
            Level::Warn => "warn",
            Level::Fail => "FAIL",
            Level::Unknown => "?   ",
        }
    }
}

/// One finding.
#[derive(Debug, Clone)]
pub struct Check {
    pub level: Level,
    /// What was checked, phrased as the *result* ("device present") rather than
    /// the question, so a wall of `ok` lines still reads as information.
    pub detail: String,
    /// What to do about it. Only ever set for `Warn`/`Fail` — a hint on a passing
    /// check is noise.
    pub hint: Option<String>,
}

impl Check {
    pub fn ok(detail: impl Into<String>) -> Self {
        Check { level: Level::Ok, detail: detail.into(), hint: None }
    }
    pub fn warn(detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Check { level: Level::Warn, detail: detail.into(), hint: Some(hint.into()) }
    }
    pub fn fail(detail: impl Into<String>, hint: impl Into<String>) -> Self {
        Check { level: Level::Fail, detail: detail.into(), hint: Some(hint.into()) }
    }
    pub fn unknown(detail: impl Into<String>) -> Self {
        Check { level: Level::Unknown, detail: detail.into(), hint: None }
    }
}

/// A grouped set of findings.
#[derive(Debug, Default)]
pub struct Report {
    sections: Vec<(String, Vec<Check>)>,
}

impl Report {
    pub fn section(&mut self, name: impl Into<String>, checks: Vec<Check>) {
        self.sections.push((name.into(), checks));
    }

    pub fn counts(&self) -> (usize, usize) {
        let all = self.sections.iter().flat_map(|(_, cs)| cs);
        (
            all.clone().filter(|c| c.level == Level::Fail).count(),
            all.filter(|c| c.level == Level::Warn).count(),
        )
    }

    /// Non-zero only for `Fail`. Warnings are informational by design — see the
    /// module docs.
    pub fn is_failure(&self) -> bool {
        self.counts().0 > 0
    }
}

impl fmt::Display for Report {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        writeln!(f, "The Turtle — preflight")?;
        for (name, checks) in &self.sections {
            writeln!(f, "\n{name}")?;
            for c in checks {
                writeln!(f, "  {} {}", c.level.tag(), c.detail)?;
                if let Some(hint) = &c.hint {
                    writeln!(f, "       -> {hint}")?;
                }
            }
        }
        let (fails, warns) = self.counts();
        writeln!(f)?;
        match (fails, warns) {
            (0, 0) => write!(f, "all checks passed"),
            (0, w) => write!(f, "{w} warning(s), no failures"),
            (f_, w) => write!(f, "{f_} FAILURE(S), {w} warning(s)"),
        }
    }
}

// ---------------------------------------------------------------------------
// The checks
// ---------------------------------------------------------------------------

/// Run every check and build the report.
///
/// `show_path` may be a `show.toml` or a bundle directory containing one, since
/// both are things a person naturally has to hand.
pub fn run(show_path: Option<&str>, socket: &Path) -> Report {
    let mut report = Report::default();

    // Without a show there is nothing to check the hardware *against* — the
    // device name and the destination ports come from show.toml. So the
    // show-dependent sections are skipped rather than guessed at.
    let show = match show_path {
        Some(p) => {
            let resolved = resolve_show_path(p);
            let (checks, show) = check_show(&resolved);
            report.section("show", checks);
            show.map(|s| (s, resolved))
        }
        None => {
            report.section(
                "show",
                vec![Check::warn(
                    "no show given — skipping bundle, audio, and MIDI checks",
                    "pass a bundle or show.toml: turtle doctor <path>",
                )],
            );
            None
        }
    };

    if let Some((show, path)) = &show {
        report.section("stems", check_stems(show, path));
        report.section("audio", crate::probe::check_audio(&show.audio.device, show.show.playback_rate));
        report.section("midi", crate::probe::check_midi(&show.destinations, &show.control.input_port));
    }

    report.section("realtime", crate::probe::check_realtime());
    report.section("system", check_system());
    report.section("daemon", check_daemon(socket));
    report
}

/// Accept either `show.toml` or the bundle directory that contains it.
fn resolve_show_path(p: &str) -> std::path::PathBuf {
    let path = std::path::PathBuf::from(p);
    if path.is_dir() { path.join("show.toml") } else { path }
}

/// Does the bundle load and validate?
fn check_show(path: &Path) -> (Vec<Check>, Option<turtle_core::Show>) {
    let show = match turtle_core::Show::load(path) {
        Ok(s) => s,
        Err(e) => {
            return (
                vec![Check::fail(
                    format!("{}: {e}", path.display()),
                    "fix the bundle, or check the path — `turtle validate <path>` for detail",
                )],
                None,
            );
        }
    };
    let mut checks = vec![Check::ok(format!(
        "loaded \"{}\": {} song(s), {} destination(s), {} Hz",
        show.show.name,
        show.setlist.len(),
        show.destinations.len(),
        show.show.playback_rate
    ))];
    match show.validate() {
        Ok(()) => checks.push(Check::ok("bundle validates")),
        Err(e) => checks.push(Check::fail(
            format!("invalid: {e}"),
            "`turtle validate <path>` shows the same error in isolation",
        )),
    }
    // An empty setlist loads and validates but cannot play anything, which is
    // worth saying out loud at preflight rather than at showtime.
    if show.setlist.is_empty() {
        checks.push(Check::warn(
            "setlist is empty — nothing to arm",
            "add [[setlist]] entries to show.toml",
        ));
    }
    (checks, Some(show))
}

/// Are the stems actually on disk, and is the bundle somewhere sane?
///
/// This is §10's "SSD mounted" check, made concrete: what matters is not whether
/// a particular device is mounted but whether the stems this show needs can
/// actually be read right now.
fn check_stems(show: &turtle_core::Show, show_path: &Path) -> Vec<Check> {
    let root = show_path.parent().unwrap_or(Path::new("."));
    let mut checks = Vec::new();
    let mut total_bytes: u64 = 0;
    let mut missing = 0;

    for entry in &show.setlist {
        let song_dir = root.join("songs").join(&entry.song);
        let song_toml = song_dir.join("song.toml");
        let song = match turtle_core::Song::load(&song_toml) {
            Ok(s) => s,
            Err(e) => {
                checks.push(Check::fail(
                    format!("song \"{}\": {e}", entry.song),
                    format!("expected {}", song_toml.display()),
                ));
                missing += 1;
                continue;
            }
        };
        for pair in &song.pairs {
            // `pair.file` is relative to the *song* directory and already carries
            // its own `stems/` prefix (e.g. "stems/pair1.wav"). Joining another
            // "stems" here looked right and produced `stems/stems/pair1.wav`,
            // which reported every stem as missing. This mirrors what
            // `turtled`'s loader does (`stems.rs`: `base_dir.join(&pair.file)`),
            // and the test below pins the two together.
            let stem = song_dir.join(&pair.file);
            match std::fs::metadata(&stem) {
                // A zero-byte stem is a truncated copy: it exists, so a naive
                // existence check would pass, and it fails only at load time.
                Ok(m) if m.len() == 0 => {
                    checks.push(Check::fail(
                        format!("stem is empty: {}", stem.display()),
                        "re-copy the bundle; this is usually a truncated transfer",
                    ));
                    missing += 1;
                }
                Ok(m) => total_bytes += m.len(),
                Err(e) => {
                    checks.push(Check::fail(
                        format!("stem unreadable: {} ({e})", stem.display()),
                        "check the file exists and the service user can read it",
                    ));
                    missing += 1;
                }
            }
        }
    }

    if missing == 0 && !show.setlist.is_empty() {
        checks.push(Check::ok(format!(
            "all stems present and readable ({:.1} MB across {} song(s))",
            total_bytes as f64 / 1_048_576.0,
            show.setlist.len()
        )));
    }
    checks
}

/// CPU tuning, read from sysfs. Portable: absent files simply mean "not tuned".
///
/// Deliberately read here rather than borrowed from `turtled`'s `sched` module —
/// that lives in the other binary, and all doctor needs is to display the raw
/// values, so re-reading two files beats coupling the crates.
fn check_system() -> Vec<Check> {
    let mut checks = Vec::new();

    match read_trimmed("/sys/devices/system/cpu/cpu0/cpufreq/scaling_governor") {
        Some(g) if g == "performance" => {
            checks.push(Check::ok("CPU governor: performance"))
        }
        Some(g) => checks.push(Check::warn(
            format!("CPU governor: {g} (not performance)"),
            "optional: enable turtle-tuning.service (see docs/pi-setup.md)",
        )),
        None => checks.push(Check::unknown(
            "CPU governor: no cpufreq on this host",
        )),
    }

    // Empty is the normal, untuned state; isolcpus is explicitly optional, so
    // this is never a failure.
    match read_trimmed("/sys/devices/system/cpu/isolated") {
        Some(s) if !s.is_empty() => {
            checks.push(Check::ok(format!("isolated CPUs: {s} (audio thread will be pinned)")))
        }
        Some(_) => checks.push(Check::warn(
            "no isolated CPUs (isolcpus not set)",
            "optional, and usually unnecessary — see docs/pi-setup.md before enabling",
        )),
        None => checks.push(Check::unknown("isolated CPUs: not reported by this kernel")),
    }
    checks
}

fn read_trimmed(path: &str) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

/// Is a daemon up, and what does it think it is doing?
fn check_daemon(socket: &Path) -> Vec<Check> {
    match crate::client::request(socket, &Request::Status) {
        Ok(turtle_core::proto::Response::Status(s)) => vec![Check::ok(format!(
            "turtled responding on {}: {:?}, song {}",
            socket.display(),
            s.state,
            s.song.as_deref().unwrap_or("(none)")
        ))],
        Ok(other) => vec![Check::warn(
            format!("turtled on {} gave an unexpected reply: {other:?}", socket.display()),
            "version mismatch between turtle and turtled?",
        )],
        // Not a failure: `doctor` is most useful *before* starting the daemon,
        // and "no daemon yet" is the expected state then.
        Err(e) => vec![Check::warn(
            format!("no daemon on {}: {e}", socket.display()),
            "expected if you have not started it yet; otherwise: systemctl status turtled",
        )],
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Warnings must not fail the run: optional tuning is reported, not enforced,
    /// so `doctor` stays usable in a script.
    #[test]
    fn only_failures_set_a_nonzero_exit() {
        let mut r = Report::default();
        r.section("a", vec![Check::ok("fine"), Check::warn("meh", "do x")]);
        assert!(!r.is_failure());
        assert_eq!(r.counts(), (0, 1));

        r.section("b", vec![Check::fail("broken", "fix y")]);
        assert!(r.is_failure());
        assert_eq!(r.counts(), (1, 1));
    }

    /// `Unknown` is neither a pass nor a failure — it must not be silently
    /// counted as either, or the summary would overstate the assurance.
    #[test]
    fn unknown_is_neither_a_pass_nor_a_failure() {
        let mut r = Report::default();
        r.section("a", vec![Check::unknown("cannot probe here")]);
        assert!(!r.is_failure());
        assert_eq!(r.counts(), (0, 0));
    }

    /// Hints exist to tell you what to do; a check that can fail without one
    /// would leave the operator stuck.
    #[test]
    fn problems_always_carry_a_hint() {
        assert!(Check::warn("x", "do this").hint.is_some());
        assert!(Check::fail("x", "do this").hint.is_some());
        assert!(Check::ok("x").hint.is_none(), "a hint on a pass is noise");
    }

    /// Both spellings a person naturally has to hand must work.
    #[test]
    fn a_bundle_directory_or_a_show_toml_both_resolve() {
        let dir = std::env::temp_dir().join(format!("turtle-doctor-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("show.toml"), "").unwrap();
        assert_eq!(resolve_show_path(dir.to_str().unwrap()), dir.join("show.toml"));

        let explicit = dir.join("show.toml");
        assert_eq!(resolve_show_path(explicit.to_str().unwrap()), explicit);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// Stems must be looked for where `turtled` actually loads them.
    ///
    /// This exists because the first version of `check_stems` joined an extra
    /// `stems/` onto `pair.file` (which already contains it), so a perfectly good
    /// bundle reported every stem missing. A preflight that cries wolf is worse
    /// than no preflight, so this pins the path against a real generated bundle
    /// rather than against my assumption about the layout.
    #[test]
    fn stems_are_found_where_the_loader_looks_for_them() {
        // Generated by `gen-tone`, not hand-written: the point is to pin doctor
        // against a bundle the project itself produces, so a future change to the
        // layout breaks this test rather than silently breaking doctor.
        let root = std::env::temp_dir().join(format!("turtle-stems-{}", std::process::id()));
        std::fs::remove_dir_all(&root).ok();
        crate::gen::gen_tone(&root, 0.25, 440.0).expect("generate a test bundle");

        let show = turtle_core::Show::load(root.join("show.toml")).unwrap();
        let checks = check_stems(&show, &root.join("show.toml"));
        assert!(
            checks.iter().all(|c| c.level == Level::Ok),
            "a present stem must not be reported missing: {checks:?}"
        );

        // And a truncated (zero-byte) stem must be caught, since it exists and
        // would pass a bare existence check while failing at load time.
        let stem = root.join("songs").join("tone").join("stems").join("pair1.wav");
        assert!(stem.exists(), "gen-tone layout changed; update this test");
        std::fs::write(&stem, b"").unwrap();
        let checks = check_stems(&show, &root.join("show.toml"));
        assert!(
            checks.iter().any(|c| c.level == Level::Fail && c.detail.contains("empty")),
            "a zero-byte stem must fail: {checks:?}"
        );

        std::fs::remove_dir_all(&root).ok();
    }

    /// A missing bundle must be one clear failure, not a panic or a cascade.
    #[test]
    fn a_missing_show_fails_cleanly_without_a_show_value() {
        let (checks, show) = check_show(Path::new("/nonexistent/turtle/show.toml"));
        assert!(show.is_none());
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].level, Level::Fail);
        assert!(checks[0].hint.is_some());
    }

    /// The report renders every section, and its summary line reflects the worst
    /// finding — that line is what a person actually reads.
    #[test]
    fn the_summary_reflects_the_worst_finding() {
        let mut r = Report::default();
        r.section("show", vec![Check::ok("loaded")]);
        let s = r.to_string();
        assert!(s.contains("all checks passed"), "{s}");

        r.section("audio", vec![Check::fail("no device", "plug it in")]);
        let s = r.to_string();
        assert!(s.contains("1 FAILURE(S)"), "{s}");
        // The hint must reach the output, not just the struct.
        assert!(s.contains("plug it in"), "{s}");
    }

    /// Untuned is the normal state and must never be a failure — most Pis will
    /// legitimately have ondemand and no isolated CPUs.
    #[test]
    fn system_checks_never_fail_the_run() {
        for c in check_system() {
            assert_ne!(c.level, Level::Fail, "system tuning is optional: {:?}", c);
        }
    }

    /// `doctor` is most useful before the daemon is started, so an absent daemon
    /// is a warning, not a failure.
    #[test]
    fn an_absent_daemon_is_only_a_warning() {
        let checks = check_daemon(Path::new("/nonexistent/turtle.sock"));
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].level, Level::Warn);
    }
}
