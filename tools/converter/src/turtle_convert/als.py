"""Parse an Ableton `.als` (gzipped XML) into the `ir.LiveSet` (spec §11).

`.als` is a moving target across Live versions; this is a best-effort parser
targeting the common element layout and is deliberately tolerant (missing
pieces are skipped, not fatal). It resolves:

  * nominal tempo from the master track,
  * audio tracks `t1`..`t4` -> stem pairs (via their arrangement clip's sample
    reference),
  * MIDI tracks named by destination -> notes (CC automation extraction is a
    known TODO — the IR/`midi_export` already handle CC when present).

**Not yet validated against real Live exports** — covered by a synthetic
fixture. Harden against actual `.als` files before relying on it.
"""

from __future__ import annotations

import gzip
import re
from pathlib import Path

from lxml import etree

from .ir import LiveSet, MidiDestTrack, Note, StemPair, TempoPoint

KNOWN_DESTINATIONS = {"lights", "pedals", "video", "wear"}
_STEM_NAME = re.compile(r"^t([1-4])$", re.IGNORECASE)


def load_als_xml(path: str | Path) -> etree._Element:
    raw = Path(path).read_bytes()
    if raw[:2] == b"\x1f\x8b":  # gzip magic
        raw = gzip.decompress(raw)
    return etree.fromstring(raw)


def parse_als(path: str | Path) -> LiveSet:
    root = load_als_xml(path)

    tempo = _find_tempo(root)
    tempo_map = [TempoPoint(beat=0.0, bpm=tempo)]

    stems: list[StemPair] = []
    midi_tracks: list[MidiDestTrack] = []
    unmapped: list[str] = []
    length_beats = 0.0

    for track in root.iterfind(".//Tracks/AudioTrack"):
        name = _track_name(track)
        m = _STEM_NAME.match(name or "")
        if not m:
            if name:
                unmapped.append(name)
            continue
        pair = _audio_track_to_pair(track, int(m.group(1)) - 1)
        if pair is not None:
            stems.append(pair)
            length_beats = max(length_beats, pair.end_beat)

    for track in root.iterfind(".//Tracks/MidiTrack"):
        name = _track_name(track)
        if not name or name.lower() not in KNOWN_DESTINATIONS:
            if name:
                unmapped.append(name)
            continue
        dest = _midi_track_to_dest(track, name.lower())
        midi_tracks.append(dest)
        for note in dest.notes:
            length_beats = max(length_beats, note.beat + note.duration_beats)

    stems.sort(key=lambda s: s.index)
    return LiveSet(
        tempo_map=tempo_map,
        length_beats=length_beats,
        stems=stems,
        midi_tracks=midi_tracks,
        unmapped_track_names=unmapped,
    )


def _find_tempo(root: etree._Element) -> float:
    # Modern Live: .../MasterTrack/.../Tempo/Manual Value="120"; fall back to any
    # Tempo/Manual in the document.
    for xpath in (".//MasterTrack//Tempo/Manual", ".//Tempo/Manual"):
        el = root.find(xpath)
        if el is not None and el.get("Value") is not None:
            try:
                return float(el.get("Value"))
            except ValueError:
                pass
    return 120.0


def _track_name(track: etree._Element) -> str | None:
    for xpath in (".//Name/EffectiveName", ".//Name/UserName"):
        el = track.find(xpath)
        if el is not None and el.get("Value"):
            return el.get("Value")
    return None


def _audio_track_to_pair(track: etree._Element, index: int) -> StemPair | None:
    clip = track.find(".//AudioClip")
    if clip is None:
        return None
    start = _float_attr(clip, "CurrentStart", "Value", 0.0)
    end = _float_attr(clip, "CurrentEnd", "Value", start)
    return StemPair(
        index=index,
        source_file=_sample_path(clip) or "",
        start_beat=start,
        end_beat=end,
    )


def _sample_path(clip: etree._Element) -> str | None:
    # Prefer an absolute/relative path element; fall back to the file name.
    for xpath in (
        ".//SampleRef//FileRef//Path",
        ".//SampleRef//FileRef//RelativePath",
        ".//SampleRef//FileRef//Name",
    ):
        el = clip.find(xpath)
        if el is not None and el.get("Value"):
            return el.get("Value")
    return None


def _midi_track_to_dest(track: etree._Element, name: str) -> MidiDestTrack:
    dest = MidiDestTrack(name=name)
    for clip in track.iterfind(".//MidiClip"):
        clip_start = _float_attr(clip, "CurrentStart", "Value", 0.0)
        for key_track in clip.iterfind(".//KeyTrack"):
            key_el = key_track.find(".//MidiKey")
            if key_el is None or key_el.get("Value") is None:
                continue
            key = int(key_el.get("Value"))
            for ev in key_track.iterfind(".//MidiNoteEvent"):
                beat = clip_start + float(ev.get("Time", "0"))
                dest.notes.append(
                    Note(
                        beat=beat,
                        duration_beats=float(ev.get("Duration", "0")),
                        key=key,
                        velocity=int(float(ev.get("Velocity", "100"))),
                    )
                )
    dest.notes.sort(key=lambda n: n.beat)
    return dest


def _float_attr(el: etree._Element, child: str, attr: str, default: float) -> float:
    found = el.find(child)
    if found is not None and found.get(attr) is not None:
        try:
            return float(found.get(attr))
        except ValueError:
            pass
    return default
