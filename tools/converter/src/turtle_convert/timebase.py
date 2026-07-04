"""Musical-time -> sample-time conversion (spec §7/§11).

Mirrors the tick->sample integration in the Rust engine (`turtle-core`'s
`timeline`): elapsed seconds accrue segment-by-segment across tempo changes.
"""

from __future__ import annotations

import math

from .ir import TempoPoint


class TempoMap:
    def __init__(self, points: list[TempoPoint]):
        if not points:
            points = [TempoPoint(beat=0.0, bpm=120.0)]
        pts = sorted(points, key=lambda p: p.beat)
        # Guarantee a point at beat 0 so early events are covered.
        if pts[0].beat > 0.0:
            pts.insert(0, TempoPoint(beat=0.0, bpm=pts[0].bpm))
        self.points = pts

    def beat_to_seconds(self, beat: float) -> float:
        secs = 0.0
        pts = self.points
        for i, p in enumerate(pts):
            if beat <= p.beat:
                break
            seg_end = pts[i + 1].beat if i + 1 < len(pts) else math.inf
            span = min(beat, seg_end) - p.beat
            secs += span * 60.0 / p.bpm
            if beat <= seg_end:
                break
        return secs

    def beat_to_samples(self, beat: float, sample_rate: int) -> int:
        return round(self.beat_to_seconds(beat) * sample_rate)

    def seconds_between(self, beat_a: float, beat_b: float) -> float:
        return abs(self.beat_to_seconds(beat_b) - self.beat_to_seconds(beat_a))
