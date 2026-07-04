"""Ableton Live project -> Turtle show bundle converter (spec §11).

Pipeline (see `cli.py`): gunzip/parse `.als` -> intermediate representation
(`ir`) -> convert musical time to samples (`timebase`) -> emit per-destination
SMF with CC thinning (`midi_export`) -> resample stems to int24 WAV (`audio`)
-> write `show.toml` / `song.toml` (`bundle`), collecting warnings (`validate`).
"""

__version__ = "0.1.0"
