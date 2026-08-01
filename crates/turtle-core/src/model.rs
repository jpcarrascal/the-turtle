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

/// Starting values for the shared delay bus (§6), from `show.toml`'s `[delay]`.
///
/// # Why these are CC values
///
/// The continuous controls are given as raw `0..=127` CC values rather than
/// engineering units, so a default and a live pedal move go through the *identical*
/// mapping. A second conversion path — percent, Hz, dB — is a second place for the
/// two to drift apart, and the whole point of a default is that it agrees with what
/// the knob does.
///
/// It also reads better than it sounds: the gain taper puts unity at CC **100**, so
/// `return = 100` is both "100" and unity gain.
///
/// `time` is the exception, and deliberately: a note division is a discrete musical
/// choice, so it is written as its own label (`"1/8"`).
///
/// # Why sends are not here
///
/// Per-pair sends stay at zero. The delay is meant to be silent until it is
/// intentionally added — a show that never touches a send never hears it, even
/// though these values are live.
#[derive(Debug, Clone, Copy, PartialEq, Deserialize, Serialize)]
pub struct DelayDefaults {
    /// Note division for the delay time, e.g. `"1/8"`.
    #[serde(default = "default_delay_time")]
    pub time: crate::timing::DelayDivision,
    /// Feedback CC: how many repeats. 64 is about half.
    #[serde(default = "default_delay_feedback")]
    pub feedback: u8,
    /// Return-level CC: how loud the echoes are. 100 is unity.
    #[serde(default = "default_delay_return")]
    pub r#return: u8,
    /// Cutoff CC for the output lowpass. The sweep is exponential, so 89 is about
    /// 2.5 kHz — a gently darkened echo rather than a muffled one.
    #[serde(default = "default_delay_cutoff")]
    pub cutoff: u8,
    /// Resonance CC. 0 is the floor (Q 0.5): no resonant peak at all.
    #[serde(default = "default_delay_resonance")]
    pub resonance: u8,
}

fn default_delay_time() -> crate::timing::DelayDivision {
    crate::timing::DelayDivision::Quarter
}
fn default_delay_feedback() -> u8 {
    64
}
fn default_delay_return() -> u8 {
    100
}
fn default_delay_cutoff() -> u8 {
    89
}
fn default_delay_resonance() -> u8 {
    0
}

impl Default for DelayDefaults {
    fn default() -> Self {
        DelayDefaults {
            time: default_delay_time(),
            feedback: default_delay_feedback(),
            r#return: default_delay_return(),
            cutoff: default_delay_cutoff(),
            resonance: default_delay_resonance(),
        }
    }
}

/// A song's `[delay]` table: per-field overrides of the show's defaults (§6).
///
/// # Why per-field rather than a whole table
///
/// A song that wants a longer delay should write one line, not restate every
/// setting. Anything left out falls through to `show.toml`, so the show carries
/// "how my delay is set up" and a song only records how it *differs*.
///
/// # Why per-song at all
///
/// The delay is already a per-song object in every other respect: its buffer is
/// sized from that song's BPM, its divisions are synced to that tempo, and a song
/// switch replaces the whole mixer — so the delay's state resets at a song boundary
/// whether or not this table exists. Show-level defaults were the odd one out; this
/// lets each song say what it resets *to*.
#[derive(Debug, Clone, Copy, Default, PartialEq, Deserialize, Serialize)]
pub struct DelayOverrides {
    pub time: Option<crate::timing::DelayDivision>,
    pub feedback: Option<u8>,
    pub r#return: Option<u8>,
    pub cutoff: Option<u8>,
    pub resonance: Option<u8>,
}

impl DelayOverrides {
    /// Layer these over the show's defaults, field by field.
    pub fn applied_to(&self, base: DelayDefaults) -> DelayDefaults {
        DelayDefaults {
            time: self.time.unwrap_or(base.time),
            feedback: self.feedback.unwrap_or(base.feedback),
            r#return: self.r#return.unwrap_or(base.r#return),
            cutoff: self.cutoff.unwrap_or(base.cutoff),
            resonance: self.resonance.unwrap_or(base.resonance),
        }
    }
}

