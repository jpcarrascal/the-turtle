//! Decode incoming foot-controller MIDI into transport [`Command`]s (spec §8).
//!
//! Uses the show's `[control]` map (§7.1). Only transport-affecting messages
//! are decoded here: Program Change (song select) and the note bindings for
//! start/stop/next/prev/panic. DSP CC and per-pair mute are handled elsewhere.

use turtle_core::model::{Binding, BindingKind, Control};
use turtle_core::Command;

/// Decode a 3-byte-ish channel message into a transport command, if it maps.
pub fn decode(control: &Control, status: u8, d1: u8, d2: u8) -> Option<Command> {
    let channel = (status & 0x0F) + 1; // MIDI channels are 1-based in config
    match status & 0xF0 {
        0xC0 => {
            // Program Change on the select channel picks a song.
            if channel == control.select_channel {
                Some(Command::Select(d1))
            } else {
                None
            }
        }
        0x90 if d2 > 0 => {
            // Note-on: match against the transport note bindings, in priority
            // order.
            let n = d1;
            if note_matches(&control.start, n) {
                Some(Command::Start)
            } else if note_matches(&control.stop, n) {
                Some(Command::Stop)
            } else if note_matches(&control.next, n) {
                Some(Command::Next)
            } else if note_matches(&control.prev, n) {
                Some(Command::Prev)
            } else if note_matches(&control.panic, n) {
                Some(Command::Panic)
            } else {
                None
            }
        }
        _ => None,
    }
}

fn note_matches(binding: &Binding, note: u8) -> bool {
    binding.kind == BindingKind::Note
        && (binding.note == Some(note)
            || binding.notes.as_ref().is_some_and(|v| v.contains(&note)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use turtle_core::Show;

    const SHOW: &str = r#"
[show]
name = "x"
playback_rate = 48000
[audio]
device = "hw:0"
[[destinations]]
name = "lights"
port = "CME:1"
[control]
input_port = "CME:in"
select_channel = 1
start = { type = "note", note = 60 }
stop  = { type = "note", note = 61 }
next  = { type = "note", note = 62 }
prev  = { type = "note", note = 63 }
panic = { type = "note", note = 65 }
mute  = { type = "note", notes = [72, 73, 74, 75] }
"#;

    fn control() -> Control {
        Show::from_toml_str(SHOW).unwrap().control
    }

    #[test]
    fn decodes_program_change_on_select_channel() {
        // 0xC0 = PC on channel 1, program 2.
        assert_eq!(decode(&control(), 0xC0, 2, 0), Some(Command::Select(2)));
        // PC on channel 2 is ignored.
        assert_eq!(decode(&control(), 0xC1, 2, 0), None);
    }

    #[test]
    fn decodes_transport_notes() {
        let c = control();
        assert_eq!(decode(&c, 0x90, 60, 100), Some(Command::Start));
        assert_eq!(decode(&c, 0x90, 61, 100), Some(Command::Stop));
        assert_eq!(decode(&c, 0x90, 62, 100), Some(Command::Next));
        assert_eq!(decode(&c, 0x90, 63, 100), Some(Command::Prev));
        assert_eq!(decode(&c, 0x90, 65, 100), Some(Command::Panic));
    }

    #[test]
    fn ignores_unmapped_and_note_off() {
        let c = control();
        assert_eq!(decode(&c, 0x90, 99, 100), None); // unmapped note
        assert_eq!(decode(&c, 0x90, 60, 0), None); // note-on vel 0 (a note-off)
        assert_eq!(decode(&c, 0x80, 60, 0), None); // note-off
    }
}
