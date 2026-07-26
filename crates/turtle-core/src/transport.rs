//! The transport / setlist state machine (spec §8).
//!
//! This is a pure state machine: it owns the discrete transport state, the
//! current + armed-next song selection, and turns incoming [`Command`]s into
//! [`Action`]s for the engine to execute. It touches no audio, MIDI, or
//! hardware — `turtled` wires the actions to the RT threads and loader — which
//! is what makes the transport rules (including the Ableton-style Stop
//! semantics) unit-testable on any machine.
//!
//! ```text
//! IDLE → LOADING → ARMED → PLAYING → (STOPPED | ENDED) → ARMED/IDLE
//! ```

use crate::model::{SetlistEntry, Show};

/// Discrete transport state (spec §8, mirrored by the GPIO status LED in §8.1).
///
/// `Serialize`/`Deserialize` because this crosses the control socket verbatim
/// in [`crate::proto::Status`]; `rename_all` keeps the wire form lowercase
/// (`"playing"`) rather than Rust's `"Playing"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
    /// No song selected.
    Idle,
    /// Preloading the current song (arming).
    Loading,
    /// Song loaded and ready; transport stopped at the top.
    Armed,
    /// Transport running.
    Playing,
    /// User-stopped (position 0 if `rewind_on_stop`, else held in place).
    Stopped,
    /// Song reached its end without auto-advancing.
    Ended,
}

/// Config the state machine needs from `show.toml` (spec §7.1, §8).
#[derive(Debug, Clone, Copy)]
pub struct TransportConfig {
    /// Gapless: start the armed-next song at `ENDED`.
    pub auto_advance: bool,
    /// On **Stop**, reset the song pointer to 0.
    pub rewind_on_stop: bool,
}

/// A transport command, decoded from the foot controller (§8) or signalled
/// internally by the loader/RT threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    /// Program-Change song selection: arm the setlist entry with this `pc`.
    Select(u8),
    /// Start / Continue / Restart (there is no separate Restart — §8).
    Start,
    /// Stop (first = clean release + rewind; second = panic).
    Stop,
    /// Arm the next setlist entry.
    Next,
    /// Arm the previous setlist entry.
    Prev,
    /// Explicit MIDI panic (Note 65 / GPIO button).
    Panic,
    /// Internal: the loader finished preloading the pending song.
    Loaded,
    /// Internal: the RT transport reached the current song's end.
    EndReached,
}

/// A side effect for the engine to carry out. The ordering within a returned
/// `Vec` is significant (e.g. release notes *before* seeking).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    /// Loader: preload this song's stems into RAM.
    Preload(String),
    /// RT: begin the transport from the current position.
    StartPlayback,
    /// RT: halt the transport.
    StopPlayback,
    /// RT: reset the song/MIDI pointer to sample 0.
    SeekToZero,
    /// MIDI: send note-offs for currently-sounding notes (clean release, §8).
    ReleaseNotes,
    /// MIDI: all-notes-off + reset-all-controllers on every port (§5).
    Panic,
}

/// The transport / setlist state machine.
#[derive(Debug, Clone)]
pub struct Transport {
    state: State,
    cfg: TransportConfig,
    setlist: Vec<SetlistEntry>,
    /// Index into `setlist` of the current (playing/armed) song.
    current: Option<usize>,
    /// Index of a song armed during playback, to start next.
    armed_next: Option<usize>,
    /// Whether the `armed_next` preload has completed.
    armed_next_ready: bool,
    /// Whether a double-Stop panic has already fired in the current stop (so
    /// further Stops are no-ops rather than re-spamming the panic burst — §5 DIN
    /// budget). Cleared on the next Play or fresh Stop.
    panicked: bool,
}

impl Transport {
    pub fn new(setlist: Vec<SetlistEntry>, cfg: TransportConfig) -> Self {
        Transport {
            state: State::Idle,
            cfg,
            setlist,
            current: None,
            armed_next: None,
            armed_next_ready: false,
            panicked: false,
        }
    }

    /// Build from a loaded show (setlist + auto_advance / rewind_on_stop).
    pub fn from_show(show: &Show) -> Self {
        Self::new(
            show.setlist.clone(),
            TransportConfig {
                auto_advance: show.show.auto_advance,
                rewind_on_stop: show.show.rewind_on_stop,
            },
        )
    }

    pub fn state(&self) -> State {
        self.state
    }

    /// The current song's directory name, if one is selected.
    pub fn current_song(&self) -> Option<&str> {
        self.current.map(|i| self.setlist[i].song.as_str())
    }

    /// The armed-next song's directory name, if one is armed.
    pub fn armed_next_song(&self) -> Option<&str> {
        self.armed_next.map(|i| self.setlist[i].song.as_str())
    }

