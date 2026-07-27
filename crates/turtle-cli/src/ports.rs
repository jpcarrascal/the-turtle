//! `turtle ports` — list audio and MIDI devices with their **stable** names.
//!
//! # Why this exists
//!
//! ALSA assigns card *indices* in enumeration order, so `hw:1,0,0` means a
//! different device after a replug or a reboot in a different order. That is not a
//! theoretical hazard: it repeatedly stopped the service from starting, because
//! `turtled` treats a missing MIDI input as fatal. Card *ids* are stable, so
//! `hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0` keeps meaning the same port forever.
//!
//! Working that string out by hand means reading `/proc/asound/cards` for the id,
//! `amidi -l` for the device/subdevice numbers, and knowing that ALSA's `hw:`
//! syntax accepts `CARD=` in place of an index. This prints it directly.
//!
//! # Why not the hint API
//!
//! [`crate::probe`] lists devices with `HintIter`, which enumerates **devices, not
//! ports** — on a 4-port interface it yields one entry (`hw:CARD=H4MIDIWC,DEV=0`)
//! and cannot tell Port 1 from Port 4. That is precisely the distinction needed
//! here, so this walks the control interface instead (`snd_ctl_rawmidi_info`,
//! which is what `amidi -l` uses) to reach individual subdevices.
//!
//! # Structure
//!
//! Enumeration is Linux-only; the [`Listing`] model and its rendering are
//! portable, so the output format is unit-tested on the dev Mac against
//! hand-built listings.

use std::fmt;

/// One card, with whatever ports it exposes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CardPorts {
    /// The stable id, as in `hw:CARD=<id>` and `/proc/asound/cards`' brackets.
    pub id: String,
    /// Human name, e.g. "H4MIDI-WC".
    pub name: String,
    /// Longer description, e.g. "CME Pro H4MIDI-WC at usb-...".
    pub description: String,
    /// True if the card has a playback PCM, i.e. it can be `[audio] device`.
    pub has_audio: bool,
    /// MIDI ports on this card.
    pub midi: Vec<MidiPort>,
}

/// One rawmidi port (a subdevice), merged across directions.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MidiPort {
    pub device: i32,
    pub subdevice: i32,
    /// The subdevice's own name, e.g. "H4MIDI-WC Port 1". This is what makes the
    /// numbers meaningful — `SUBDEV=0` alone does not tell you it is "Port 1".
    pub name: String,
    pub input: bool,
    pub output: bool,
}

impl MidiPort {
    /// The stable ALSA name to put in `show.toml`.
    ///
    /// Always fully qualified with `DEV` and `SUBDEV`. `SUBDEV` matters: it
    /// defaults to `-1` ("any"), so an unqualified name on a multi-port interface
    /// gives whichever port ALSA picks first — reproducible until it isn't.
    pub fn alsa_name(&self, card_id: &str) -> String {
        format!(
            "hw:CARD={card_id},DEV={},SUBDEV={}",
            self.device, self.subdevice
        )
    }

    /// `IO` / `I` / `O`, matching how `amidi -l` labels directions.
    fn direction(&self) -> &'static str {
        match (self.input, self.output) {
            (true, true) => "IO",
            (true, false) => "I ",
            (false, true) => " O",
            (false, false) => "  ",
        }
    }
}

/// Everything found, ready to render.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct Listing {
    pub cards: Vec<CardPorts>,
    /// Set when enumeration could not run at all (non-Linux host).
    pub unavailable: Option<String>,
}

impl fmt::Display for Listing {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        if let Some(why) = &self.unavailable {
            return write!(f, "cannot enumerate devices: {why}");
        }
        if self.cards.is_empty() {
            return write!(f, "no ALSA cards found — is the interface plugged in?");
        }

        writeln!(f, "Paste these into show.toml. They are stable across reboots and")?;
        writeln!(f, "replugs, unlike the hw:<index> form that `amidi -l` prints.")?;