/// Per-pair `dsp_pair{0..=3}_{param}` control-map parameters (§6).
///
/// Lives here, with the rest of the config schema, so `validate` can reject a
/// misspelled or obsolete key. `turtled`'s decoder has a matching `match`, and a
/// test there asserts every name in these lists parses — that is what stops the
/// two from drifting apart.
pub const DSP_PAIR_PARAMS: [&str; 4] = ["gain", "cutoff", "resonance", "send"];

/// Shared-delay-bus `dsp_delay_{param}` control-map parameters (§6). No pair
/// index: there is one delay.
pub const DSP_DELAY_PARAMS: [&str; 5] = ["time", "feedback", "return", "cutoff", "resonance"];

/// Is this a control-map `dsp_*` key the engine actually understands?
pub fn is_valid_dsp_key(key: &str) -> bool {
    if let Some(param) = key.strip_prefix("dsp_delay_") {
        return DSP_DELAY_PARAMS.contains(&param);
    }
    let Some(rest) = key.strip_prefix("dsp_pair") else {
        return false;
    };
    let Some((pair, param)) = rest.split_once('_') else {
        return false;
    };
    matches!(pair.parse::<usize>(), Ok(p) if p <= 3) && DSP_PAIR_PARAMS.contains(&param)
}

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
    /// Logical MIDI port aliases (§5/§7.1): alias -> ALSA card id, so a
    /// destination can say `port = "CME:1"` instead of repeating a full
    /// `hw:CARD=...` address. See [`crate::ports`]. Optional: a show that writes
    /// full ALSA addresses needs no table at all.
    #[serde(default)]
    pub ports: BTreeMap<String, String>,
    /// Starting values for the shared delay (§6). Absent means the built-in
    /// defaults, which are chosen to be immediately usable — see [`DelayDefaults`].
    #[serde(default)]
    pub delay: DelayDefaults,
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
/// `dsp` captures the open-ended `dsp_*` CC controls (e.g. `dsp_pair0_cutoff`,
/// `dsp_delay_return`) via `#[serde(flatten)]`. `validate` rejects any key that
/// is not one the engine understands — see [`is_valid_dsp_key`].
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Control {
    pub input_port: String,
    /// MIDI channel (1..=16) on which Program Change selects a song.
    pub select_channel: u8,
    /// Optional MIDI channel gate (1..=16) for the transport note bindings
    /// below (start/stop/next/prev/panic only — not `mute`). `None` = any
    /// channel (default, matches pre-existing behavior). Set this —
    /// together with `dsp_channel` — when transport and mixing come from
    /// different physical controllers merged onto one MIDI cable/port, so a
    /// stray message from one can't be misread as the other's.
    #[serde(default)]
    pub transport_channel: Option<u8>,
    /// Optional MIDI channel gate (1..=16) for `mute` and every `dsp_*` CC
    /// binding — both are live mixing controls, not transport commands.
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
    /// The song's stems, when it has no sections. Mutually exclusive with
    /// [`Song::sections`] — see [`Song::validate`].
    #[serde(default)]
    pub pairs: Vec<Pair>,
    /// Sections, each looping on its own until the next is triggered (§14).
    ///
    /// Ordered: the first is where playback starts unless another is selected.
    #[serde(default)]
    pub sections: Vec<Section>,
    /// Per-pair DSP config, keyed `pair0`, `pair1`, ... (`[dsp.pair0]`).
    #[serde(default)]
    pub dsp: BTreeMap<String, PairDsp>,
    /// This song's delay settings, overriding the show's field by field (§6).
    /// Absent means "use the show's".
    #[serde(default)]
    pub delay: DelayOverrides,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct SongMeta {
    pub name: String,
    /// Nominal tempo, used for tempo-synced delay (§6).
    pub bpm: f64,
    pub length_samples: u64,
    /// Repeat this song seamlessly until it is stopped, instead of ending and
    /// auto-advancing to the next setlist entry (§14).
    ///
    /// Named `looping` because `loop` is a Rust keyword; the TOML key is `loop`.
    /// Defaults to false, so existing songs are unaffected.
    #[serde(default, rename = "loop")]
    pub looping: bool,
}

/// One section of a song: an alternative set of stems that loops on its own (§14).
///
/// A song with sections is a live arrangement — an intro, a verse, a chorus — each
/// looping until the next is triggered. A song without them is just a song, and is
/// treated internally as a single unnamed section (see [`Song::effective_sections`])
/// so everything downstream has one code path rather than two.
#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Section {
    /// How the section is referred to — in a control binding, a log line, or
    /// `turtle status`.
    pub name: String,
    /// This section's stems. Sections may use different numbers of pairs.
    #[serde(default)]
    pub pairs: Vec<Pair>,
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
            // Catch an unresolvable port label here rather than at showtime: a
            // typo'd alias is a config error, and `turtle validate` is where
            // config errors should surface.
            if let Err(e) = self.resolve_port(&d.port) {
                p.push(format!("destination {}: {e}", d.name));
            }
        }
        if let Err(e) = self.resolved_input_port() {
            p.push(format!("control.input_port: {e}"));
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
            // An unknown `dsp_*` key is rejected rather than ignored. Silently
            // ignoring it is how a stale config leaves a pedal mysteriously dead
            // with nothing to blame — and the §6 rearchitecture renamed several,
            // so this is exactly the moment that matters.
            if !crate::model::is_valid_dsp_key(key) {
                p.push(format!(
                    "unknown control binding {key:?}: expected dsp_pair<0-3>_<{}> \
                     or dsp_delay_<{}>",
                    DSP_PAIR_PARAMS.join("|"),
                    DSP_DELAY_PARAMS.join("|")
                ));
            }
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

    /// Resolve one `port`/`input_port` value against this show's `[ports]` table.
    ///
    /// The single place any consumer should turn a configured port into something
    /// to hand ALSA, so `turtled` and `turtle doctor` cannot disagree about what a
    /// label means.
    pub fn resolve_port(&self, spec: &str) -> Result<String, crate::ports::PortError> {
        crate::ports::resolve(spec, &self.ports)
    }

    /// Every destination's resolved ALSA address, in order.
    ///
    /// Fails on the first unresolvable one; `validate` reports them all at once,
    /// so callers that have validated can treat this as infallible in practice.
    pub fn resolved_destination_ports(&self) -> Result<Vec<String>, crate::ports::PortError> {
        self.destinations
            .iter()
            .map(|d| self.resolve_port(&d.port))
            .collect()
    }

    /// The resolved control-input address.
    pub fn resolved_input_port(&self) -> Result<String, crate::ports::PortError> {
        self.resolve_port(&self.control.input_port)
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
        // Either form, never both: with both present it is genuinely ambiguous
        // which stems play, and guessing would be worse than refusing.
        if !self.pairs.is_empty() && !self.sections.is_empty() {
            p.push(
                "a song has either [[pairs]] or [[sections]], not both — \
                 move the pairs into a section"
                    .into(),
            );
        }

        check_pairs(&self.pairs, "song", &mut p);

        let mut seen_names = BTreeMap::new();
        for (i, section) in self.sections.iter().enumerate() {
            if section.name.trim().is_empty() {
                p.push(format!("section {i}: name must not be empty"));
            }
            // Names identify a section in bindings and logs, so duplicates would
            // make one of them unreachable.
            if seen_names.insert(section.name.clone(), ()).is_some() {
                p.push(format!("duplicate section name {:?}", section.name));
            }
            if section.pairs.is_empty() {
                p.push(format!(
                    "section {:?}: needs at least one [[sections.pairs]]",
                    section.name
                ));
            }
            check_pairs(&section.pairs, &format!("section {:?}", section.name), &mut p);
        }

        Error::from_problems(p)
    }

    /// The song's sections, treating a section-less song as one unnamed section.
    ///
    /// Lets everything downstream — loader, mixer, transport — work in terms of
    /// sections alone, instead of branching on whether a song happens to have them.
    /// Clones, but only at load time, never on the RT path.
    pub fn effective_sections(&self) -> Vec<Section> {
        if self.sections.is_empty() {
            vec![Section {
                name: self.song.name.clone(),
                pairs: self.pairs.clone(),
            }]
        } else {
            self.sections.clone()
        }
    }
}