    /// Apply a command, returning the ordered actions to perform.
    pub fn apply(&mut self, cmd: Command) -> Vec<Action> {
        match cmd {
            Command::Select(pc) => self.select(pc),
            Command::Next => self.step(1),
            Command::Prev => self.step(-1),
            Command::Start => self.start(),
            Command::Stop => self.stop(),
            Command::Panic => vec![Action::Panic],
            Command::Loaded => self.loaded(),
            Command::EndReached => self.end_reached(),
        }
    }

    fn select(&mut self, pc: u8) -> Vec<Action> {
        match self.setlist.iter().position(|e| e.pc == pc) {
            Some(idx) => self.arm_song(idx),
            None => vec![], // unknown PC: ignore
        }
    }

    fn step(&mut self, delta: isize) -> Vec<Action> {
        if self.setlist.is_empty() {
            return vec![];
        }
        let target = self.reference_index() + delta;
        if target < 0 || target as usize >= self.setlist.len() {
            return vec![]; // at an end: no-op (no wrap)
        }
        self.arm_song(target as usize)
    }

    /// The index that Next/Prev step from: the armed-next while playing (so
    /// repeated Next walks forward), else the current song, else "before 0".
    fn reference_index(&self) -> isize {
        let idx = if self.state == State::Playing {
            self.armed_next.or(self.current)
        } else {
            self.current
        };
        idx.map(|i| i as isize).unwrap_or(-1)
    }

    fn arm_song(&mut self, idx: usize) -> Vec<Action> {
        let song = self.setlist[idx].song.clone();
        if self.state == State::Playing {
            // Arm the next song without interrupting the current one (§8).
            self.armed_next = Some(idx);
            self.armed_next_ready = false;
        } else {
            // (Re)arm as the current song.
            self.current = Some(idx);
            self.armed_next = None;
            self.armed_next_ready = false;
            self.state = State::Loading;
        }
        vec![Action::Preload(song)]
    }

    fn loaded(&mut self) -> Vec<Action> {
        match self.state {
            State::Loading => self.state = State::Armed,
            State::Playing => self.armed_next_ready = true,
            _ => {}
        }
        vec![]
    }

    fn start(&mut self) -> Vec<Action> {
        // Playing again makes the double-Stop panic available for the next stop.
        self.panicked = false;
        match self.state {
            // Fresh arm, or resume from a stop (position already 0 if rewound,
            // else continue in place). Either way, just run.
            State::Armed | State::Stopped => {
                self.state = State::Playing;
                vec![Action::StartPlayback]
            }
            // Replay a finished song from the top.
            State::Ended => {
                self.state = State::Playing;
                vec![Action::SeekToZero, Action::StartPlayback]
            }
            // Start while playing == restart from the top (Start *is* Restart).
            State::Playing => vec![Action::ReleaseNotes, Action::SeekToZero],
            // Nothing armed yet.
            State::Idle | State::Loading => vec![],
        }
    }

    fn stop(&mut self) -> Vec<Action> {
        match self.state {
            State::Playing => {
                self.state = State::Stopped;
                // Fresh stop: the next Stop is a panic (not yet fired).
                self.panicked = false;
                let mut actions = vec![Action::ReleaseNotes, Action::StopPlayback];
                if self.cfg.rewind_on_stop {
                    actions.push(Action::SeekToZero);
                }
                actions
            }
            // Second Stop (double-tap) sends a full panic — but only once (§8);
            // further Stops are no-ops until the next Play, to spare the DIN bus.
            State::Stopped | State::Ended => {
                self.state = State::Stopped;
                if self.panicked {
                    vec![]
                } else {
                    self.panicked = true;
                    vec![Action::Panic]
                }
            }
            // Nothing playing.
            State::Idle | State::Loading | State::Armed => vec![],
        }
    }

