"""Emit `show.toml` / `song.toml` (spec §7), matching the Rust `turtle-core`
data model exactly so bundles round-trip through the engine's loader.

TOML is emitted by hand (small, fixed schema) rather than pulling in a writer
dependency. The default control map and destination ports mirror §7.1; the
performer edits ports/offsets after conversion.
"""

from __future__ import annotations

from dataclasses import dataclass

# Default logical destination -> physical port, in the spec's order (§7.1).
DEFAULT_PORTS = {
    "lights": "CME:1",
    "pedals": "CME:2",
    "video": "CME:3",
    "wear": "CME:4",
}


@dataclass
class Destination:
    name: str
    port: str
    offset_ms: float = 0.0


@dataclass
class SongEntry:
    dir_name: str  # e.g. "01-opener"
    pc: int


def render_show_toml(
    name: str,
    playback_rate: int,
    destinations: list[Destination],
    setlist: list[SongEntry],
    device: str = "hw:CARD=HXStomp",
    buffer_frames: int = 1024,
    auto_advance: bool = False,
    rewind_on_stop: bool = True,
) -> str:
    lines: list[str] = []
    lines += [
        "[show]",
        f'name = "{name}"',
        f"playback_rate = {playback_rate}",
        f"auto_advance = {_b(auto_advance)}",
        f"rewind_on_stop = {_b(rewind_on_stop)}",
        "",
        "[audio]",
        f'device = "{device}"',
        f"buffer_frames = {buffer_frames}",
        "",
    ]
    for d in destinations:
        lines += [
            "[[destinations]]",
            f'name = "{d.name}"',
            f'port = "{d.port}"',
            f"offset_ms = {float(d.offset_ms)}",
        ]
    lines.append("")
    # Default foot-controller map (§7.1); mute/dsp included so it validates.
    lines += [
        "[control]",
        'input_port = "CME:in"',
        "select_channel = 1",
        'start = { type = "note", note = 60 }',
        'stop = { type = "note", note = 61 }',
        'next = { type = "note", note = 62 }',
        'prev = { type = "note", note = 63 }',
        'panic = { type = "note", note = 65 }',
        "mute = { type = \"note\", notes = [72, 73, 74, 75] }",
        'dsp_cutoff = { type = "cc", cc = 20 }',
        'dsp_delay_mix = { type = "cc", cc = 21 }',
        "",
    ]
    for entry in setlist:
        lines += ["[[setlist]]", f"pc = {entry.pc}", f'song = "{entry.dir_name}"']
    return "\n".join(lines) + "\n"


def render_song_toml(
    name: str,
    bpm: float,
    length_samples: int,
    pairs: list[tuple[int, str]],  # (index, relative file path)
    filters: dict[int, str] | None = None,  # pair index -> "lp"/"hp"/"bp"
) -> str:
    lines = [
        "[song]",
        f'name = "{name}"',
        f"bpm = {float(bpm)}",
        f"length_samples = {length_samples}",
        "",
    ]
    for index, path in pairs:
        lines += ["[[pairs]]", f"index = {index}", f'file = "{path}"']
    lines.append("")
    for index, kind in sorted((filters or {}).items()):
        lines += [f"[dsp.pair{index}]", f'filter = "{kind}"']
    return "\n".join(lines) + "\n"


def _b(value: bool) -> str:
    return "true" if value else "false"
