"""Offline stem resampling to the show rate, written as int24 WAV (spec §11).

The engine never resamples (§4); the converter does it once here. This first
cut uses linear interpolation — adequate for scaffolding and correctness tests;
a band-limited (polyphase / windowed-sinc) resampler is a TODO before release.
"""

from __future__ import annotations

from pathlib import Path

import numpy as np
import soundfile as sf


def resample_linear(data: np.ndarray, src_rate: int, dst_rate: int) -> np.ndarray:
    """Resample `(frames, channels)` float audio from `src_rate` to `dst_rate`."""
    if src_rate == dst_rate:
        return data
    if data.ndim == 1:
        data = data[:, np.newaxis]
    n_src = data.shape[0]
    n_dst = int(round(n_src * dst_rate / src_rate))
    src_index = np.arange(n_src, dtype=np.float64)
    dst_index = np.arange(n_dst, dtype=np.float64) * (src_rate / dst_rate)
    out = np.empty((n_dst, data.shape[1]), dtype=np.float64)
    for ch in range(data.shape[1]):
        out[:, ch] = np.interp(dst_index, src_index, data[:, ch])
    return out


def convert_stem(src_path: str | Path, dst_path: str | Path, dst_rate: int) -> int:
    """Read a stem, resample to `dst_rate`, write int24 WAV. Returns source rate."""
    data, src_rate = sf.read(str(src_path), always_2d=True, dtype="float64")
    out = resample_linear(data, src_rate, dst_rate)
    # Clip to valid range so int24 conversion can't wrap on overshoots.
    np.clip(out, -1.0, 1.0, out=out)
    Path(dst_path).parent.mkdir(parents=True, exist_ok=True)
    sf.write(str(dst_path), out, dst_rate, subtype="PCM_24")
    return src_rate
