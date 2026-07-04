from turtle_convert.ir import CCPoint, MidiDestTrack, Note
from turtle_convert.midi_export import DEFAULT_PPQ, build_destination_smf, thin_cc
from turtle_convert.timebase import TempoMap
from turtle_convert.ir import TempoPoint


def _abs_ticks(track):
    """Yield (absolute_tick, message) for a mido track."""
    t = 0
    for msg in track:
        t += msg.time
        yield t, msg


def test_note_ticks_follow_beats():
    dest = MidiDestTrack(name="lights", notes=[Note(beat=1.0, duration_beats=1.0, key=60, velocity=100)])
    tm = TempoMap([TempoPoint(0.0, 120.0)])
    mid = build_destination_smf(dest, tm)

    on = next(t for t, m in _abs_ticks(mid.tracks[0]) if m.type == "note_on")
    off = next(t for t, m in _abs_ticks(mid.tracks[0]) if m.type == "note_off")
    assert on == 1 * DEFAULT_PPQ  # beat 1
    assert off == 2 * DEFAULT_PPQ  # beat 2


def test_tempo_meta_is_emitted():
    dest = MidiDestTrack(name="lights", notes=[Note(0.0, 1.0, 60, 100)])
    tm = TempoMap([TempoPoint(0.0, 90.0)])
    mid = build_destination_smf(dest, tm)
    assert any(m.type == "set_tempo" for m in mid.tracks[0])


def test_thin_cc_bounds_message_rate():
    tm = TempoMap([TempoPoint(0.0, 120.0)])  # 0.5 s/beat -> 2 beats = 1 s
    # 400 dense points across 2 beats (~1 s) on controller 20.
    pts = [CCPoint(beat=i * (2.0 / 400), controller=20, value=i % 128) for i in range(400)]
    kept = thin_cc(pts, tm, max_hz=100.0)

    assert len(kept) < len(pts)
    # ~1 s of audio at <=100 Hz => at most ~101 points.
    assert len(kept) <= 101
    # Sorted by beat and preserves the controller.
    assert all(kept[i].beat <= kept[i + 1].beat for i in range(len(kept) - 1))
    assert all(p.controller == 20 for p in kept)


def test_thin_cc_keeps_latest_value_in_window():
    tm = TempoMap([TempoPoint(0.0, 120.0)])
    # Two points inside the same 10 ms window: the later value should win.
    pts = [CCPoint(beat=0.0, controller=7, value=10), CCPoint(beat=0.0005, controller=7, value=99)]
    kept = thin_cc(pts, tm, max_hz=100.0)
    assert len(kept) == 1
    assert kept[0].value == 99
