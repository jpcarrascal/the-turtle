"""`turtle-convert` entry point: Live project folder -> Turtle show bundle (§11)."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from . import audio, bundle, midi_export, validate
from .als import parse_als
from .timebase import TempoMap


def convert_project(
    project_dir: str | Path,
    out_bundle: str | Path,
    playback_rate: int = 48000,
    song_dir_name: str = "01-song",
) -> list[str]:
    """Convert one `.als`-bearing project folder into a one-song bundle.

    Returns the list of validation warnings. Raises on fatal problems.
    """
    project_dir = Path(project_dir)
    out_bundle = Path(out_bundle)

    als = _find_als(project_dir)
    live = parse_als(als)
    tempo_map = TempoMap(live.tempo_map)

    if not live.stems:
        raise SystemExit(f"{als}: no stem tracks (t1-t4) found; nothing to convert")

    song_dir = out_bundle / "songs" / song_dir_name
    (song_dir / "stems").mkdir(parents=True, exist_ok=True)
    (song_dir / "midi").mkdir(parents=True, exist_ok=True)

    # --- stems: resample -> int24 WAV ---
    source_rates: dict[int, int] = {}
    pairs: list[tuple[int, str]] = []
    for stem in live.stems:
        src = project_dir / stem.source_file
        rel = f"stems/pair{stem.index + 1}.wav"
        dst = song_dir / rel
        if src.exists():
            source_rates[stem.index] = audio.convert_stem(src, dst, playback_rate)
        pairs.append((stem.index, rel))

    # --- MIDI: per-destination SMF with thinned CC ---
    for dest in live.midi_tracks:
        mid = midi_export.build_destination_smf(dest, tempo_map)
        mid.save(str(song_dir / "midi" / f"{dest.name}.mid"))

    # --- config: song.toml + show.toml ---
    length_samples = tempo_map.beat_to_samples(live.length_beats, playback_rate)
    (song_dir / "song.toml").write_text(
        bundle.render_song_toml(
            name=song_dir_name,
            bpm=live.nominal_bpm,
            length_samples=length_samples,
            pairs=pairs,
        )
    )

    destinations = [
        bundle.Destination(name=d.name, port=bundle.DEFAULT_PORTS.get(d.name, "CME:1"))
        for d in live.midi_tracks
    ]
    out_bundle.joinpath("show.toml").write_text(
        bundle.render_show_toml(
            name=out_bundle.stem,
            playback_rate=playback_rate,
            destinations=destinations or [bundle.Destination("lights", "CME:1")],
            setlist=[bundle.SongEntry(dir_name=song_dir_name, pc=0)],
        )
    )

    return validate.collect_warnings(live, project_dir, source_rates, playback_rate)


def _find_als(project_dir: Path) -> Path:
    als_files = sorted(project_dir.glob("*.als"))
    if not als_files:
        raise SystemExit(f"{project_dir}: no .als file found")
    return als_files[0]


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="turtle-convert", description=__doc__)
    parser.add_argument("project", help="Live project folder containing a .als")
    parser.add_argument("output", help="output bundle path (e.g. MyShow.turtle)")
    parser.add_argument("--rate", type=int, default=48000, help="playback rate (default 48000)")
    args = parser.parse_args(argv)

    warnings = convert_project(args.project, args.output, args.rate)
    for w in warnings:
        print(f"warning: {w}", file=sys.stderr)
    print(f"wrote bundle: {args.output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
