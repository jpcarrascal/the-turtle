import numpy as np
import soundfile as sf

from turtle_convert.audio import convert_stem, resample_linear


def test_resample_changes_length_by_ratio():
    data = np.zeros((44_100, 2), dtype=np.float64)
    out = resample_linear(data, 44_100, 48_000)
    assert out.shape[0] == 48_000
    assert out.shape[1] == 2


def test_resample_noop_when_rates_match():
    data = np.random.randn(1000, 1)
    out = resample_linear(data, 48_000, 48_000)
    assert np.array_equal(out, data)


def test_convert_stem_writes_int24_at_target_rate(tmp_path):
    # A 44.1k sine written to disk, then converted to 48k int24.
    n = 44_100
    t = np.arange(n) / 44_100
    tone = 0.5 * np.sin(2 * np.pi * 440 * t)
    src = tmp_path / "in.wav"
    sf.write(str(src), np.column_stack([tone, tone]), 44_100, subtype="PCM_24")

    dst = tmp_path / "out" / "pair1.wav"
    src_rate = convert_stem(src, dst, 48_000)

    assert src_rate == 44_100
    info = sf.info(str(dst))
    assert info.samplerate == 48_000
    assert info.subtype == "PCM_24"
    assert abs(info.frames - 48_000) <= 1
