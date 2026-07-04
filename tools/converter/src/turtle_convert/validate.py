"""Conversion-time validation warnings (spec §11).

Warnings are advisory strings; the converter still produces a bundle. Fatal
problems (no stems at all) are raised by the CLI.
"""

from __future__ import annotations

from pathlib import Path

from .ir import LiveSet
from .timebase import TempoMap

# A DIN port is ~31.25 kBaud ≈ 1 ms per 3-byte message => ~1000 msg/s (§5).
DIN_MSG_PER_SEC = 1000.0


def collect_warnings(
    live: LiveSet,
    project_dir: str | Path,
    source_rates: dict[int, int],
    target_rate: int,
) -> list[str]:
    warnings: list[str] = []
    project_dir = Path(project_dir)

    for name in live.unmapped_track_names:
        warnings.append(f"unmapped track '{name}' (not t1-t4 or a known destination) — ignored")

    if not live.stems:
        warnings.append("no stem tracks (t1-t4) found")

    song_len = live.length_beats
    for stem in live.stems:
        src = (project_dir / stem.source_file) if stem.source_file else None
        if not stem.source_file or (src is not None and not src.exists()):
            warnings.append(f"stem t{stem.index + 1}: source file not found ({stem.source_file!r})")
        span = stem.end_beat - stem.start_beat
        if song_len and span + 1e-6 < song_len:
            warnings.append(
                f"stem t{stem.index + 1}: clip spans {span:.1f} beats, shorter than song ({song_len:.1f})"
            )

    for index, rate in source_rates.items():
        if rate != target_rate:
            warnings.append(
                f"stem t{index + 1}: source {rate} Hz resampled to {target_rate} Hz"
            )

    warnings.extend(_bandwidth_warnings(live))
    return warnings


def _bandwidth_warnings(live: LiveSet) -> list[str]:
    """Flag destinations whose peak message density exceeds a DIN port."""
    warnings: list[str] = []
    tempo_map = TempoMap(live.tempo_map)
    for dest in live.midi_tracks:
        # Count all message onsets (note on+off + cc) per 1-second window.
        times: list[float] = []
        for n in dest.notes:
            times.append(tempo_map.beat_to_seconds(n.beat))
            times.append(tempo_map.beat_to_seconds(n.beat + n.duration_beats))
        for c in dest.ccs:
            times.append(tempo_map.beat_to_seconds(c.beat))
        peak = _peak_per_second(times)
        if peak > DIN_MSG_PER_SEC:
            warnings.append(
                f"destination '{dest.name}': peak ~{peak:.0f} msg/s exceeds DIN budget "
                f"(~{DIN_MSG_PER_SEC:.0f}/s) — thin CC or split ports"
            )
    return warnings


def _peak_per_second(times: list[float]) -> float:
    if not times:
        return 0.0
    times = sorted(times)
    peak = 0
    start = 0
    for end in range(len(times)):
        while times[end] - times[start] > 1.0:
            start += 1
        peak = max(peak, end - start + 1)
    return float(peak)