        // The paste-able string comes **first** in every section, and every
        // address starts in the same column. Two reasons, both about it being a
        // copy-and-paste tool: a consistent position is one less thing to
        // re-locate between sections, and a left-aligned column of addresses can
        // be selected cleanly, which a column pushed rightwards by
        // variable-length descriptions cannot.
        //
        // Widths are measured rather than hardcoded, so the columns still line up
        // on hardware with longer card ids than the ones I had to hand.
        let audio: Vec<&CardPorts> = self.cards.iter().filter(|c| c.has_audio).collect();
        if !audio.is_empty() {
            let w = audio
                .iter()
                .map(|c| c.id.len() + "hw:CARD=".len())
                .max()
                .unwrap_or(0);
            writeln!(f, "\naudio  ([audio] device)")?;
            for c in audio {
                let addr = format!("hw:CARD={}", c.id);
                writeln!(f, "  {addr:<w$}  {}", c.description)?;
            }
        }

        let with_midi: Vec<&CardPorts> = self.cards.iter().filter(|c| !c.midi.is_empty()).collect();
        if with_midi.is_empty() {
            writeln!(f, "\nmidi   (none found)")?;
        } else {
            // One width across all cards, so the addresses line up down the whole
            // section rather than restarting per card.
            let w = with_midi
                .iter()
                .flat_map(|c| c.midi.iter().map(|p| p.alsa_name(&c.id).len()))
                .max()
                .unwrap_or(0);
            writeln!(f, "\nmidi   ([control] input_port, [[destinations]] port)")?;
            for c in with_midi {
                writeln!(f, "  {} — {}", c.id, c.description)?;
                for p in &c.midi {
                    let addr = p.alsa_name(&c.id);
                    writeln!(f, "    {addr:<w$}  {}  {}", p.direction(), p.name)?;
                }
            }
        }
        Ok(())
    }
}

impl Listing {
    /// Render ready-to-paste TOML for the first plausible setup.
    ///
    /// A starting point, not a finished config: it cannot know which port your
    /// lights are actually on. It picks the first audio card and the first MIDI
    /// port, and says so, rather than silently guessing at a whole setlist.
    pub fn to_toml_snippet(&self) -> String {
        let mut out = String::new();
        out.push_str("# Generated by `turtle ports --toml`. Check the port choices:\n");
        out.push_str("# the first MIDI port is used for both control input and the\n");
        out.push_str("# first destination, which is a guess, not a discovery.\n\n");

        match self.cards.iter().find(|c| c.has_audio) {
            Some(c) => {
                out.push_str(&format!("[audio]\ndevice = \"hw:CARD={}\"  # {}\n\n", c.id, c.name));
            }
            None => out.push_str("# no playback-capable card found\n\n"),
        }

        let first_midi = self
            .cards
            .iter()
            .find(|c| !c.midi.is_empty())
            .map(|c| (c, &c.midi[0]));
        match first_midi {
            Some((card, port)) => {
                out.push_str(&format!(
                    "[control]\ninput_port = \"{}\"  # {}\n\n",
                    port.alsa_name(&card.id),
                    port.name
                ));
                out.push_str(&format!(
                    "[[destinations]]\nname = \"lights\"\nport = \"{}\"  # {}\noffset_ms = 0.0\n",
                    port.alsa_name(&card.id),
                    port.name
                ));
            }
            None => out.push_str("# no MIDI ports found\n"),
        }
        out
    }
}

// ---------------------------------------------------------------------------
// Enumeration (Linux)
// ---------------------------------------------------------------------------

/// Walk every ALSA card, collecting its id, description, and MIDI ports.
#[cfg(target_os = "linux")]
pub fn enumerate() -> Listing {
    use alsa::ctl::Ctl;

    let mut cards = Vec::new();
    for card in alsa::card::Iter::new().filter_map(Result::ok) {
        // The control handle is the only route to the card *id* and to the
        // rawmidi subdevice list. A card we cannot open is skipped rather than
        // fatal — one unreadable device must not hide the rest.
        let Ok(ctl) = Ctl::from_card(&card, false) else { continue };
        let Ok(info) = ctl.card_info() else { continue };

        let id = info.get_id().unwrap_or("?").to_string();
        let name = info.get_name().unwrap_or("?").to_string();
        let description = info.get_longname().unwrap_or(&name).to_string();

        cards.push(CardPorts {
            id,
            name,
            description,
            has_audio: card_has_playback(&card),
            midi: midi_ports(&ctl),
        });
    }
    Listing { cards, unavailable: None }
}