/// Shared pair checks, so a section's pairs are held to the same rules as a
/// section-less song's (§4: at most 4, indices 0..=3, no duplicates).
fn check_pairs(pairs: &[Pair], ctx: &str, p: &mut Vec<String>) {
    if pairs.len() > 4 {
        p.push(format!("{ctx}: at most 4 pairs allowed (got {})", pairs.len()));
    }
    let mut seen_idx = BTreeMap::new();
    for pair in pairs {
        if pair.index > 3 {
            p.push(format!("{ctx}: pair index {} out of range 0..=3", pair.index));
        }
        if seen_idx.insert(pair.index, ()).is_some() {
            p.push(format!("{ctx}: duplicate pair index {}", pair.index));
        }
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

[ports]
CME = "H4MIDIWC"

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
input_port   = "CME:1"
select_channel = 1
start   = { type = "note", note = 60 }
stop    = { type = "note", note = 61 }
next    = { type = "note", note = 62 }
prev    = { type = "note", note = 63 }
panic   = { type = "note", note = 65 }
mute    = { type = "note", notes = [72, 73, 74, 75] }
dsp_pair0_cutoff = { type = "cc", cc = 20 }
dsp_delay_return = { type = "cc", cc = 21 }

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

    // ---- sections (§14) ----

    /// A section-less song is one implicit section, so everything downstream can
    /// assume sections exist without a legacy code path.
    #[test]
    fn a_song_without_sections_reads_as_one_section_named_after_the_song() {
        let song = Song::from_toml_str(SONG_TOML).expect("parse");
        let sections = song.effective_sections();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].name, "Opener");
        assert_eq!(sections[0].pairs.len(), 2);
        assert_eq!(sections[0].pairs[1].file, "stems/pair2.wav");
    }

    #[test]
    fn sections_parse_with_their_own_pairs_and_lengths() {
        let toml = r#"
[song]
name = "Arranged"
bpm  = 120.0
length_samples = 100

[[sections]]
name = "intro"
[[sections.pairs]]
index = 0
file  = "stems/intro_drums.wav"

[[sections]]
name = "chorus"
[[sections.pairs]]
index = 0
file  = "stems/chorus_drums.wav"
[[sections.pairs]]
index = 1
file  = "stems/chorus_gtr.wav"
"#;
        let song = Song::from_toml_str(toml).expect("parse");
        song.validate().expect("valid");
        let sections = song.effective_sections();
        assert_eq!(sections.len(), 2);
        assert_eq!(sections[0].name, "intro");
        assert_eq!(sections[0].pairs.len(), 1);
        assert_eq!(sections[1].name, "chorus");
        assert_eq!(sections[1].pairs.len(), 2);
        assert_eq!(sections[1].pairs[1].file, "stems/chorus_gtr.wav");
        // The song's own `[[pairs]]` stays empty — sections carry the stems.
        assert!(song.pairs.is_empty());
    }

    /// Both forms at once is ambiguous — which set plays? — so it is rejected
    /// rather than resolved by a precedence rule nobody would remember.
    #[test]
    fn pairs_and_sections_together_are_rejected() {
        let toml = r#"
[song]
name = "Both"
bpm  = 120.0
length_samples = 100

[[pairs]]
index = 0
file  = "stems/a.wav"

[[sections]]
name = "intro"
[[sections.pairs]]
index = 0
file  = "stems/b.wav"
"#;
        let song = Song::from_toml_str(toml).expect("parse");
        let err = song.validate().unwrap_err().to_string();
        assert!(err.contains("not both"), "unhelpful error: {err}");
    }

    #[test]
    fn sections_need_unique_non_empty_names() {
        let dup = r#"
[song]
name = "D"
bpm  = 120.0
length_samples = 100

[[sections]]
name = "verse"
[[sections.pairs]]
index = 0
file  = "stems/a.wav"

[[sections]]
name = "verse"
[[sections.pairs]]
index = 0
file  = "stems/b.wav"
"#;
        let err = Song::from_toml_str(dup).unwrap().validate().unwrap_err().to_string();
        assert!(err.contains("duplicate"), "unhelpful error: {err}");

        let empty = r#"
[song]
name = "E"
bpm  = 120.0
length_samples = 100

[[sections]]
name = ""
[[sections.pairs]]
index = 0
file  = "stems/a.wav"
"#;
        let err = Song::from_toml_str(empty).unwrap().validate().unwrap_err().to_string();
        assert!(err.contains("name"), "unhelpful error: {err}");
    }

    #[test]
    fn a_section_needs_at_least_one_pair() {
        let toml = r#"
[song]
name = "S"
bpm  = 120.0
length_samples = 100

[[sections]]
name = "silent"
"#;
        let err = Song::from_toml_str(toml).unwrap().validate().unwrap_err().to_string();
        assert!(err.contains("silent"), "error should name the section: {err}");
    }

    /// §4's pair rules are per-section, not per-song — a five-pair or
    /// duplicate-index section must fail the same way a song-level one does.
    #[test]
    fn section_pairs_obey_the_same_rules_as_song_pairs() {
        let toml = r#"
[song]
name = "S"
bpm  = 120.0
length_samples = 100

[[sections]]
name = "bad"
[[sections.pairs]]
index = 0
file  = "stems/a.wav"
[[sections.pairs]]
index = 0
file  = "stems/b.wav"
[[sections.pairs]]
index = 9
file  = "stems/c.wav"
"#;
        let err = Song::from_toml_str(toml).unwrap().validate().unwrap_err().to_string();
        assert!(err.contains("duplicate pair index 0"), "missing dup error: {err}");
        assert!(err.contains("out of range"), "missing range error: {err}");
        // And it says WHICH section, since a song now has several.
        assert!(err.contains("bad"), "error should name the section: {err}");
    }

    /// End-to-end: a `[ports]` table in a real show.toml must produce the ALSA
    /// addresses `turtled` will open, for both destinations and the control input.
    /// The unit tests in `ports` cover the rules; this covers the wiring.
    #[test]
    fn a_ports_table_resolves_destinations_and_the_control_input() {
        let toml = r#"
[show]
name = "T"
playback_rate = 48000

[audio]
device = "hw:CARD=L6"

[ports]
CME = "H4MIDIWC"

[[destinations]]
name = "lights"
port = "CME:1"
[[destinations]]
name = "pedals"
port = "CME:4"
[[destinations]]
name = "raw"
port = "hw:CARD=L6,DEV=0,SUBDEV=2"

[control]
input_port = "CME:2"
select_channel = 1
start = { type = "note", note = 60 }
stop  = { type = "note", note = 61 }
next  = { type = "note", note = 62 }
prev  = { type = "note", note = 63 }
panic = { type = "note", note = 65 }
mute  = { type = "note", notes = [72, 73, 74, 75] }
"#;
        let show: Show = toml::from_str(toml).expect("parses");
        show.validate().expect("valid");

        assert_eq!(
            show.resolved_destination_ports().unwrap(),
            vec![
                "hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0",
                "hw:CARD=H4MIDIWC,DEV=0,SUBDEV=3",
                // An explicit address alongside labels must survive untouched.
                "hw:CARD=L6,DEV=0,SUBDEV=2",
            ]
        );
        assert_eq!(
            show.resolved_input_port().unwrap(),
            "hw:CARD=H4MIDIWC,DEV=0,SUBDEV=1"
        );
    }

    /// A typo'd alias must fail `turtle validate`, not wait until showtime and
    /// surface as an ALSA "No such device".
    #[test]
    fn an_unresolvable_port_fails_validation() {
        let toml = r#"
[show]
name = "T"
playback_rate = 48000

[audio]
device = "hw:CARD=L6"

[ports]
CME = "H4MIDIWC"

[[destinations]]
name = "lights"
port = "CEM:1"

[control]
input_port = "CME:1"
select_channel = 1
start = { type = "note", note = 60 }
stop  = { type = "note", note = 61 }
next  = { type = "note", note = 62 }
prev  = { type = "note", note = 63 }
panic = { type = "note", note = 65 }
mute  = { type = "note", notes = [72, 73, 74, 75] }
"#;
        let show: Show = toml::from_str(toml).expect("parses");
        let err = show.validate().unwrap_err().to_string();
        assert!(err.contains("destination lights"), "{err}");
        assert!(err.contains("CEM"), "must name the bad alias: {err}");
        assert!(err.contains("known aliases: CME"), "must list valid ones: {err}");
    }

    /// The §6 rearchitecture renamed several `dsp_*` keys. A stale show.toml must
    /// fail `turtle validate` with the offending key named — silently ignoring it
    /// leaves a pedal mysteriously dead, which is the worst way to find out.
    #[test]
    fn obsolete_and_misspelled_dsp_keys_are_rejected() {
        for bad in [
            "dsp_pair0_delay_time",  // pre-rearchitecture: delay is a bus now
            "dsp_pair0_delay_mix",
            "dsp_cutoff",            // no pair index
            "dsp_pair4_gain",        // pair out of range
            "dsp_pair0_wobble",      // not a parameter
            "dsp_delay_mix",         // renamed to dsp_delay_return
        ] {
            assert!(!is_valid_dsp_key(bad), "{bad} should be rejected");
        }
        for good in [
            "dsp_pair0_gain",
            "dsp_pair3_send",
            "dsp_delay_time",
            "dsp_delay_resonance",
        ] {
            assert!(is_valid_dsp_key(good), "{good} should be accepted");
        }
    }

    /// A song's `[delay]` overrides the show's field by field: what it names wins,
    /// what it omits falls through. Restating the whole table to change one value
    /// would be the wrong ergonomics for the common case (one song wants a longer
    /// delay).
    #[test]
    fn song_delay_overrides_the_show_field_by_field() {
        let show = DelayDefaults {
            time: crate::timing::DelayDivision::Quarter,
            feedback: 64,
            r#return: 100,
            cutoff: 89,
            resonance: 0,
        };

        // Nothing specified: the show's values, unchanged.
        assert_eq!(DelayOverrides::default().applied_to(show), show);

        // Two fields specified: those two change, the rest do not.
        let song: Song = toml::from_str(
            "[song]\nname=\"b\"\nbpm=90.0\nlength_samples=4\n\n             [delay]\ntime = \"1/2\"\nfeedback = 90\n",
        )
        .unwrap();
        let merged = song.delay.applied_to(show);
        assert_eq!(merged.time, crate::timing::DelayDivision::Half);
        assert_eq!(merged.feedback, 90);
        assert_eq!(merged.r#return, show.r#return, "unspecified fields fall through");
        assert_eq!(merged.cutoff, show.cutoff);
        assert_eq!(merged.resonance, show.resonance);
    }

    /// A song with no `[delay]` at all must behave exactly as before this existed —
    /// the feature is additive, and every existing song.toml lacks the table.
    #[test]
    fn a_song_without_a_delay_table_uses_the_shows_settings() {
        let song: Song =
            toml::from_str("[song]\nname=\"a\"\nbpm=120.0\nlength_samples=4\n").unwrap();
        assert_eq!(song.delay, DelayOverrides::default());
        let show = DelayDefaults::default();
        assert_eq!(song.delay.applied_to(show), show);
    }

    /// A typo'd division in a *song* must be rejected at parse time with the valid
    /// names, exactly as in show.toml — the same type, so the same error.
    #[test]
    fn a_bad_division_in_a_song_is_rejected() {
        let err = toml::from_str::<Song>(
            "[song]\nname=\"a\"\nbpm=120.0\nlength_samples=4\n\n[delay]\ntime = \"1/3\"\n",
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown note division"), "{err}");
    }

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
        assert_eq!(show.control.dsp["dsp_pair0_cutoff"].cc, Some(20));
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
[ports]
CME = "H4MIDIWC"

[[destinations]]
name = "lights"
port = "CME:1"
[control]
input_port = "CME:1"
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
