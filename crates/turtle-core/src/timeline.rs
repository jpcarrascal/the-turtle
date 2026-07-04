//! Timeline compilation (spec §5): a per-destination Standard MIDI File is
//! compiled into one time-sorted vector of `(sample_time, midi_bytes)` events.
//!
//! Tick→sample conversion follows the SMF's own tempo map (`set_tempo` meta
//! events), so timing is correct regardless of the PPQ / tempo the converter
//! (§11) chose to encode with. The show's playback rate fixes the sample grid.
//!
//! Per-destination latency offsets (§5) are **not** applied here — they are a
//! dispatch-time concern in `turtled`, since they are tuned live.

use midly::{MetaMessage, MidiMessage, Smf, Timing, TrackEventKind};

use crate::error::Error;

/// A raw MIDI channel-voice message (1–3 bytes), ready for ALSA rawmidi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RawMidi {
    bytes: [u8; 3],
    len: u8,
}

impl RawMidi {
    /// The on-wire bytes of the message.
    pub fn as_bytes(&self) -> &[u8] {
        &self.bytes[..self.len as usize]
    }

    /// The status byte (including channel nibble).
    pub fn status(&self) -> u8 {
        self.bytes[0]
    }
}

/// A MIDI message stamped with its absolute position on the show timeline.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct TimedEvent {
    pub sample_time: u64,
    pub message: RawMidi,
}

/// The compiled event vector for a single destination, sorted by `sample_time`.
#[derive(Debug, Clone, Default)]
pub struct Timeline {
    pub events: Vec<TimedEvent>,
}

impl Timeline {
    /// Compile a per-destination SMF into a sample-timed, sorted event vector.
    ///
    /// Only channel-voice messages (note/CC/PC/aftertouch/pitch-bend) are kept;
    /// meta and sysex are dropped (tempo metas are consumed for timing).
    pub fn compile_smf(smf_bytes: &[u8], sample_rate: u32) -> Result<Self, Error> {
        let smf = Smf::parse(smf_bytes).map_err(|e| Error::Midi(e.to_string()))?;

        let ppq = match smf.header.timing {
            Timing::Metrical(t) => t.as_int() as u64,
            Timing::Timecode(..) => {
                return Err(Error::Midi("SMPTE-timecode SMF is not supported".into()))
            }
        };
        if ppq == 0 {
            return Err(Error::Midi("SMF has zero ticks-per-quarter".into()));
        }

        // 1. Flatten every track to (absolute_tick, event), preserving in-track
        //    order for equal ticks via a stable sort.
        let mut flat: Vec<(u64, TrackEventKind)> = Vec::new();
        for track in &smf.tracks {
            let mut tick = 0u64;
            for ev in track {
                tick += ev.delta.as_int() as u64;
                flat.push((tick, ev.kind));
            }
        }
        flat.sort_by_key(|(tick, _)| *tick);

        // 2. Single walk in tick order, integrating elapsed time across tempo
        //    changes, converting each channel message to a sample position.
        let mut events = Vec::new();
        let mut last_tick = 0u64;
        let mut elapsed_sec = 0.0f64;
        let mut tempo_us_per_qn = 500_000.0f64; // MIDI default = 120 BPM

        for (tick, kind) in &flat {
            let dt_ticks = (tick - last_tick) as f64;
            elapsed_sec += dt_ticks * tempo_us_per_qn / (ppq as f64 * 1_000_000.0);
            last_tick = *tick;

            match kind {
                TrackEventKind::Meta(MetaMessage::Tempo(us)) => {
                    tempo_us_per_qn = us.as_int() as f64;
                }
                TrackEventKind::Midi { channel, message } => {
                    if let Some(message) = raw_from(channel.as_int(), *message) {
                        let sample_time = (elapsed_sec * sample_rate as f64).round() as u64;
                        events.push(TimedEvent {
                            sample_time,
                            message,
                        });
                    }
                }
                _ => {}
            }
        }

        // Elapsed time is monotonic in tick order, so events are already sorted;
        // a stable sort is a cheap safety net that also orders any co-timed ties.
        events.sort_by_key(|e| e.sample_time);
        Ok(Timeline { events })
    }
}