/// MIDI ports on one card: read the raw per-direction entries, then merge.
///
/// Split in two so the merging — the part with real logic in it — is a portable
/// function that can be tested without ALSA. What is left here is a thin adapter
/// over the crate's iterator, which is the part that genuinely needs hardware.
#[cfg(target_os = "linux")]
fn midi_ports(ctl: &alsa::ctl::Ctl) -> Vec<MidiPort> {
    use alsa::Direction;

    let raw = alsa::rawmidi::Iter::new(ctl)
        .filter_map(Result::ok)
        .map(|info| {
            let device = info.get_device();
            let subdevice = info.get_subdevice();
            let name = info
                .get_subdevice_name()
                .ok()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| format!("device {device} port {subdevice}"));
            (device, subdevice, info.get_stream() == Direction::Capture, name)
        })
        .collect::<Vec<_>>();
    merge_ports(raw)
}

/// Merge per-direction rawmidi entries into one entry per physical port.
///
/// ALSA reports input and output streams separately, so a duplex port arrives
/// twice — once as capture, once as playback. Left unmerged, a four-port interface
/// would list as eight ports, and neither entry would say the port is `IO`.
///
/// Portable and unit-tested: this is where a mistake would actually be made,
/// whereas the ALSA iteration above is mechanical. Only *called* from the Linux
/// path, but kept available to `cfg(test)` so the dev Mac still tests the logic.
#[cfg(any(target_os = "linux", test))]
fn merge_ports(raw: impl IntoIterator<Item = (i32, i32, bool, String)>) -> Vec<MidiPort> {
    let mut ports: Vec<MidiPort> = Vec::new();
    for (device, subdevice, is_input, name) in raw {
        match ports
            .iter_mut()
            .find(|p| p.device == device && p.subdevice == subdevice)
        {
            Some(existing) => {
                existing.input |= is_input;
                existing.output |= !is_input;
                // Prefer a real subdevice name over the synthesised fallback, in
                // case only one direction reported one.
                if existing.name.starts_with("device ") && !name.starts_with("device ") {
                    existing.name = name;
                }
            }
            None => ports.push(MidiPort {
                device,
                subdevice,
                name,
                input: is_input,
                output: !is_input,
            }),
        }
    }
    // Sorted so the printed order matches `amidi -l` and is stable run to run.
    ports.sort_by_key(|p| (p.device, p.subdevice));
    ports
}

/// Does this card have a playback PCM, i.e. can it be `[audio] device`?
///
/// Answered from `/proc/asound/cardN/pcm*p` rather than by opening the device:
/// opening would fail with `EBUSY` on the card `turtled` is currently playing
/// through, which would hide the one card you most want listed.
#[cfg(target_os = "linux")]
fn card_has_playback(card: &alsa::card::Card) -> bool {
    let dir = format!("/proc/asound/card{}", card.get_index());
    std::fs::read_dir(dir).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|e| {
            e.file_name()
                .to_str()
                .is_some_and(|n| n.starts_with("pcm") && n.ends_with('p'))
        })
    })
}

