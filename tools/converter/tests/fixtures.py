"""Helpers to build a synthetic Ableton project for tests.

The `.als` XML here mirrors the element layout `als.parse_als` targets. It is
intentionally minimal but uses real Ableton element names so the parser is
exercised the way a real export would drive it.
"""

from __future__ import annotations

import gzip
from pathlib import Path

import numpy as np
import soundfile as sf

ALS_TEMPLATE = """<?xml version="1.0" encoding="UTF-8"?>
<Ableton MajorVersion="5" MinorVersion="12.0">
 <LiveSet>
  <MasterTrack>
   <DeviceChain><Mixer><Tempo><Manual Value="{bpm}"/></Tempo></Mixer></DeviceChain>
  </MasterTrack>
  <Tracks>
   <AudioTrack>
    <Name><EffectiveName Value="t1"/></Name>
    <AudioClip>
     <CurrentStart Value="0"/>
     <CurrentEnd Value="{length}"/>
     <SampleRef><FileRef><Path Value="stems/pair1.wav"/></FileRef></SampleRef>
    </AudioClip>
   </AudioTrack>
   <AudioTrack>
    <Name><EffectiveName Value="t2"/></Name>
    <AudioClip>
     <CurrentStart Value="0"/>
     <CurrentEnd Value="{length}"/>
     <SampleRef><FileRef><Path Value="stems/pair2.wav"/></FileRef></SampleRef>
    </AudioClip>
   </AudioTrack>
   <AudioTrack>
    <Name><EffectiveName Value="scratch"/></Name>
   </AudioTrack>
   <MidiTrack>
    <Name><EffectiveName Value="lights"/></Name>
    <MidiClip>
     <CurrentStart Value="0"/>
     <Notes><KeyTracks>
      <KeyTrack>
       <Notes>
        <MidiNoteEvent Time="0" Duration="1" Velocity="100"/>
        <MidiNoteEvent Time="2" Duration="1" Velocity="80"/>
       </Notes>
       <MidiKey Value="60"/>
      </KeyTrack>
     </KeyTracks></Notes>
    </MidiClip>
   </MidiTrack>
  </Tracks>
 </LiveSet>
</Ableton>
"""


def write_als(path: Path, bpm: float = 120.0, length: float = 8.0) -> Path:
    xml = ALS_TEMPLATE.format(bpm=bpm, length=length).encode("utf-8")
    path.write_bytes(gzip.compress(xml))
    return path


def make_project(project_dir: Path, bpm: float = 120.0, length: float = 8.0, src_rate: int = 44100) -> Path:
    """Create a project folder with a .als plus two real stem WAVs at src_rate."""
    project_dir.mkdir(parents=True, exist_ok=True)
    write_als(project_dir / "MySong.als", bpm=bpm, length=length)

    stems = project_dir / "stems"
    stems.mkdir(exist_ok=True)
    dur_s = length * 60.0 / bpm
    n = int(dur_s * src_rate)
    t = np.arange(n) / src_rate
    for name, freq in (("pair1.wav", 220.0), ("pair2.wav", 330.0)):
        tone = 0.2 * np.sin(2 * np.pi * freq * t)
        stereo = np.column_stack([tone, tone])
        sf.write(str(stems / name), stereo, src_rate, subtype="PCM_24")
    return project_dir
