//! Decode incoming foot-controller MIDI into transport [`Command`]s (spec §8).
//!
//! Uses the show's `[control]` map (§7.1). Program Change (song select) and
//! the note bindings for start/stop/next/prev/panic decode to a [`Command`]
//! and go through the transport state machine ([`decode`]). Per-pair mute
//! ([`decode_mute`]) and live DSP CC ([`decode_dsp`]) are independent of
//! transport state, so they decode separately rather than through a
//! `Command`.
//!
//! `[control] transport_channel`/`dsp_channel` optionally gate decoding to
//! one MIDI channel each, split by role rather than by message type:
//! `transport_channel` covers only start/stop/next/prev/panic; `dsp_channel`
//! covers mute *and* the `dsp_*` CCs, since both are live mixing controls
//! rather than transport commands. Useful when transport and mixing come
//! from different physical controllers merged onto one MIDI cable/port, so a
//! stray message from one can't land on a binding meant for the other.
//! `None` (the default) matches any channel, same as before these existed.

use turtle_core::model::{Binding, BindingKind, Control};
use turtle_core::Command;

/// A live-CC-driven DSP parameter (§6), scoped to one pair by a
/// `dsp_pair{N}_{param}` control-map key (e.g. `dsp_pair0_cutoff`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DspParam {
    Gain,
    Cutoff,
    Resonance,
    DelayTime,
    DelayFeedback,
    DelayMix,
}

/// Extract the 1-based MIDI channel from a status byte.
fn midi_channel(status: u8) -> u8 {
    (status & 0x0F) + 1
}

/// `gate` is an optional required channel (`None` = any channel matches).
fn channel_matches(gate: Option<u8>, channel: u8) -> bool {
    gate.is_none_or(|required| required == channel)
}

