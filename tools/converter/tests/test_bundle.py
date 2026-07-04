import tomllib

from turtle_convert.bundle import (
    Destination,
    SongEntry,
    render_show_toml,
    render_song_toml,
)


def test_show_toml_round_trips_and_matches_model():
    text = render_show_toml(
        name="Spring Tour",
        playback_rate=48000,
        destinations=[Destination("lights", "CME:1", -8.0), Destination("pedals", "CME:2")],
        setlist=[SongEntry("01-opener", 0), SongEntry("02-second", 1)],
    )
    doc = tomllib.loads(text)

    assert doc["show"]["playback_rate"] == 48000
    assert doc["show"]["rewind_on_stop"] is True
    assert doc["audio"]["buffer_frames"] == 1024
    assert doc["destinations"][0] == {"name": "lights", "port": "CME:1", "offset_ms": -8.0}
    # Control map matches spec §7.1 shapes.
    assert doc["control"]["start"] == {"type": "note", "note": 60}
    assert doc["control"]["mute"] == {"type": "note", "notes": [72, 73, 74, 75]}
    assert doc["control"]["dsp_cutoff"] == {"type": "cc", "cc": 20}
    assert doc["setlist"][1] == {"pc": 1, "song": "02-second"}


def test_song_toml_round_trips():
    text = render_song_toml(
        name="Opener",
        bpm=122.0,
        length_samples=14112000,
        pairs=[(0, "stems/pair1.wav"), (1, "stems/pair2.wav")],
        filters={0: "lp"},
    )
    doc = tomllib.loads(text)
    assert doc["song"]["bpm"] == 122.0
    assert doc["song"]["length_samples"] == 14112000
    assert doc["pairs"][0] == {"index": 0, "file": "stems/pair1.wav"}
    assert doc["dsp"]["pair0"]["filter"] == "lp"
