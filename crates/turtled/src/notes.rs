//! Active-note tracking for clean Stop (spec §8).
//!
//! The scheduler feeds every dispatched channel message through [`ActiveNotes`].
//! On **Stop**, `release_all` yields note-offs for exactly the notes still
//! sounding — the Ableton-style clean release, as opposed to a full panic.

use std::collections::BTreeSet;

#[derive(Default)]
pub struct ActiveNotes {
    // (port, channel 0..15, note) currently sounding.
    active: BTreeSet<(usize, u8, u8)>,
}

impl ActiveNotes {
    pub fn new() -> Self {
        Self::default()
    }

    /// Observe a dispatched message so note state stays in sync.
    pub fn observe(&mut self, port: usize, status: u8, d1: u8, d2: u8) {
        let channel = status & 0x0F;
        match status & 0xF0 {
            0x90 => {
                // Note-on with velocity 0 is a note-off by convention.
                if d2 > 0 {
                    self.active.insert((port, channel, d1));
                } else {
                    self.active.remove(&(port, channel, d1));
                }
            }
            0x80 => {
                self.active.remove(&(port, channel, d1));
            }
            _ => {}
        }
    }

    /// Note-off messages `(port, bytes)` for every sounding note, then clears.
    pub fn release_all(&mut self) -> Vec<(usize, [u8; 3])> {
        let msgs = self
            .active
            .iter()
            .map(|&(port, channel, note)| (port, [0x80 | channel, note, 0]))
            .collect();
        self.active.clear();
        msgs
    }

    /// Drop all tracking without emitting anything (used on panic).
    pub fn clear(&mut self) {
        self.active.clear();
    }

    pub fn is_empty(&self) -> bool {
        self.active.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn releases_only_sounding_notes() {
        let mut n = ActiveNotes::new();
        n.observe(0, 0x90, 60, 100); // note-on ch0 note60 port0
        n.observe(1, 0x91, 64, 80); // note-on ch1 note64 port1
        n.observe(0, 0x80, 60, 0); // note-off ch0 note60 -> no longer sounding

        let released = n.release_all();
        assert_eq!(released, vec![(1, [0x81, 64, 0])]);
        assert!(n.is_empty());
    }

    #[test]
    fn note_on_velocity_zero_is_note_off() {
        let mut n = ActiveNotes::new();
        n.observe(0, 0x90, 60, 100);
        n.observe(0, 0x90, 60, 0); // vel 0 == off
        assert!(n.is_empty());
    }

    #[test]
    fn clear_emits_nothing() {
        let mut n = ActiveNotes::new();
        n.observe(0, 0x90, 60, 100);
        n.clear();
        assert!(n.release_all().is_empty());
    }
}