/// Decode a 3-byte-ish channel message into a transport command, if it maps.
pub fn decode(control: &Control, status: u8, d1: u8, d2: u8) -> Option<Command> {
    let channel = midi_channel(status);
    match status & 0xF0 {
        0xC0 => {
            // Program Change on the select channel picks a song.
            if channel == control.select_channel {
                Some(Command::Select(d1))
            } else {
                None
            }
        }
        0x90 if d2 > 0 && channel_matches(control.transport_channel, channel) => {
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

/// Decode a note-on against the `mute` binding, returning the pair index
/// (the note's position in `mute.notes`) if it matches. A tap toggles that
/// pair's mute directly on the mixer — this bypasses the transport state
/// machine entirely, so it is decoded separately from [`decode`]. Gated by
/// `dsp_channel`, not `transport_channel`: mute is a live mixing control
/// (like the DSP CCs), not a transport command — `transport_channel` is
/// reserved for start/stop/next/prev/panic only.
pub fn decode_mute(control: &Control, status: u8, d1: u8, d2: u8) -> Option<usize> {
    if status & 0xF0 != 0x90 || d2 == 0 {
        return None;
    }
    if !channel_matches(control.dsp_channel, midi_channel(status)) {
        return None;
    }
    if control.mute.kind != BindingKind::Note {
        return None;
    }
    control.mute.notes.as_ref()?.iter().position(|&n| n == d1)
}

/// Decode an incoming CC against every `[control]` `dsp_*` binding, returning
/// every `(pair, param, raw 0..=127 value)` it drives. More than one binding
/// can share a CC number — e.g. mapping the same pedal to `dsp_pair0_delay_time`
/// through `dsp_pair3_delay_time` fans one move out to all four pairs — so
/// this returns *all* matches, not just the first. Live DSP is CC-only (§6)
/// and, like mute, bypasses the transport state machine entirely — a knob
/// move is valid in any state.
pub fn decode_dsp(control: &Control, status: u8, d1: u8, d2: u8) -> Vec<(usize, DspParam, u8)> {
    if status & 0xF0 != 0xB0 {
        return Vec::new();
    }
    if !channel_matches(control.dsp_channel, midi_channel(status)) {
        return Vec::new();
    }
    control
        .dsp
        .iter()
        .filter_map(|(key, binding)| {
            if binding.kind == BindingKind::Cc && binding.cc == Some(d1) {
                parse_dsp_key(key).map(|(pair, param)| (pair, param, d2))
            } else {
                None
            }
        })
        .collect()
}

/// Parse a `dsp_*` control-map key into the pair index and parameter it
/// drives. Convention: `dsp_pair{0..=3}_{param}`, e.g. `dsp_pair0_cutoff`.
/// Anything else (typos, out-of-range pair, unknown param) doesn't match and
/// is silently ignored by [`decode_dsp`] — consistent with how [`decode`]
/// already ignores unmapped notes/CCs rather than erroring at runtime.
fn parse_dsp_key(key: &str) -> Option<(usize, DspParam)> {
    let rest = key.strip_prefix("dsp_pair")?;
    let (pair_str, param_str) = rest.split_once('_')?;
    let pair: usize = pair_str.parse().ok()?;
    if pair > 3 {
        return None;
    }
    let param = match param_str {
        "gain" => DspParam::Gain,
        "cutoff" => DspParam::Cutoff,
        "resonance" => DspParam::Resonance,
        "delay_time" => DspParam::DelayTime,
        "delay_feedback" => DspParam::DelayFeedback,
        "delay_mix" => DspParam::DelayMix,
        _ => return None,
    };
    Some((pair, param))
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
dsp_pair0_cutoff = { type = "cc", cc = 20 }
dsp_pair0_delay_mix = { type = "cc", cc = 21 }
dsp_pair1_resonance = { type = "cc", cc = 22 }
dsp_pair0_delay_time = { type = "cc", cc = 30 }
dsp_pair1_delay_time = { type = "cc", cc = 30 }
dsp_pair2_delay_time = { type = "cc", cc = 30 }
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

    #[test]
    fn decodes_mute_notes_to_pair_index() {
        let c = control();
        assert_eq!(decode_mute(&c, 0x90, 72, 100), Some(0));
        assert_eq!(decode_mute(&c, 0x90, 73, 100), Some(1));
        assert_eq!(decode_mute(&c, 0x90, 74, 100), Some(2));
        assert_eq!(decode_mute(&c, 0x90, 75, 100), Some(3));
    }

    #[test]
    fn ignores_unmapped_mute_note_and_note_off() {
        let c = control();
        assert_eq!(decode_mute(&c, 0x90, 99, 100), None); // unmapped note
        assert_eq!(decode_mute(&c, 0x90, 72, 0), None); // note-on vel 0
        assert_eq!(decode_mute(&c, 0x80, 72, 0), None); // note-off
        assert_eq!(decode(&c, 0x90, 72, 100), None); // mute notes aren't transport Commands
    }

    #[test]
    fn decodes_dsp_cc_to_pair_and_param() {
        let c = control();
        assert_eq!(
            decode_dsp(&c, 0xB0, 20, 64),
            vec![(0, DspParam::Cutoff, 64)]
        );
        assert_eq!(
            decode_dsp(&c, 0xB0, 21, 100),
            vec![(0, DspParam::DelayMix, 100)]
        );
        assert_eq!(
            decode_dsp(&c, 0xB0, 22, 10),
            vec![(1, DspParam::Resonance, 10)]
        );
    }

    #[test]
    fn fans_one_cc_out_to_every_binding_that_shares_it() {
        // CC 30 drives dsp_pair{0,1,2}_delay_time — one pedal move should
        // update all three pairs, not just the lexicographically-first key.
        let c = control();
        assert_eq!(
            decode_dsp(&c, 0xB0, 30, 50),
            vec![
                (0, DspParam::DelayTime, 50),
                (1, DspParam::DelayTime, 50),
                (2, DspParam::DelayTime, 50),
            ]
        );
    }

    #[test]
    fn ignores_unmapped_cc_and_non_cc_status() {
        let c = control();
        assert!(decode_dsp(&c, 0xB0, 99, 64).is_empty()); // unmapped CC number
        assert!(decode_dsp(&c, 0x90, 20, 64).is_empty()); // note-on, not a CC status
        assert_eq!(decode(&c, 0xB0, 20, 64), None); // dsp CCs aren't transport Commands
    }

    const CHANNEL_GATED_SHOW: &str = r#"
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
transport_channel = 2
dsp_channel = 3
start = { type = "note", note = 60 }
stop  = { type = "note", note = 61 }
next  = { type = "note", note = 62 }
prev  = { type = "note", note = 63 }
panic = { type = "note", note = 65 }
mute  = { type = "note", notes = [72, 73, 74, 75] }
dsp_pair0_cutoff = { type = "cc", cc = 20 }
"#;

    fn channel_gated_control() -> Control {
        Show::from_toml_str(CHANNEL_GATED_SHOW).unwrap().control
    }

    #[test]
    fn transport_channel_gates_transport_notes_only() {
        let c = channel_gated_control();
        // 0x91 = note-on channel 2 (the configured transport_channel).
        assert_eq!(decode(&c, 0x91, 60, 100), Some(Command::Start));
        // 0x90 = note-on channel 1: same note, wrong channel, ignored.
        assert_eq!(decode(&c, 0x90, 60, 100), None);
        // Mute is NOT gated by transport_channel — it's a mixing control, not
        // a transport command, so it doesn't fire on the transport channel...
        assert_eq!(decode_mute(&c, 0x91, 72, 100), None);
    }

    #[test]
    fn dsp_channel_gates_mute_and_cc_but_select_channel_is_independent() {
        let c = channel_gated_control();
        // 0xB2 = CC channel 3 (the configured dsp_channel).
        assert_eq!(
            decode_dsp(&c, 0xB2, 20, 64),
            vec![(0, DspParam::Cutoff, 64)]
        );
        // 0xB0 = CC channel 1: same CC number, wrong channel, ignored.
        assert!(decode_dsp(&c, 0xB0, 20, 64).is_empty());
        // Mute shares dsp_channel (channel 3), not transport_channel: it's a
        // live mixing control alongside the DSP CCs. 0x92 = channel 3.
        assert_eq!(decode_mute(&c, 0x92, 72, 100), Some(0));
        // 0x91 = channel 2 (transport_channel, not dsp_channel): wrong.
        assert_eq!(decode_mute(&c, 0x91, 72, 100), None);
        // select_channel (song select, PC) is unaffected by either gate: PC
        // still only checks its own channel, 1 here.
        assert_eq!(decode(&c, 0xC0, 2, 0), Some(Command::Select(2)));
    }

    #[test]
    fn parses_dsp_key_convention() {
        assert_eq!(parse_dsp_key("dsp_pair0_gain"), Some((0, DspParam::Gain)));
        assert_eq!(
            parse_dsp_key("dsp_pair3_delay_time"),
            Some((3, DspParam::DelayTime))
        );
        assert_eq!(
            parse_dsp_key("dsp_pair2_delay_feedback"),
            Some((2, DspParam::DelayFeedback))
        );
        assert_eq!(parse_dsp_key("dsp_pair4_gain"), None); // pair out of 0..=3 range
        assert_eq!(parse_dsp_key("dsp_pair0_unknown"), None); // unknown param
        assert_eq!(parse_dsp_key("dsp_cutoff"), None); // old global-style key, no pair
    }
}
