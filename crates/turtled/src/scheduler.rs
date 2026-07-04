//! Per-port MIDI event scheduling (spec §5).
//!
//! Each port owns a time-sorted `Vec<TimedEvent>` (compiled by
//! `turtle_core::Timeline`) and a cursor. On each scheduler wake, `drain_due`
//! returns the events whose `sample_time` has passed since the last call and
//! advances the cursor; `seek` repositions the cursor (rewind / restart).

use turtle_core::TimedEvent;

pub struct PortScheduler {
    events: Vec<TimedEvent>,
    cursor: usize,
}

impl PortScheduler {
    /// `events` must be sorted by `sample_time` (as `Timeline` produces).
    pub fn new(events: Vec<TimedEvent>) -> Self {
        PortScheduler { events, cursor: 0 }
    }

    /// Events with `sample_time <= pos` not yet returned, advancing the cursor.
    pub fn drain_due(&mut self, pos: u64) -> &[TimedEvent] {
        let start = self.cursor;
        while self.cursor < self.events.len() && self.events[self.cursor].sample_time <= pos {
            self.cursor += 1;
        }
        &self.events[start..self.cursor]
    }

    /// Reposition the cursor to the first event at or after `pos` (seek/rewind).
    pub fn seek(&mut self, pos: u64) {
        self.cursor = self.events.partition_point(|e| e.sample_time < pos);
    }

    pub fn reset(&mut self) {
        self.cursor = 0;
    }

    pub fn len(&self) -> usize {
        self.events.len()
    }

    pub fn is_empty(&self) -> bool {
        self.events.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use turtle_core::Timeline;

    // `RawMidi` has no public constructor, so borrow one real message from a
    // compiled SMF and reuse it as the payload for the timing tests.
    fn sample_message() -> turtle_core::RawMidi {
        use midly::num::{u15, u28, u4, u7};
        use midly::{Format, Header, MetaMessage, MidiMessage, Smf, Timing, TrackEvent, TrackEventKind};
        let track = vec![
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOn { key: u7::new(60), vel: u7::new(100) },
                },
            },
            TrackEvent { delta: u28::new(0), kind: TrackEventKind::Meta(MetaMessage::EndOfTrack) },
        ];
        let smf = Smf {
            header: Header { format: Format::SingleTrack, timing: Timing::Metrical(u15::new(480)) },
            tracks: vec![track],
        };
        let mut buf = Vec::new();
        smf.write_std(&mut buf).unwrap();
        Timeline::compile_smf(&buf, 48_000).unwrap().events[0].message
    }

    fn sched(times: &[u64]) -> PortScheduler {
        let m = sample_message();
        PortScheduler::new(
            times.iter().map(|&t| TimedEvent { sample_time: t, message: m }).collect(),
        )
    }

    #[test]
    fn drains_due_events_in_order() {
        let mut s = sched(&[0, 24_000, 48_000]);
        assert_eq!(s.drain_due(100).len(), 1); // event at 0
        assert_eq!(s.drain_due(23_000).len(), 0); // nothing new yet
        assert_eq!(s.drain_due(30_000).len(), 1); // event at 24000
        assert_eq!(s.drain_due(1_000_000).len(), 1); // event at 48000
        assert_eq!(s.drain_due(2_000_000).len(), 0); // exhausted
    }

    #[test]
    fn seek_repositions_cursor() {
        let mut s = sched(&[0, 24_000, 48_000]);
        s.drain_due(1_000_000); // consume everything
        s.seek(24_000);
        // After seeking to 24000, that event and the later one are due again.
        assert_eq!(s.drain_due(1_000_000).len(), 2);
    }

    #[test]
    fn reset_returns_to_start() {
        let mut s = sched(&[0, 24_000]);
        s.drain_due(1_000_000);
        s.reset();
        assert_eq!(s.drain_due(1_000_000).len(), 2);
    }
}