/// Non-Linux stub: report honestly rather than printing an empty list that looks
/// like "no devices found".
#[cfg(not(target_os = "linux"))]
pub fn enumerate() -> Listing {
    Listing {
        cards: Vec::new(),
        unavailable: Some(format!(
            "ALSA is Linux-only (this host is {})",
            std::env::consts::OS
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A listing shaped like JP's actual Pi: a 4-port CME MIDI interface plus a
    /// ZOOM L6 audio interface that also carries MIDI.
    fn sample() -> Listing {
        Listing {
            cards: vec![
                CardPorts {
                    id: "H4MIDIWC".into(),
                    name: "H4MIDI-WC".into(),
                    description: "CME Pro H4MIDI-WC at usb-0000:01:00.0-1.3".into(),
                    has_audio: false,
                    midi: (0..4)
                        .map(|i| MidiPort {
                            device: 0,
                            subdevice: i,
                            name: format!("H4MIDI-WC Port {}", i + 1),
                            input: true,
                            output: true,
                        })
                        .collect(),
                },
                CardPorts {
                    id: "L6".into(),
                    name: "L6".into(),
                    description: "ZOOM Corporation L6 at usb-0000:01:00.0-1.2".into(),
                    has_audio: true,
                    // The real L6 exposes three MIDI ports alongside its audio.
                    midi: ["L6 MIDI I/O Port", "L6 Mixer Control Port", "L6 for L6 Editor Port"]
                        .iter()
                        .enumerate()
                        .map(|(i, n)| MidiPort {
                            device: 0,
                            subdevice: i as i32,
                            name: (*n).to_string(),
                            input: true,
                            output: true,
                        })
                        .collect(),
                },
            ],
            unavailable: None,
        }
    }

    /// The whole point: the printed string must be the stable `CARD=` form, fully
    /// qualified. An index-based or SUBDEV-less string here would recreate the bug
    /// this command exists to prevent.
    #[test]
    fn printed_names_are_stable_and_fully_qualified() {
        let l = sample();
        let out = l.to_string();
        assert!(out.contains("hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0"), "{out}");
        assert!(out.contains("hw:CARD=H4MIDIWC,DEV=0,SUBDEV=3"), "{out}");
        assert!(out.contains("hw:CARD=L6"), "{out}");
        // Never the fragile form.
        assert!(!out.contains("hw:0,"), "index-based name leaked: {out}");
        assert!(!out.contains("hw:1,"), "index-based name leaked: {out}");
    }

    /// `SUBDEV=0` on its own is meaningless to a person; the device's own port
    /// name is what makes the mapping usable.
    #[test]
    fn each_port_is_labelled_with_its_own_name() {
        let out = sample().to_string();
        for n in 1..=4 {
            assert!(out.contains(&format!("H4MIDI-WC Port {n}")), "{out}");
        }
    }

    /// Only playback-capable cards belong under `audio`, or the list suggests
    /// devices that cannot be `[audio] device`.
    #[test]
    fn only_playback_cards_are_offered_as_audio_devices() {
        let out = sample().to_string();
        // Split on the section header itself, not the bare word "midi" — the
        // preamble mentions `amidi -l`, which contains it.
        let audio_section = out.split("\nmidi ").next().unwrap();
        assert!(audio_section.contains("hw:CARD=L6"), "{audio_section}");
        assert!(
            !audio_section.contains("H4MIDIWC"),
            "a MIDI-only card must not be offered as an audio device: {audio_section}"
        );
    }

    /// The TOML snippet must be valid TOML that our own loader can parse — a
    /// generator emitting something `Show::load` rejects would be worse than no
    /// generator.
    #[test]
    fn the_toml_snippet_parses_as_toml() {
        let snippet = sample().to_toml_snippet();
        let parsed: toml::Value = toml::from_str(&snippet).expect("snippet must be valid TOML");
        assert_eq!(
            parsed["audio"]["device"].as_str(),
            Some("hw:CARD=L6"),
            "{snippet}"
        );
        assert_eq!(
            parsed["control"]["input_port"].as_str(),
            Some("hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0"),
            "{snippet}"
        );
        // It must admit that the port choice is a guess.
        assert!(snippet.contains("guess"), "{snippet}");
    }

    /// An empty result must not read as "your hardware is missing" when the real
    /// answer is "this host cannot look".
    #[test]
    fn a_non_linux_host_says_so_rather_than_showing_nothing() {
        let l = Listing {
            cards: Vec::new(),
            unavailable: Some("ALSA is Linux-only (this host is macos)".into()),
        };
        let out = l.to_string();
        assert!(out.contains("cannot enumerate"), "{out}");
        assert!(!out.contains("no ALSA cards found"), "{out}");

        // Whereas a Linux host with nothing plugged in *should* say that.
        let empty = Listing { cards: Vec::new(), unavailable: None };
        assert!(empty.to_string().contains("no ALSA cards found"));
    }

    /// The address is what you came to copy, so it leads every line in every
    /// section — and all of them start in the same column, which is what makes a
    /// column of them selectable. Regressing either would make this a worse
    /// copy-and-paste tool while still "working".
    #[test]
    fn every_line_leads_with_the_address_in_a_fixed_column() {
        let out = sample().to_string();
        let mut checked = 0;
        for line in out.lines() {
            let t = line.trim_start();
            if !t.starts_with("hw:CARD=") {
                continue;
            }
            // The address is first on the line, not trailing after a description.
            let indent = line.len() - t.len();
            let addr = t.split_whitespace().next().unwrap();
            // Every address in a section starts at the same column as its peers.
            assert!(
                line.starts_with(&" ".repeat(indent)),
                "unexpected indent on {line:?}"
            );
            assert!(
                addr.starts_with("hw:CARD="),
                "line must lead with the address: {line:?}"
            );
            checked += 1;
        }
        assert!(checked >= 8, "only checked {checked} address lines: {out}");

        // Columns line up: gather the MIDI lines and confirm the text after the
        // address begins at one consistent offset.
        let offsets: Vec<usize> = out
            .lines()
            .filter(|l| l.trim_start().starts_with("hw:CARD=") && l.contains("IO"))
            .map(|l| l.find("IO").unwrap())
            .collect();
        assert!(offsets.len() >= 7, "expected the midi rows: {out}");
        assert!(
            offsets.windows(2).all(|w| w[0] == w[1]),
            "direction column is ragged at offsets {offsets:?}:\n{out}"
        );
    }

    /// The merge is the one place real logic lives, and ALSA feeds it one entry
    /// per *direction*. This is the shape a 4-port duplex interface arrives in:
    /// four capture entries, then four playback entries.
    #[test]
    fn per_direction_entries_merge_into_one_port_each() {
        let mut raw: Vec<(i32, i32, bool, String)> = Vec::new();
        for sub in 0..4 {
            raw.push((0, sub, true, format!("H4MIDI-WC Port {}", sub + 1)));
        }
        for sub in 0..4 {
            raw.push((0, sub, false, format!("H4MIDI-WC Port {}", sub + 1)));
        }
        let ports = merge_ports(raw);
        assert_eq!(ports.len(), 4, "four physical ports, not eight: {ports:?}");
        for (i, p) in ports.iter().enumerate() {
            assert!(p.input && p.output, "port {i} should be duplex: {p:?}");
            assert_eq!(p.direction(), "IO");
            assert_eq!(p.subdevice, i as i32);
        }
    }

    /// An output-only or input-only port must not be reported as duplex — the
    /// direction is what tells you whether it can be a destination at all.
    #[test]
    fn single_direction_ports_keep_their_direction() {
        let ports = merge_ports(vec![
            (0, 0, false, "Out Only".into()),
            (0, 1, true, "In Only".into()),
        ]);
        assert_eq!(ports.len(), 2);
        assert_eq!((ports[0].input, ports[0].output), (false, true));
        assert_eq!(ports[0].direction(), " O");
        assert_eq!((ports[1].input, ports[1].output), (true, false));
        assert_eq!(ports[1].direction(), "I ");
    }

    /// Multiple devices on one card must stay distinct, and come out in a stable
    /// order regardless of how ALSA happened to yield them.
    #[test]
    fn separate_devices_are_not_merged_and_output_is_sorted() {
        let ports = merge_ports(vec![
            (1, 0, true, "Second device".into()),
            (0, 1, true, "First device port 2".into()),
            (0, 0, true, "First device port 1".into()),
        ]);
        assert_eq!(
            ports.iter().map(|p| (p.device, p.subdevice)).collect::<Vec<_>>(),
            vec![(0, 0), (0, 1), (1, 0)]
        );
    }

    /// If only one direction reports a usable name, that name should win over the
    /// synthesised fallback rather than depending on arrival order.
    #[test]
    fn a_real_name_beats_the_synthesised_fallback() {
        let ports = merge_ports(vec![
            (0, 0, true, "device 0 port 0".into()),
            (0, 0, false, "H4MIDI-WC Port 1".into()),
        ]);
        assert_eq!(ports[0].name, "H4MIDI-WC Port 1");
    }

    /// Duplex ports must appear once as `IO`, not twice — the underlying iterator
    /// yields one entry per direction, and an unmerged list would look like eight
    /// ports on a four-port interface.
    #[test]
    fn a_duplex_port_is_listed_once() {
        let out = sample().to_string();
        let occurrences = out.matches("hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0").count();
        assert_eq!(occurrences, 1, "duplex port listed {occurrences} times: {out}");
        assert!(out.contains("IO "), "direction should render as IO: {out}");
    }
}
