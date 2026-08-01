# The Turtle

A headless backing-track and MIDI-automation player for live solo performance,
built as a Raspberry Pi appliance. It replaces a laptop running Ableton on stage:
it plays multichannel backing stems and emits sample-locked MIDI to lighting,
wearables, guitar pedals and video, all driven from a foot controller.

Boots to ready, needs no network at showtime, and is controlled entirely over MIDI
while you play.

- [`docs/turtle-spec.md`](docs/turtle-spec.md) — the system specification.
- [`docs/pi-setup.md`](docs/pi-setup.md) — flashing a Pi through to running it as a
  service.

## Status

Running on hardware and used for rehearsal. Everything in the spec is implemented
except GPIO (§8.1), which is waiting on parts.

| Area | State |
|---|---|
| Audio engine (§4) — preload to RAM, 4 stereo pairs, per-pair gain/mute | ✅ |
| MIDI output (§5) — per-destination SMF, sample-locked, per-port latency offsets | ✅ |
| MIDI clock master (§5.1) — 24 ppqn, opt-in per destination, ~1 ms measured jitter | ✅ |
| Live DSP (§6) — per-pair filter, shared tempo-synced delay, linked master limiter | ✅ |
| Transport + setlist (§8) — MIDI control, gapless auto-advance, background preload | ✅ |
| Per-song looping (§14) — seamless, `loop = true` | ✅ |
| CLI + control socket (§10) — `status`/`arm`/transport/`monitor`/`doctor`/`ports` | ✅ |
| System (§12) — systemd, watchdog, RT priorities, device wait, read-only rootfs | ✅ |
| GPIO (§8.1) — status/error LEDs, panic button | ⬜ no hardware yet |
| `turtle calibrate` / `turtle test` (§10) | ⬜ needs a bench with lights |
| Ableton → bundle converter (§11, Python) | 🚧 `tools/converter` |

## How it fits together

```
Ableton project ──(converter, on your laptop)──▶  MyShow.turtle/  ──▶  the Pi
                                                  show.toml
                                                  songs/*/song.toml
                                                          stems/*.wav
                                                          midi/*.mid
```

A **show bundle** is a plain directory of TOML, WAV and standard MIDI files —
human-inspectable and DAW-portable. The Pi runs `turtled`, which loads a bundle,
preloads a song's stems into RAM, and plays it while dispatching each destination's
MIDI against the audio timeline.

## Crates

| Crate | Kind | Responsibility |
|---|---|---|
| `turtle-core` | lib | Data model, bundle load/validate, timeline compilation, sample-time maths, control-socket wire protocol |
| `turtle-dsp` | lib | Biquad, delay, gain, limiter — alloc-free and unit-tested |
| `turtled` | bin | The daemon: audio RT loop, MIDI scheduling, transport state machine, ALSA |
| `turtle-cli` | bin (`turtle`) | Control-socket client plus offline tools |

Everything that can be portable is: the DSP, the data model, the transport state
machine and the socket protocol all compile and test on macOS. Only the ALSA layer
is Linux-gated — so `cargo test` on a laptop covers most of the system, and the Pi
covers the rest.

## The `turtle` CLI

```
turtle ports [--toml]              list devices with their stable ALSA names
turtle doctor [<bundle>]           preflight: devices, RT limits, tuning, daemon
turtle validate <show.toml>        schema and bundle checks
turtle gen-tone <dir> [s] [hz]     write a playable test bundle
turtle status | monitor            transport state; stream incoming commands
turtle arm <song> | start | stop | next | prev | panic
```

`doctor` is the one to reach for first: it checks the audio device and rate, every
MIDI port, RT priority and memlock limits, CPU tuning, and whether the daemon is
running — and says what to do about anything it finds.

## Building

```bash
cargo test              # the whole portable core, on any host
cargo build --release -p turtled -p turtle-cli
```

On a Pi you also need `libasound2-dev`; see [`docs/pi-setup.md`](docs/pi-setup.md).

## Design notes worth knowing

**The transport clock is decoupled from the audio buffer** (§3.1). The RT thread
publishes `(sample_pos, monotonic_ns)` under a seqlock each period; the MIDI
scheduler interpolates between periods. So the buffers can be large and xrun-proof
without coarsening MIDI timing — latency is irrelevant here, because nothing is
monitored through the Pi.

**The RT thread never allocates, locks or blocks.** Commands cross to it over
lock-free SPSC queues; song loading happens on a background thread and the finished
mixer is handed over the same way.

**Failure policy is that the show keeps playing** (§12). RT priority, memory locking
and CPU tuning are all best-effort and warn rather than refuse. Devices that are late
at boot are waited for. An audio device that dies is reported and the daemon exits so
systemd can restart it — because silently continuing without audio is worse than
restarting.
