"""Per-destination SMF emission with CC thinning (spec §5/§11).

Emits standard tick-based SMF carrying the tempo map, so the Rust engine's
timeline compiler (which follows the SMF tempo map) reproduces sample-accurate
timing. CC automation is thinned to a bandwidth budget before emission.
"""

from __future__ import annotations

from collections import defaultdict

import mido

from .ir import CCPoint, MidiDestTrack
from .timebase import TempoMap

DEFAULT_PPQ = 480
DEFAULT_CC_MAX_HZ = 100.0  # §5: <=100 Hz/CC default thinning


def thin_cc(
    points: list[CCPoint],
    tempo_map: TempoMap,
    max_hz: float = DEFAULT_CC_MAX_HZ,
) -> list[CCPoint]:
    """Decimate CC points to at most `max_hz` per controller.

    Time is bucketed into `1/max_hz`-second windows; within each window the
    last value for a controller wins (keeps the most recent value while bounding
    the message rate). Points are returned sorted by beat.
    """
    if max_hz <= 0:
        return list(points)
    min_dt = 1.0 / max_hz

    # controller -> bucket -> chosen point (last one seen in the bucket)
    per_ctrl: dict[int, dict[int, CCPoint]] = defaultdict(dict)
    for p in sorted(points, key=lambda c: c.beat):
        t = tempo_map.beat_to_seconds(p.beat)
        bucket = int(t / min_dt)
        per_ctrl[p.controller][bucket] = p

    kept: list[CCPoint] = []
    for buckets in per_ctrl.values():
        kept.extend(buckets.values())
    kept.sort(key=lambda c: c.beat)
    return kept


def build_destination_smf(
    dest: MidiDestTrack,
    tempo_map: TempoMap,
    ppq: int = DEFAULT_PPQ,
    cc_max_hz: float = DEFAULT_CC_MAX_HZ,
) -> mido.MidiFile:
    """Compile one destination's notes + thinned CC into a single-track SMF."""
    mid = mido.MidiFile(ticks_per_beat=ppq)
    track = mido.MidiTrack()
    mid.tracks.append(track)

    # (tick, order, message) — `order` keeps meta/tempo ahead of channel data
    # at identical ticks and gives a stable sort.
    events: list[tuple[int, int, mido.Message | mido.MetaMessage]] = []

    for tp in tempo_map.points:
        events.append(
            (round(tp.beat * ppq), 0, mido.MetaMessage("set_tempo", tempo=mido.bpm2tempo(tp.bpm)))
        )

    for cc in thin_cc(dest.ccs, tempo_map, cc_max_hz):
        events.append(
            (
                round(cc.beat * ppq),
                1,
                mido.Message("control_change", control=cc.controller, value=cc.value, channel=cc.channel),
            )
        )

    for n in dest.notes:
        on = round(n.beat * ppq)
        off = round((n.beat + n.duration_beats) * ppq)
        events.append((on, 2, mido.Message("note_on", note=n.key, velocity=n.velocity, channel=n.channel)))
        # note_off ordered before note_on at the same tick (order 1 < 2 above is
        # for CC; note_off uses -1 so a zero-length note still closes first).
        events.append((off, 3, mido.Message("note_off", note=n.key, velocity=0, channel=n.channel)))

    events.sort(key=lambda e: (e[0], e[1]))

    last_tick = 0
    for tick, _order, msg in events:
        msg = msg.copy(time=tick - last_tick)
        last_tick = tick
        track.append(msg)

    track.append(mido.MetaMessage("end_of_track", time=0))
    return mid


def message_count(mid: mido.MidiFile) -> int:
    """Number of channel-voice messages (for bandwidth estimation)."""
    return sum(1 for msg in mid if not msg.is_meta)
