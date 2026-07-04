# The Turtle — System Specification

**Status:** v0.3 (draft, in active co-design)
**Last updated:** 2026-07-04

The Turtle is a headless, MIDI-controlled backing-track and MIDI-automation player
for live solo performance, built as a Raspberry Pi appliance. It replaces a laptop
running Ableton Live on stage: it plays multichannel backing stems and emits
sample-locked MIDI to lighting, wearables, guitar pedals, and video software, all
driven live from a foot controller.

---

## 1. Goals and non-goals

### Goals
- Play multichannel backing stems (up to 8 channels as 4 stereo pairs) with per-pair
  live mute/gain.
- Emit multiple independent MIDI streams (notes, PC, CC) tightly locked to the audio
  timeline, to distinct logical destinations (lighting, wearables, pedals, video).
- Run headless, boot-to-ready, controlled entirely by MIDI at performance time.
- Optional live DSP "knobs" (per-pair filter + delay) driven by incoming CC.
- CLI for configuration, preflight, and prep.
- A companion tool that converts an Ableton Live project into a Turtle show bundle.

### Non-goals (deliberately sacrificed)
- No plugin/effects host, no arbitrary Live devices.
- No audio warping / time-stretching (stems are pre-rendered at the show rate).
- No live monitoring path through the Pi (guitar/voice run their own analog chain).
- No native-DSP automation — the Pi's own filter/delay/gain are **live-CC only**.
  (External MIDI CC automation — e.g. light fades — is still supported; see §5.)

---

## 2. Hardware

- **Compute:** Raspberry Pi 4B, 4 GB.
- **Storage:** OS on SD (read-only / overlay root); **stems on a USB SSD**.
- **Audio out:** class-compliant USB audio interface **or** an I2S DAC HAT
  (HiFiBerry DAC2 Pro / Pisound recommended for a pedalboard build). The Pi's onboard
  3.5 mm PWM output is explicitly **not** used.
  - Initial interface: **Line 6 HX Stomp** (USB-class-compliant, **fixed 48 kHz**).
- **MIDI I/O:** a single USB class-compliant multi-port interface (**CME**), which fans
  out to wired DIN and BLE *downstream of the Pi*. The Pi never touches Bluetooth.
  Early bring-up may use the HX Stomp's single MIDI port.
- **Status/UI:** two GPIO LEDs (status + error) and a GPIO panic/stop button.

---

## 3. Architecture

A single daemon, `turtled`, with three long-lived threads plus a background loader.

| Thread | Priority | Responsibility |
|---|---|---|
| Audio RT | `SCHED_FIFO` | ALSA PCM loop: read preloaded stems → per-pair gain/mute → biquad → delay → sum → master limiter → device. Owns the master transport sample counter. No alloc/locks/syscalls. |
| MIDI scheduler | `SCHED_FIFO` | Wakes every ~0.5–1 ms (`timerfd`), reads interpolated transport position, dispatches due events via ALSA rawmidi. |
| Control | normal | Parses incoming foot-controller MIDI, runs the transport/setlist state machine, serves the CLI control socket, drives GPIO LEDs, tells the loader what to preload. |
| Loader | normal | Decodes/preloads song stems into RAM off the real-time path. |

Inter-thread communication is via lock-free SPSC command queues (`rtrb`). Only the
control and loader threads allocate.

### 3.1 The decoupled transport clock (key design point)

Each audio period, the RT thread atomically publishes `(sample_pos, monotonic_ns)`.
The MIDI scheduler interpolates the current position between periods:

```
pos = last_sample_pos + (now_ns - last_ns) * Fs / 1e9
```

This decouples MIDI timing granularity (the ~1 ms timer) from the audio buffer size.
We can therefore run **large, xrun-proof audio buffers** (there is no monitoring path,
so latency is irrelevant) *without* coarsening MIDI timing. Real-world MIDI jitter is
dominated by the ~1 ms timer plus DIN serialization, landing in the low single-digit
milliseconds — well within tolerance for kick-synced lighting and pedal/video cues.

---

## 4. Audio engine

- **Preload-to-RAM per song.** When a song is armed, its stems are fully decoded into
  RAM; nothing touches storage during playback. Budget @ 24-bit/48k/5 min/8ch
  ≈ 345 MB/song; current + armed-next ≈ 700 MB, comfortable in 4 GB.
- **Fixed playback rate per show** (`show.toml`), matching the audio device. The engine
  never resamples; the converter resamples source stems offline (see §11).
- **Channels:** up to 8, arranged as 4 stereo pairs; per song a subset may be used.
- **Format:** WAV int24 (converter output). WAV-only in v1.
- **Master bus:** sum of pairs → optional brickwall limiter → device out (stereo).

---

## 5. MIDI scheduling and output

- Internal event: `(sample_time, port_index, midi_bytes)`. Compiled at song load from
  the bundle's per-destination SMF into one time-sorted vector per port.
- **Per-output latency offset** (signed ms) applied at dispatch to align each
  destination against the audio. Compensates mean latency only (jitter, e.g. downstream
  BLE inside the CME box, is not compensable).
