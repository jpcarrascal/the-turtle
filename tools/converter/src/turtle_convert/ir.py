"""Intermediate representation: the parsed-but-not-yet-compiled Live set.

Everything here is in *musical time* (beats). Conversion to samples happens in
`timebase`; nothing in the IR knows the playback rate.
"""

from __future__ import annotations

from dataclasses import dataclass, field


@dataclass
class TempoPoint:
    """A tempo in BPM effective from `beat` onward."""

    beat: float
    bpm: float


@dataclass
class Note:
    beat: float
    duration_beats: float
    key: int
    velocity: int
    channel: int = 0


@dataclass
class CCPoint:
    beat: float
    controller: int
    value: int  # 0..127
    channel: int = 0


@dataclass
class MidiDestTrack:
    """A MIDI track routed to a logical destination by its name (§11)."""

    name: str  # lights / pedals / video / wear
    notes: list[Note] = field(default_factory=list)
    ccs: list[CCPoint] = field(default_factory=list)


@dataclass
class StemPair:
    """An audio track `t1`..`t4` -> stereo stem pair 0..3 (§11)."""

    index: int  # 0..3
    source_file: str  # path as referenced by the Live set
    start_beat: float = 0.0
    end_beat: float = 0.0


@dataclass
class LiveSet:
    tempo_map: list[TempoPoint]
    length_beats: float
    stems: list[StemPair] = field(default_factory=list)
    midi_tracks: list[MidiDestTrack] = field(default_factory=list)
    unmapped_track_names: list[str] = field(default_factory=list)

    @property
    def nominal_bpm(self) -> float:
        """The first tempo point's BPM (used for tempo-synced delay, §6)."""
        return self.tempo_map[0].bpm if self.tempo_map else 120.0
