//! The show/song data model (spec §7), as `serde` structs that deserialize
//! directly from `show.toml` / `song.toml`.
//!
//! The types mirror the TOML shape 1:1 so a bundle round-trips through
//! [`Show::from_toml_str`] / [`Song::from_toml_str`]. Semantic checks that TOML
//! typing can't express live in [`Show::validate`] / [`Song::validate`].

use std::collections::BTreeMap;
use std::path::Path;

use serde::{Deserialize, Serialize};

use crate::error::Error;

// ---------------------------------------------------------------------------
// show.toml
// ---------------------------------------------------------------------------

/// Top-level `show.toml`: setlist, routing, and global playback config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Show {
    pub show: ShowMeta,
    pub audio: Audio,
    #[serde(default)]
    pub destinations: Vec<Destination>,
    pub control: Control,
    #[serde(default)]
    pub setlist: Vec<SetlistEntry>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ShowMeta {
    pub name: String,
    /// Must match the audio device; the engine never resamples (§4).
    pub playback_rate: u32,
    /// Gapless setlist: start the armed-next song at `ENDED` (§8).
    #[serde(default)]
    pub auto_advance: bool,
    /// On **Stop**, reset the song pointer to 0 (§8). Default `true`.
    #[serde(default = "default_true")]
    pub rewind_on_stop: bool,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Audio {
    pub device: String,
    /// Large buffers are xrun-proof; latency is irrelevant (no monitoring path).
    #[serde(default = "default_buffer_frames")]
    pub buffer_frames: u32,
    /// Global audio-output latency (§9): how far the audio path lags the transport
    /// clock (buffer + DAC). Added to every destination's MIDI dispatch so cues
    /// line up with the *audible* audio; per-destination `offset_ms` trims from
    /// here. Tunable live.
    #[serde(default)]
    pub output_latency_ms: f64,
}

/// A logical MIDI destination -> physical port + latency offset (§5).
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Destination {
    pub name: String,
    pub port: String,
    /// Signed millisecond offset applied at dispatch; compensates mean latency.
    #[serde(default)]
    pub offset_ms: f64,
}

/// Incoming foot-controller map (§7.1). All entries are remappable.
///
/// `dsp` captures the open-ended `dsp_*` CC controls (e.g. `dsp_cutoff`,
/// `dsp_delay_mix`) via `#[serde(flatten)]`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Control {
    pub input_port: String,
    /// MIDI channel (1..=16) on which Program Change selects a song.
    pub select_channel: u8,
    /// Optional MIDI channel gate (1..=16) for the note bindings below
    /// (start/stop/next/prev/panic/mute). `None` = any channel (default,
    /// matches pre-existing behavior). Set this — together with
    /// `dsp_channel` — when transport and DSP CC come from different
    /// physical controllers merged onto one MIDI cable/port, so a stray
    /// message from one can't be misread as the other's.
    #[serde(default)]
    pub transport_channel: Option<u8>,
    /// Optional MIDI channel gate (1..=16) for every `dsp_*` CC binding.
    /// `None` = any channel (default).
    #[serde(default)]
    pub dsp_channel: Option<u8>,
    pub start: Binding,
    pub stop: Binding,
    pub next: Binding,
    pub prev: Binding,
    pub panic: Binding,
    /// Per-pair mute toggles: a single `notes = [..]` binding.
    pub mute: Binding,
    /// Remaining `dsp_*` CC bindings, keyed by their TOML key.
    #[serde(flatten)]
    pub dsp: BTreeMap<String, Binding>,
}

/// A control binding: `{ type = "note", note = 60 }`,
/// `{ type = "note", notes = [72, 73] }`, or `{ type = "cc", cc = 20 }`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Binding {
    #[serde(rename = "type")]
    pub kind: BindingKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<u8>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub notes: Option<Vec<u8>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cc: Option<u8>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum BindingKind {
    Note,
    Cc,
}

/// An ordered setlist entry binding a selection Program Change number to a song
/// directory name under `songs/`.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SetlistEntry {
    pub pc: u8,
    pub song: String,
}

// ---------------------------------------------------------------------------
// song.toml
// ---------------------------------------------------------------------------

/// Top-level `song.toml`: tempo, length, stem->pair map, per-pair DSP config.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Song {
    pub song: SongMeta,
    #[serde(default)]
    pub pairs: Vec<Pair>,
    /// Per-pair DSP config, keyed `pair0`, `pair1`, ... (`[dsp.pair0]`).
    #[serde(default)]
    pub dsp: BTreeMap<String, PairDsp>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SongMeta {
    pub name: String,
    /// Nominal tempo, used for tempo-synced delay (§6).
    pub bpm: f64,
    pub length_samples: u64,
}

/// One stereo stem pair (§4): up to 4 per song.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Pair {
    pub index: u8,
    pub file: String,
}