    fn end_reached(&mut self) -> Vec<Action> {
        if self.state != State::Playing {
            return vec![];
        }
        // Gapless advance to the armed-next song if one is loaded (§8).
        if self.cfg.auto_advance && self.armed_next_ready {
            if let Some(next) = self.armed_next.take() {
                self.current = Some(next);
                self.armed_next_ready = false;
                self.state = State::Playing;
                return vec![Action::SeekToZero, Action::StartPlayback];
            }
        }
        self.state = State::Ended;
        vec![Action::StopPlayback]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn setlist() -> Vec<SetlistEntry> {
        vec![
            SetlistEntry { pc: 0, song: "01-opener".into() },
            SetlistEntry { pc: 1, song: "02-second".into() },
            SetlistEntry { pc: 2, song: "03-third".into() },
        ]
    }

    fn transport(auto_advance: bool, rewind_on_stop: bool) -> Transport {
        Transport::new(setlist(), TransportConfig { auto_advance, rewind_on_stop })
    }

    #[test]
    fn select_arms_then_starts() {
        let mut t = transport(false, true);
        assert_eq!(t.apply(Command::Select(1)), vec![Action::Preload("02-second".into())]);
        assert_eq!(t.state(), State::Loading);

        assert_eq!(t.apply(Command::Loaded), vec![]);
        assert_eq!(t.state(), State::Armed);
        assert_eq!(t.current_song(), Some("02-second"));

        assert_eq!(t.apply(Command::Start), vec![Action::StartPlayback]);
        assert_eq!(t.state(), State::Playing);
    }

    #[test]
    fn stop_releases_and_rewinds_by_default() {
        let mut t = transport(false, true);
        t.apply(Command::Select(0));
        t.apply(Command::Loaded);
        t.apply(Command::Start);

        assert_eq!(
            t.apply(Command::Stop),
            vec![Action::ReleaseNotes, Action::StopPlayback, Action::SeekToZero]
        );
        assert_eq!(t.state(), State::Stopped);

        // Start after Stop restarts from 0 (no separate Restart command).
        assert_eq!(t.apply(Command::Start), vec![Action::StartPlayback]);
        assert_eq!(t.state(), State::Playing);
    }

    #[test]
    fn second_stop_panics_but_only_once() {
        let mut t = transport(false, true);
        t.apply(Command::Select(0));
        t.apply(Command::Loaded);
        t.apply(Command::Start);
        t.apply(Command::Stop); // first: clean release

        // Second Stop panics.
        assert_eq!(t.apply(Command::Stop), vec![Action::Panic]);
        assert_eq!(t.state(), State::Stopped);
        // Third+ Stop is a no-op — no re-spamming the panic burst.
        assert_eq!(t.apply(Command::Stop), vec![]);
        assert_eq!(t.apply(Command::Stop), vec![]);

        // Play, then the double-Stop panic is available again.
        assert_eq!(t.apply(Command::Start), vec![Action::StartPlayback]);
        t.apply(Command::Stop); // clean release
        assert_eq!(t.apply(Command::Stop), vec![Action::Panic]);
    }

    #[test]
    fn pause_in_place_when_rewind_disabled() {
        let mut t = transport(false, false);
        t.apply(Command::Select(0));
        t.apply(Command::Loaded);
        t.apply(Command::Start);

        // No SeekToZero: position is held.
        assert_eq!(
            t.apply(Command::Stop),
            vec![Action::ReleaseNotes, Action::StopPlayback]
        );
        // Start continues from where it stopped.
        assert_eq!(t.apply(Command::Start), vec![Action::StartPlayback]);
    }

    #[test]
    fn start_while_playing_restarts() {
        let mut t = transport(false, true);
        t.apply(Command::Select(0));
        t.apply(Command::Loaded);
        t.apply(Command::Start);

        assert_eq!(
            t.apply(Command::Start),
            vec![Action::ReleaseNotes, Action::SeekToZero]
        );
        assert_eq!(t.state(), State::Playing);
    }

    #[test]
    fn select_during_playback_arms_next_and_auto_advances() {
        let mut t = transport(true, true);
        t.apply(Command::Select(0));
        t.apply(Command::Loaded);
        t.apply(Command::Start);

        // Arm the next song mid-playback: current keeps playing.
        assert_eq!(t.apply(Command::Select(1)), vec![Action::Preload("02-second".into())]);
        assert_eq!(t.state(), State::Playing);
        assert_eq!(t.current_song(), Some("01-opener"));
        assert_eq!(t.armed_next_song(), Some("02-second"));

        // Preload completes, then the song ends -> gapless advance.
        t.apply(Command::Loaded);
        assert_eq!(
            t.apply(Command::EndReached),
            vec![Action::SeekToZero, Action::StartPlayback]
        );
        assert_eq!(t.state(), State::Playing);
        assert_eq!(t.current_song(), Some("02-second"));
        assert_eq!(t.armed_next_song(), None);
    }

    #[test]
    fn end_without_armed_next_goes_to_ended() {
        let mut t = transport(true, true);
        t.apply(Command::Select(0));
        t.apply(Command::Loaded);
        t.apply(Command::Start);

        assert_eq!(t.apply(Command::EndReached), vec![Action::StopPlayback]);
        assert_eq!(t.state(), State::Ended);

        // Start replays from the top.
        assert_eq!(
            t.apply(Command::Start),
            vec![Action::SeekToZero, Action::StartPlayback]
        );
    }

    #[test]
    fn next_prev_step_the_setlist() {
        let mut t = transport(false, true);
        // From Idle, Next arms the first entry.
        assert_eq!(t.apply(Command::Next), vec![Action::Preload("01-opener".into())]);
        t.apply(Command::Loaded);
        // Next again arms the second.
        assert_eq!(t.apply(Command::Next), vec![Action::Preload("02-second".into())]);
        t.apply(Command::Loaded);
        // Prev goes back to the first.
        assert_eq!(t.apply(Command::Prev), vec![Action::Preload("01-opener".into())]);
        t.apply(Command::Loaded);
        // Prev at index 0 is a no-op.
        assert_eq!(t.apply(Command::Prev), vec![]);
    }

    #[test]
    fn unknown_pc_is_ignored() {
        let mut t = transport(false, true);
        assert_eq!(t.apply(Command::Select(99)), vec![]);
        assert_eq!(t.state(), State::Idle);
    }
}