- **DIN bandwidth discipline:** a physical port is a 31.25 kBaud serial pipe (~1 ms per
  3-byte message). CC automation is thinned at conversion (≤100 Hz/CC default); the
  converter warns on budget overruns.
- **External CC automation is retained.** Light/wearable fades and any filter-sweep-as-CC
  to external gear are read from Live's automation and **baked into the destination SMF
  as timestamped CC events** at conversion time. This is distinct from native DSP (§6),
  which has no automation.
- **MIDI panic** command: all-notes-off + reset-all-controllers on every port.

---

## 6. Native DSP — live "knobs"

Fixed, preallocated per-pair chain (fixed topology = RT-deterministic):

- **gain + mute** (per pair)
- **one biquad** — type per pair (LP/HP/BP); params cutoff, resonance
- **one delay** — time (free ms or tempo-synced to the song's nominal BPM × division),
  feedback, mix
- **master:** output gain + optional limiter

Every parameter is driven **only** by a live incoming CC mapping (foot controller /
expression pedal). No envelopes. Transparent defaults so the chain is inaudible until a
knob is grabbed: delay mix 0, feedback 0; filter LP, minimal Q, cutoff 20 kHz; gain unity.

---

## 7. Show / Song data model

A **Turtle Show Bundle** is a directory — human-inspectable, DAW-portable:

```
MyShow.turtle/
  show.toml                 # setlist, global routing, per-output offsets, playback rate
  songs/
    01-opener/
      song.toml             # tempo, length, stem→pair map, DSP config
      stems/  pair1.wav ...  # int24 @ show rate, up to 4 pairs
      midi/   lights.mid pedals.mid video.mid wear.mid   # SMF per destination
```

- Config = TOML (`serde`); MIDI = standard SMF per destination (editable in any DAW).
  The engine compiles SMF → internal sample-timed vectors at load.
- **Time base:** samples internally, at the show's playback rate.
- No native-DSP envelope files (live-only DSP).

### 7.1 `show.toml` (illustrative)

```toml
[show]
name = "Spring Tour 2026"
playback_rate = 48000          # must match the audio device
auto_advance  = false          # true = gapless setlist (start next armed song at end)
rewind_on_stop = true          # true = Stop resets song pointer to 0; false = pause in place

[audio]
device = "hw:CARD=HXStomp"
buffer_frames = 1024           # large = xrun-proof; latency irrelevant

[[destinations]]               # logical destination -> physical MIDI port + offset
name = "lights"
port = "CME:1"
offset_ms = -8.0
[[destinations]]
name = "pedals"
port = "CME:2"
offset_ms = 0.0
[[destinations]]
name = "video"
port = "CME:3"
offset_ms = -20.0
[[destinations]]
name = "wear"
port = "CME:4"
offset_ms = 0.0

[control]                      # incoming foot-controller map (all remappable)
input_port   = "CME:in"
select_channel = 1             # Program Change selects song
start   = { type = "note", note = 60 }
stop    = { type = "note", note = 61 }
next    = { type = "note", note = 62 }
prev    = { type = "note", note = 63 }
restart = { type = "note", note = 64 }
panic   = { type = "note", note = 65 }
mute    = { type = "note", notes = [72, 73, 74, 75] }   # per-pair toggle
dsp_cutoff = { type = "cc", cc = 20 }
dsp_delay_mix = { type = "cc", cc = 21 }

[[setlist]]                    # ordered songs, each bound to a selection PC number
pc = 0
song = "01-opener"
[[setlist]]
pc = 1
song = "02-second"
```

### 7.2 `song.toml` (illustrative)

```toml
[song]
name = "Opener"
bpm  = 122.0                   # nominal, for tempo-synced delay
length_samples = 14112000

[[pairs]]
index = 0
file  = "stems/pair1.wav"
[[pairs]]
index = 1
file  = "stems/pair2.wav"
# ... up to 4

[dsp.pair0]
filter = "lp"                  # default transparent; live CC drives cutoff/Q
```

---

## 8. Control surface and state machine

**States:** `IDLE → LOADING → ARMED → PLAYING → (STOPPED | ENDED) → ARMED/IDLE`.

Selecting a song *arms* it (background preload) so **Start** is instant. Selecting during
playback arms the next without interrupting the current song. v1 = manual per-song start;
with `auto_advance = true`, `ENDED` immediately starts the armed next song (gapless, no
crossfade — crossfade is future).

**Stop behavior:** on **Stop**, the song (and MIDI) pointer resets to sample 0 by
default — the song re-arms at the top rather than pausing in place. This is governed by
`rewind_on_stop` in `show.toml` (default `true`); setting it `false` restores
pause-in-place semantics, where **Start** after **Stop** continues from the stopped
position instead of restarting.

Default command map (all remappable, see §7.1):

| Function | Default |
|---|---|
| Select song | Program Change on `select_channel` |
| Start / Continue | Note 60 |
| Stop | Note 61 |
| Next / Prev (arm) | Note 62 / 63 |
| Restart song | Note 64 |
| MIDI panic | Note 65 |
| Per-pair mute toggle | Notes 72–75 |
| DSP live params | CC 20, 21, ... |

### 8.1 GPIO status

| Signal | State |
|---|---|
| Status LED off | Idle / no show |
| Status LED fast blink | Loading / arming |
| Status LED solid | Armed / ready |
| Status LED slow blink | Playing |
| Error LED double-blink | Error |
| Panic button (GPIO in) | Hardware backup for MIDI panic/stop |

Driven via the `rppal` crate from the control thread.

---

## 9. Latency alignment

- Global audio-output latency figure + per-destination MIDI offset (ms), stored in
  `show.toml`, tunable live.
- `turtle calibrate <destination>` emits a click on audio and a test event on the chosen
  port so the offset can be measured and dialed per destination. Mean latency is
  compensable; residual jitter is not.

---

## 10. CLI and control socket

`turtled` (systemd daemon) exposes a Unix-domain control socket (JSON line protocol).
The `turtle` CLI is a thin client. Turtle-semantic commands only — raw ALSA device
enumeration is left to `aplay -l` / `amidi -l`.

- `turtle doctor` — preflight: audio device present + supports show rate; all mapped
  destinations present; RT priority available; SSD mounted.
- `turtle validate <bundle>` — schema, stem/format, DIN CC-bandwidth checks.
- `turtle load <show>` / `status` / `arm <song>` / `start` / `stop` / `panic`.
- `turtle calibrate <destination>` — latency alignment.
- `turtle test <destination>` — send a test event through Turtle routing + offset.
- `turtle monitor` — print incoming commands (map debugging).

---

## 11. Ableton → Turtle converter (Python)

Ingests a **Live Project folder** using naming conventions (no separate mapping file):

- Audio tracks named `t1`–`t4` are the four stem pairs. **Each must be a single
  consolidated, unwarped clip spanning the song** (Live *Consolidate*, warp off, no clip
  envelopes) — the only way to reference rendered audio without running Live.
- MIDI tracks named by destination (`lights`, `pedals`, `video`, `wear`) route by name.
- Tempo comes from the Live set.

Pipeline: gunzip `.als` → parse XML → extract tempo map, arrangement, MIDI
(notes/CC/PC), external CC automation → convert musical time → samples → thin/emit
external MIDI as per-destination SMF → resample referenced stems to the show rate,
write int24 WAV → emit `show.toml` / `song.toml`. Emits validation warnings (CC
bandwidth, unmapped tracks, missing/short stems, rate mismatches).

Libs: `lxml`, `mido`, `numpy`, `soundfile`.

**Rationale for a precompile step (not runtime `.als` reading):** `.als` is gzipped XML
and a moving target across Ableton versions; the RT engine must not be coupled to it.
Conversion is where validation happens, and the resulting bundle is small, stable,
versioned, and fast-loading.

### 11.1 Sample-rate / device matching

Source project rate (44.1 today) is independent of playback rate. The converter
resamples offline to the show's playback rate. With the HX Stomp (**48 kHz only**), set
`playback_rate = 48000`; 44.1 projects are resampled once at conversion. This avoids the
runtime-resample failure mode seen on older Pis. Playback rate stays configurable for
future 44.1-capable interfaces / the I2S HAT.

---

## 12. System / boot / robustness

- `systemd` unit, auto-restart, boot-to-ready; hardware watchdog.
- Overlay / read-only rootfs; stems on USB SSD — a yanked power cord cannot corrupt
  state mid-set.
- `cpu governor=performance`, `threadirqs`, RT priorities for audio + MIDI threads,
  `rlimit rtprio`; consider `isolcpus` for the audio core.
- Failure policy: xrun → log + continue (never stop audio); stem-load failure → refuse to
  arm + signal error (CLI + error LED); no network required at showtime.

---

## 13. Rust module layout

| Crate | Kind | Responsibility |
|---|---|---|
| `turtle-core` | lib | Data model, bundle load/validate, timeline compilation, sample-time math. |
| `turtle-dsp` | lib | Biquad, delay, gain, limiter — alloc-free, unit-tested. |
| `turtled` | bin | Three threads + state machine + ALSA. The lock-free RT boundary lives here in one small, heavily-commented, `unsafe`-quarantined module. |
| `turtle-cli` | bin | Control-socket client. |

Key crates: `alsa`, `rtrb`/`ringbuf`, `crossbeam`, `serde` + `toml`, `clap`,
`midly` (SMF), `hound` (WAV), `rppal` (GPIO), `tracing` (non-RT only; the RT thread uses
a preallocated log ring).

---

## 14. Open items / future

- Crossfade segues (v1 is hard auto-advance only).
- Touch-takeover blending of DSP params (v1 is live-only, no blend).
- FLAC/`symphonia` stem support for smaller bundles.
- Optional WiFi AP for config convenience.
- Full tempo-map-following for tempo-synced delay (v1 uses nominal BPM).
