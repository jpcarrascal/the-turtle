//! Logical MIDI port labels (spec §5/§7.1): `"CME:1"` instead of a raw ALSA name.
//!
//! # The problem this solves
//!
//! `show.toml` has to name MIDI ports, and the only names ALSA accepts look like
//! `hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0`. Two things are wrong with putting that in a
//! show file. It leaks the sound-system's addressing into what should be a
//! description of a *show*; and it repeats a card id on every destination, so
//! swapping interfaces means editing all of them.
//!
//! The spec's answer is a logical label. A `[ports]` table maps a short alias to a
//! card, and destinations refer to `"<alias>:<n>"`:
//!
//! ```toml
//! [ports]
//! CME = "H4MIDIWC"          # the card id from `turtle ports`
//!
//! [[destinations]]
//! name = "lights"
//! port = "CME:1"            # -> hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0
//! ```
//!
//! Swap to a different interface and one line changes.
//!
//! # Why the mapping is arithmetic, not a device lookup
//!
//! `"CME:1"` becomes `SUBDEV=0` by subtracting one — it does **not** enumerate the
//! hardware to find "the first port". Enumeration would be more faithful, but it
//! would mean this resolution could only happen on Linux with the card present,
//! which would push it out of `turtle-core` and make `turtle validate` unable to
//! check a show file on a laptop. Keeping it pure string arithmetic means the same
//! rules are testable everywhere and a typo is caught at validate time.
//!
//! The assumption is `DEV=0` and one subdevice per port, which is how both
//! multi-port interfaces on the dev rig actually enumerate (a 4-port CME and a
//! 3-port ZOOM: `DEV=0`, `SUBDEV=0..n`). When a device does not fit that shape,
//! the escape hatch needs no new syntax — write the full ALSA address in `port`
//! and it passes through untouched.
//!
//! # 1-based
//!
//! `CME:1` is the port the hardware calls "Port 1" and `turtle ports` lists first.
//! Matching the label on the box matters more than matching the `SUBDEV` number,
//! since the label is what you read while patching cables.

use std::collections::BTreeMap;
use std::fmt;

/// Why a port spec could not be resolved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PortError {
    /// `"CME:1"` where no `[ports]` entry defines `CME`.
    UnknownAlias { spec: String, alias: String, known: Vec<String> },
    /// `"CME:0"` or `"CME:-1"` — ports are numbered from 1.
    BadIndex { spec: String, index: String },
}

impl fmt::Display for PortError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            PortError::UnknownAlias { spec, alias, known } => {
                write!(f, "port {spec:?}: no [ports] entry named {alias:?}")?;
                if known.is_empty() {
                    write!(
                        f,
                        " (no [ports] table at all — add `[ports]\\n{alias} = \"<card-id>\"`, \
                         card ids from `turtle ports`)"
                    )
                } else {
                    write!(f, " (known aliases: {})", known.join(", "))
                }
            }
            PortError::BadIndex { spec, index } => write!(
                f,
                "port {spec:?}: {index:?} is not a port number — ports are numbered from 1, \
                 as `turtle ports` lists them"
            ),
        }
    }
}

impl std::error::Error for PortError {}

/// Does this look like something ALSA can already use verbatim?
///
/// Checked first so a real address is never mistaken for an alias reference —
/// `hw:CARD=L6,DEV=0` has a colon in it, and `hw` is not an alias.
fn is_alsa_name(spec: &str) -> bool {
    spec.starts_with("hw:")
        || spec.starts_with("plughw:")
        || spec.contains("CARD=")
        // No colon at all: a bare device name like `virtual` or `default`, which
        // ALSA resolves itself.
        || !spec.contains(':')
}