/// Convert a midly channel message into raw status+data bytes.
fn raw_from(channel: u8, message: MidiMessage) -> Option<RawMidi> {
    let ch = channel & 0x0F;
    let (status, d1, d2, len) = match message {
        MidiMessage::NoteOff { key, vel } => (0x80 | ch, key.as_int(), vel.as_int(), 3),
        MidiMessage::NoteOn { key, vel } => (0x90 | ch, key.as_int(), vel.as_int(), 3),
        MidiMessage::Aftertouch { key, vel } => (0xA0 | ch, key.as_int(), vel.as_int(), 3),
        MidiMessage::Controller { controller, value } => {
            (0xB0 | ch, controller.as_int(), value.as_int(), 3)
        }
        MidiMessage::ProgramChange { program } => (0xC0 | ch, program.as_int(), 0, 2),
        MidiMessage::ChannelAftertouch { vel } => (0xD0 | ch, vel.as_int(), 0, 2),
        MidiMessage::PitchBend { bend } => {
            let v = bend.0.as_int(); // 14-bit, 0..=16383
            (0xE0 | ch, (v & 0x7F) as u8, (v >> 7) as u8, 3)
        }
    };
    Some(RawMidi {
        bytes: [status, d1, d2],
        len,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use midly::num::{u15, u24, u28, u4, u7};
    use midly::{Format, Header, MetaMessage, MidiMessage, Smf, TrackEvent, TrackEventKind};

    /// A one-track SMF: 120 BPM, PPQ 480, a middle-C note held one quarter.
    fn quarter_note_smf() -> Vec<u8> {
        let track = vec![
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(500_000))),
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOn {
                        key: u7::new(60),
                        vel: u7::new(100),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(480),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOff {
                        key: u7::new(60),
                        vel: u7::new(0),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
            },
        ];
        let smf = Smf {
            header: Header {
                format: Format::SingleTrack,
                timing: Timing::Metrical(u15::new(480)),
            },
            tracks: vec![track],
        };
        let mut buf = Vec::new();
        smf.write_std(&mut buf).expect("write smf");
        buf
    }

    #[test]
    fn compiles_note_on_off_to_samples() {
        let tl = Timeline::compile_smf(&quarter_note_smf(), 48_000).expect("compile");
        assert_eq!(tl.events.len(), 2);

        assert_eq!(tl.events[0].sample_time, 0);
        assert_eq!(tl.events[0].message.as_bytes(), &[0x90, 60, 100]);

        // One quarter at 120 BPM = 0.5 s = 24000 samples at 48 kHz.
        assert_eq!(tl.events[1].sample_time, 24_000);
        assert_eq!(tl.events[1].message.as_bytes(), &[0x80, 60, 0]);
    }

    #[test]
    fn tempo_change_shifts_later_events() {
        // Same as above but double the tempo (250000 us = 240 BPM) right after
        // note-on: the quarter now lasts 0.25 s = 12000 samples.
        let track = vec![
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOn {
                        key: u7::new(60),
                        vel: u7::new(100),
                    },
                },
            },
            TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::Tempo(u24::new(250_000))),
            },
            TrackEvent {
                delta: u28::new(480),
                kind: TrackEventKind::Midi {
                    channel: u4::new(0),
                    message: MidiMessage::NoteOff {
                        key: u7::new(60),
                        vel: u7::new(0),
                    },
                },
            },
        ];
        let smf = Smf {
            header: Header {
                format: Format::SingleTrack,
                timing: Timing::Metrical(u15::new(480)),
            },
            tracks: vec![track],
        };
        let mut buf = Vec::new();
        smf.write_std(&mut buf).unwrap();

        let tl = Timeline::compile_smf(&buf, 48_000).unwrap();
        assert_eq!(tl.events[1].sample_time, 12_000);
    }

    #[test]
    fn rejects_smpte_timing() {
        // Craft a header with timecode timing.
        let smf = Smf {
            header: Header {
                format: Format::SingleTrack,
                timing: Timing::Timecode(midly::Fps::Fps25, 40),
            },
            tracks: vec![vec![TrackEvent {
                delta: u28::new(0),
                kind: TrackEventKind::Meta(MetaMessage::EndOfTrack),
            }]],
        };
        let mut buf = Vec::new();
        smf.write_std(&mut buf).unwrap();
        assert!(Timeline::compile_smf(&buf, 48_000).is_err());
    }
}
