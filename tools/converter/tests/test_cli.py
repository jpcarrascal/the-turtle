import tomllib

import soundfile as sf
from fixtures import make_project

from turtle_convert.cli import convert_project


def test_end_to_end_conversion(tmp_path):
    project = make_project(tmp_path / "proj", bpm=120.0, length=8.0, src_rate=44_100)
    out = tmp_path / "MyShow.turtle"

    warnings = convert_project(project, out, playback_rate=48_000)

    # Bundle layout (spec §7).
    show = out / "show.toml"
    song = out / "songs" / "01-song" / "song.toml"
    assert show.exists() and song.exists()
    assert (out / "songs" / "01-song" / "midi" / "lights.mid").exists()

    # Stems resampled to 48k int24.
    pair1 = out / "songs" / "01-song" / "stems" / "pair1.wav"
    assert pair1.exists()
    info = sf.info(str(pair1))
    assert info.samplerate == 48_000
    assert info.subtype == "PCM_24"

    # song.toml has the right tempo/length and both pairs.
    doc = tomllib.loads(song.read_text())
    assert doc["song"]["bpm"] == 120.0
    # 8 beats @120 BPM = 4 s = 192000 samples at 48k.
    assert doc["song"]["length_samples"] == 192_000
    assert len(doc["pairs"]) == 2

    # The "scratch" audio track should surface as an unmapped-track warning.
    assert any("scratch" in w for w in warnings)


def test_show_toml_parses_as_valid_toml(tmp_path):
    project = make_project(tmp_path / "proj", bpm=120.0, length=4.0)
    out = tmp_path / "Show.turtle"
    convert_project(project, out)
    # Must be loadable (the Rust engine parses this same file).
    tomllib.loads((out / "show.toml").read_text())