/// Resolve one `port` value against a `[ports]` table.
///
/// Passes anything already usable straight through, so existing show files keep
/// working untouched — this is additive, not a migration.
pub fn resolve(spec: &str, aliases: &BTreeMap<String, String>) -> Result<String, PortError> {
    if is_alsa_name(spec) {
        return Ok(spec.to_string());
    }

    // `alias:index`. `rsplit_once` so an alias containing a colon still splits at
    // the last one, where the index is.
    let (alias, index) = spec.rsplit_once(':').expect("is_alsa_name covers no-colon");

    let Some(card) = aliases.get(alias) else {
        // Deliberately an error rather than a pass-through. Handing `"CME:1"` to
        // ALSA produces "No such device", which says nothing about the real
        // mistake — a missing `[ports]` entry. This is exactly the spec's own
        // example, so it is the most likely thing someone writes by hand.
        return Err(PortError::UnknownAlias {
            spec: spec.to_string(),
            alias: alias.to_string(),
            known: aliases.keys().cloned().collect(),
        });
    };

    let n: u32 = index.parse().map_err(|_| PortError::BadIndex {
        spec: spec.to_string(),
        index: index.to_string(),
    })?;
    if n == 0 {
        return Err(PortError::BadIndex {
            spec: spec.to_string(),
            index: index.to_string(),
        });
    }

    // An alias may name a bare card id (`"H4MIDIWC"`) or, for a card whose ports
    // are not all on device 0, an explicit `hw:` prefix to extend.
    if card.starts_with("hw:") || card.starts_with("plughw:") {
        Ok(format!("{card},SUBDEV={}", n - 1))
    } else {
        Ok(format!("hw:CARD={card},DEV=0,SUBDEV={}", n - 1))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn aliases() -> BTreeMap<String, String> {
        let mut m = BTreeMap::new();
        m.insert("CME".to_string(), "H4MIDIWC".to_string());
        m.insert("L6".to_string(), "L6".to_string());
        m
    }

    /// The spec's own example, against the dev rig's real card id.
    #[test]
    fn an_alias_reference_resolves_to_a_full_stable_address() {
        let a = aliases();
        assert_eq!(
            resolve("CME:1", &a).unwrap(),
            "hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0"
        );
        assert_eq!(
            resolve("CME:4", &a).unwrap(),
            "hw:CARD=H4MIDIWC,DEV=0,SUBDEV=3"
        );
        assert_eq!(resolve("L6:2", &a).unwrap(), "hw:CARD=L6,DEV=0,SUBDEV=1");
    }

    /// 1-based, because that is what the hardware is labelled and what
    /// `turtle ports` prints. Off-by-one here would silently address the wrong
    /// physical socket, which is the worst kind of wrong.
    #[test]
    fn port_numbers_are_one_based_like_the_hardware_labels() {
        let a = aliases();
        assert!(resolve("CME:1", &a).unwrap().ends_with("SUBDEV=0"));
        // 0 is not a port; catching it beats silently addressing SUBDEV=-1.
        assert!(matches!(
            resolve("CME:0", &a),
            Err(PortError::BadIndex { .. })
        ));
    }

    /// Existing show files must keep working: this feature is additive, and a real
    /// ALSA address has a colon in it too, so it must not be read as an alias.
    #[test]
    fn real_alsa_names_pass_through_untouched() {
        let a = aliases();
        for name in [
            "hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0",
            "hw:0,0,0",
            "plughw:CARD=L6",
            "sysdefault:CARD=L6",
            "virtual",
            "default",
        ] {
            assert_eq!(resolve(name, &a).unwrap(), name, "{name} should pass through");
        }
    }

    /// The most likely hand-written mistake — the spec's example with no `[ports]`
    /// table — must name the fix, not hand `"CME:1"` to ALSA and report
    /// "No such device".
    #[test]
    fn an_unknown_alias_explains_what_to_add() {
        let empty = BTreeMap::new();
        let err = resolve("CME:1", &empty).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("no [ports] table at all"), "{msg}");
        assert!(msg.contains("CME = "), "must show the line to add: {msg}");
        assert!(msg.contains("turtle ports"), "must say where ids come from: {msg}");

        // With a table present, list what *is* defined — a typo is likelier than
        // a missing table once one exists.
        let err = resolve("CMEE:1", &aliases()).unwrap_err();
        let msg = err.to_string();
        assert!(msg.contains("known aliases: CME, L6"), "{msg}");
    }

    /// A non-numeric index is a typo worth catching, but must not swallow names
    /// that merely contain a colon and no number.
    #[test]
    fn a_non_numeric_index_is_reported_but_odd_names_pass_through() {
        let a = aliases();
        assert!(matches!(
            resolve("CME:one", &a),
            Err(PortError::BadIndex { .. })
        ));
        // Unknown alias *and* non-numeric: report the alias, which is the more
        // useful of the two complaints.
        assert!(matches!(
            resolve("mystery:thing", &a),
            Err(PortError::UnknownAlias { .. })
        ));
    }

    /// The documented escape hatch for a card whose ports are not all on DEV=0:
    /// point the alias at an explicit prefix and let `:n` extend it.
    #[test]
    fn an_alias_may_name_an_explicit_device_prefix() {
        let mut a = BTreeMap::new();
        a.insert("Odd".to_string(), "hw:CARD=Weird,DEV=2".to_string());
        assert_eq!(
            resolve("Odd:3", &a).unwrap(),
            "hw:CARD=Weird,DEV=2,SUBDEV=2"
        );
    }
}
