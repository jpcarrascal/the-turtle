# Raspberry Pi setup

How to bring up a Pi for The Turtle and build/run the daemon. This covers the
host-independent core that compiles today; the Linux-only ALSA backend (§2/§3
of `turtle-spec.md`) is not written yet — see [When we add ALSA](#when-we-add-alsa).

## 1. Flash the OS

Use **Raspberry Pi OS Lite, 64-bit** (current Debian release, e.g. Trixie) —
headless, no desktop.

- Debian + `systemd` + `apt` match the appliance model (spec §12), and the
  `alsa` crate's `libasound` dependency is a one-line `apt install`.
- Lite (no GUI) leaves CPU cores free for the audio/MIDI RT threads and makes
  `isolcpus` / overlay-root straightforward.
- 64-bit gives the `aarch64` Rust target and better throughput on the Pi 4.

Flash with **Raspberry Pi Imager**; in its advanced options set the hostname,
enable SSH, and configure your user + Wi-Fi so the Pi is headless from first
boot.

### Kernel: stock is fine for v1

Do **not** reach for a `PREEMPT_RT` kernel yet. The design uses large,
xrun-proof audio buffers because latency is irrelevant (§3.1), so stock kernel
+ `SCHED_FIFO` + `threadirqs` + `isolcpus` should be plenty. Only pursue an
RT-patched kernel if you actually observe xruns.

## 2. Install Rust and build

SSH into the Pi, then:

```bash
# Build tooling + git (libasound2-dev comes later, with the ALSA backend)
sudo apt update && sudo apt install -y build-essential pkg-config git

# Rust — rustup picks aarch64 stable; rust-toolchain.toml pins the rest
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

# Clone + verify: the whole host-independent core compiles and its tests pass
git clone https://github.com/jpcarrascal/the-turtle.git
cd the-turtle
cargo test

# Build and run the daemon skeleton (loads + validates a show bundle)
cargo build --release -p turtled
./target/release/turtled path/to/MyShow.turtle/show.toml
```

A clean native build on a Pi 4 (4 GB) takes a few minutes. `cargo test` being
green on `aarch64` revalidates the entire host-independent core (`turtle-core`,
`turtle-dsp`, and the `turtled` RT logic) on real hardware — this is the first
time any of it runs on the real target arch/OS rather than a dev Mac.

## 3. Smoke test with a minimal bundle

No bundle is checked into the repo yet, so create a throwaway one directly on
the Pi to exercise `turtle-cli` and `turtled`'s load/validate path:

```bash
mkdir -p ~/smoke && cat > ~/smoke/show.toml <<'EOF'
[show]
name = "Pi Smoke Test"
playback_rate = 48000

[audio]
device = "hw:CARD=HXStomp"

[[destinations]]
name = "lights"
port = "CME:1"

[control]
input_port = "CME:in"
select_channel = 1
start = { type = "note", note = 60 }
stop  = { type = "note", note = 61 }
next  = { type = "note", note = 62 }
prev  = { type = "note", note = 63 }
panic = { type = "note", note = 65 }
mute  = { type = "note", notes = [72, 73, 74, 75] }
EOF

./target/release/turtle-cli validate ~/smoke/show.toml
./target/release/turtled ~/smoke/show.toml
```

Expect:

```
~/smoke/show.toml: ok
loaded "Pi Smoke Test": 1 destination(s), 0 song(s); audio 48000 Hz / 1024 frames; state Idle
RT runtime not started (requires Linux/ALSA). Engine wiring OK.
```

This proves the model, validation, timeline compilation, transport state
machine, and the engine's lock-free wiring all work on real hardware. It does
**not** touch audio or MIDI I/O — `turtled` runs against `NullAudio`/`NullMidi`
stubs until the ALSA backend lands (below), so no sound or lights yet.

## 4. What runs where

- **The Pi** runs `turtled` and consumes finished `.turtle` bundles. No network
  is required at showtime (§12).
- **Your laptop** runs the Python converter (`tools/converter`) to turn Ableton
  projects into bundles. You do **not** need Python on the Pi.

## When we add ALSA

The audio PCM loop and MIDI rawmidi I/O (spec §2/§3) are Linux-only and sit
behind the `backend` traits in `turtled`. When that lands, the only extra setup
step is the ALSA development headers:

```bash
sudo apt install -y libasound2-dev
```

## Faster iteration (later)

Native builds on the Pi are the simplest starting point. If they become a
bottleneck, cross-compile from a faster machine with
[`cross`](https://github.com/cross-rs/cross) (Docker-based) targeting
`aarch64-unknown-linux-gnu`.