/// Per-pair native DSP defaults. Every param is live-CC driven at runtime (§6);
/// this only sets the fixed topology (e.g. filter type).
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct PairDsp {
    #[serde(default)]
    pub filter: Option<FilterKind>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum FilterKind {
    Lp,
    Hp,
    Bp,
}

// ---------------------------------------------------------------------------
// load + validate
// ---------------------------------------------------------------------------

impl Show {
    /// Parse a `show.toml` from a string (does not validate).
    pub fn from_toml_str(s: &str) -> Result<Self, Error> {
        Ok(toml::from_str(s)?)
    }

    /// Read and parse a `show.toml` from disk (does not validate).
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// Semantic checks beyond TOML typing (spec §7/§8). Storage-level checks
    /// (stem files present, format) belong to `turtle validate`.
    pub fn validate(&self) -> Result<(), Error> {
        let mut p = Vec::new();

        if !(1..=16).contains(&self.control.select_channel) {
            p.push(format!(
                "control.select_channel {} out of range 1..=16",
                self.control.select_channel
            ));
        }
        if let Some(ch) = self.control.transport_channel {
            if !(1..=16).contains(&ch) {
                p.push(format!(
                    "control.transport_channel {ch} out of range 1..=16"
                ));
            }
        }
        if let Some(ch) = self.control.dsp_channel {
            if !(1..=16).contains(&ch) {
                p.push(format!("control.dsp_channel {ch} out of range 1..=16"));
            }
        }
        if self.playback_rate() == 0 {
            p.push("show.playback_rate must be > 0".into());
        }
        if self.destinations.is_empty() {
            p.push("at least one [[destinations]] entry is required".into());
        }

        let mut seen_names = BTreeMap::new();
        for d in &self.destinations {
            if !d.offset_ms.is_finite() {
                p.push(format!("destination {}: offset_ms must be finite", d.name));
            }
            if seen_names.insert(&d.name, ()).is_some() {
                p.push(format!("duplicate destination name {:?}", d.name));
            }
        }

        // Named control bindings, each with its expected arity.
        self.control.start.check("control.start", &mut p);
        self.control.stop.check("control.stop", &mut p);
        self.control.next.check("control.next", &mut p);
        self.control.prev.check("control.prev", &mut p);
        self.control.panic.check("control.panic", &mut p);
        self.control.mute.check("control.mute", &mut p);
        for (key, b) in &self.control.dsp {
            b.check(key, &mut p);
        }

        let mut seen_pc = BTreeMap::new();
        for e in &self.setlist {
            if seen_pc.insert(e.pc, ()).is_some() {
                p.push(format!("duplicate setlist pc {}", e.pc));
            }
        }

        Error::from_problems(p)
    }

    fn playback_rate(&self) -> u32 {
        self.show.playback_rate
    }
}

impl Song {
    /// Parse a `song.toml` from a string (does not validate).
    pub fn from_toml_str(s: &str) -> Result<Self, Error> {
        Ok(toml::from_str(s)?)
    }

    /// Read and parse a `song.toml` from disk (does not validate).
    pub fn load(path: impl AsRef<Path>) -> Result<Self, Error> {
        let path = path.as_ref();
        let text = std::fs::read_to_string(path).map_err(|source| Error::Io {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_toml_str(&text)
    }

    /// Semantic checks: pair count/indices and non-zero length (§4).
    pub fn validate(&self) -> Result<(), Error> {
        let mut p = Vec::new();

        if self.song.bpm <= 0.0 || !self.song.bpm.is_finite() {
            p.push(format!("song.bpm must be > 0 (got {})", self.song.bpm));
        }
        if self.song.length_samples == 0 {
            p.push("song.length_samples must be > 0".into());
        }
        if self.pairs.len() > 4 {
            p.push(format!("at most 4 pairs allowed (got {})", self.pairs.len()));
        }

        let mut seen_idx = BTreeMap::new();
        for pair in &self.pairs {
            if pair.index > 3 {
                p.push(format!("pair index {} out of range 0..=3", pair.index));
            }
            if seen_idx.insert(pair.index, ()).is_some() {
                p.push(format!("duplicate pair index {}", pair.index));
            }
        }

        Error::from_problems(p)
    }
}

impl Binding {
    /// Check that the present fields match the declared `kind`.
    fn check(&self, ctx: &str, problems: &mut Vec<String>) {
        match self.kind {
            BindingKind::Note => {
                if self.note.is_none() && self.notes.is_none() {
                    problems.push(format!("{ctx}: note binding needs `note` or `notes`"));
                }
                if self.cc.is_some() {
                    problems.push(format!("{ctx}: note binding has a stray `cc`"));
                }
            }
            BindingKind::Cc => {
                if self.cc.is_none() {
                    problems.push(format!("{ctx}: cc binding needs `cc`"));
                }
                if self.note.is_some() || self.notes.is_some() {
                    problems.push(format!("{ctx}: cc binding has a stray `note`/`notes`"));
                }
            }
        }
    }
}

fn default_true() -> bool {
    true
}

fn default_buffer_frames() -> u32 {
    1024
}

#[cfg(test)]
mod tests {
    use super::*;

    // Mirrors the illustrative show.toml in spec §7.1.
    const SHOW_TOML: &str = r#"
[show]
name = "Spring Tour 2026"
playback_rate = 48000
auto_advance  = false
rewind_on_stop = true

[audio]
device = "hw:CARD=HXStomp"
buffer_frames = 1024

[[destinations]]
name = "lights"
port = "CME:1"
offset_ms = -8.0
[[destinations]]
name = "pedals"
port = "CME:2"
offset_ms = 0.0
[[destinations]]
name = "video"
port = "CME:3"
offset_ms = -20.0
[[destinations]]
name = "wear"
port = "CME:4"
offset_ms = 0.0

[control]
input_port   = "CME:in"
select_channel = 1
start   = { type = "note", note = 60 }
stop    = { type = "note", note = 61 }
next    = { type = "note", note = 62 }
prev    = { type = "note", note = 63 }
panic   = { type = "note", note = 65 }
mute    = { type = "note", notes = [72, 73, 74, 75] }
dsp_cutoff = { type = "cc", cc = 20 }
dsp_delay_mix = { type = "cc", cc = 21 }

[[setlist]]
pc = 0
song = "01-opener"
[[setlist]]
pc = 1
song = "02-second"
"#;

    // Mirrors the illustrative song.toml in spec §7.2.
    const SONG_TOML: &str = r#"
[song]
name = "Opener"
bpm  = 122.0
length_samples = 14112000

[[pairs]]
index = 0
file  = "stems/pair1.wav"
[[pairs]]
index = 1
file  = "stems/pair2.wav"

[dsp.pair0]
filter = "lp"
"#;

    #[test]
    fn parses_spec_show() {
        let show = Show::from_toml_str(SHOW_TOML).expect("parse");
        assert_eq!(show.show.playback_rate, 48000);
        assert!(show.show.rewind_on_stop);
        assert_eq!(show.destinations.len(), 4);
        assert_eq!(show.control.start.note, Some(60));
        assert_eq!(show.control.mute.notes.as_deref(), Some(&[72, 73, 74, 75][..]));
        // The two dsp_* keys land in the flattened map.
        assert_eq!(show.control.dsp.len(), 2);
        assert_eq!(show.control.dsp["dsp_cutoff"].cc, Some(20));
        assert_eq!(show.setlist.len(), 2);
        show.validate().expect("valid");
    }

    #[test]
    fn rewind_on_stop_defaults_true() {
        let toml = r#"
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
mute  = { type = "note", notes = [72] }
"#;
        let show = Show::from_toml_str(toml).expect("parse");
        assert!(show.show.rewind_on_stop, "default should be true");
        assert_eq!(show.audio.buffer_frames, 1024, "default buffer");
    }

    #[test]
    fn parses_spec_song() {
        let song = Song::from_toml_str(SONG_TOML).expect("parse");
        assert_eq!(song.pairs.len(), 2);
        assert_eq!(song.dsp["pair0"].filter, Some(FilterKind::Lp));
        song.validate().expect("valid");
    }

    #[test]
    fn rejects_bad_select_channel() {
        let show = Show::from_toml_str(&SHOW_TOML.replace("select_channel = 1", "select_channel = 0"))
            .expect("parse");
        assert!(show.validate().is_err(), "channel 0 must be rejected");
    }

    #[test]
    fn transport_and_dsp_channel_default_to_any() {
        // Neither is present in SHOW_TOML, so both should default to `None`
        // (any channel) rather than requiring every existing show to set them.
        let show = Show::from_toml_str(SHOW_TOML).expect("parse");
        assert_eq!(show.control.transport_channel, None);
        assert_eq!(show.control.dsp_channel, None);
    }

    #[test]
    fn parses_and_validates_transport_and_dsp_channel() {
        let toml = SHOW_TOML.replacen(
            "select_channel = 1",
            "select_channel = 1\ntransport_channel = 3\ndsp_channel = 4",
            1,
        );
        let show = Show::from_toml_str(&toml).expect("parse");
        assert_eq!(show.control.transport_channel, Some(3));
        assert_eq!(show.control.dsp_channel, Some(4));
        show.validate().expect("valid");
    }

    #[test]
    fn rejects_out_of_range_transport_and_dsp_channel() {
        let toml = SHOW_TOML.replacen(
            "select_channel = 1",
            "select_channel = 1\ntransport_channel = 17",
            1,
        );
        let show = Show::from_toml_str(&toml).expect("parse");
        assert!(show.validate().is_err(), "channel 17 must be rejected");

        let toml = SHOW_TOML.replacen(
            "select_channel = 1",
            "select_channel = 1\ndsp_channel = 0",
            1,
        );
        let show = Show::from_toml_str(&toml).expect("parse");
        assert!(show.validate().is_err(), "channel 0 must be rejected");
    }

    #[test]
    fn rejects_too_many_pairs() {
        let mut toml = String::from(
            r#"
[song]
name = "x"
bpm = 120.0
length_samples = 1000
"#,
        );
        for i in 0..5 {
            toml.push_str(&format!("[[pairs]]\nindex = {i}\nfile = \"s.wav\"\n"));
        }
        let song = Song::from_toml_str(&toml).expect("parse");
        assert!(song.validate().is_err(), "5 pairs must be rejected");
    }
}
