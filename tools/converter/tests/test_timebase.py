from turtle_convert.ir import TempoPoint
from turtle_convert.timebase import TempoMap


def test_constant_tempo_beat_to_samples():
    tm = TempoMap([TempoPoint(0.0, 120.0)])
    # 1 beat at 120 BPM = 0.5 s = 24000 samples at 48k (matches the Rust side).
    assert tm.beat_to_samples(1.0, 48_000) == 24_000
    assert tm.beat_to_samples(4.0, 48_000) == 96_000


def test_tempo_change_integrates_segments():
    # 120 BPM for the first 4 beats, then 240 BPM.
    tm = TempoMap([TempoPoint(0.0, 120.0), TempoPoint(4.0, 240.0)])
    # 4 beats @120 = 2.0 s; next 4 beats @240 = 1.0 s => 3.0 s total.
    assert abs(tm.beat_to_seconds(8.0) - 3.0) < 1e-9
    assert tm.beat_to_samples(8.0, 48_000) == 144_000


def test_inserts_zero_point():
    tm = TempoMap([TempoPoint(4.0, 100.0)])
    assert tm.points[0].beat == 0.0
