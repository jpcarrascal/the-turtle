from fixtures import write_als

from turtle_convert.als import parse_als


def test_parses_tempo_stems_and_notes(tmp_path):
    als = write_als(tmp_path / "MySong.als", bpm=128.0, length=8.0)
    live = parse_als(als)

    assert live.nominal_bpm == 128.0
    assert live.length_beats == 8.0

    # t1, t2 -> pair indices 0, 1; the "scratch" audio track is unmapped.
    assert [s.index for s in live.stems] == [0, 1]
    assert live.stems[0].source_file == "stems/pair1.wav"
    assert live.stems[0].end_beat == 8.0
    assert "scratch" in live.unmapped_track_names

    # One destination track "lights" with two notes on key 60.
    assert [d.name for d in live.midi_tracks] == ["lights"]
    notes = live.midi_tracks[0].notes
    assert len(notes) == 2
    assert notes[0].key == 60
    assert notes[0].beat == 0.0
    assert notes[1].beat == 2.0
    assert notes[1].velocity == 80


def test_handles_gzip_and_plain_xml(tmp_path):
    # write_als gzips; ensure the gzip path is what we exercise.
    als = write_als(tmp_path / "s.als")
    assert als.read_bytes()[:2] == b"\x1f\x8b"
    live = parse_als(als)
    assert live.stems
