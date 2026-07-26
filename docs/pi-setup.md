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
# Build tooling + git + ALSA headers. libasound2-dev is now required: the
# Linux-only ALSA backend (alsa_backend.rs) compiles as part of turtled.
sudo apt update && sudo apt install -y build-essential pkg-config git libasound2-dev

# Rust — rustup picks aarch64 stable; rust-toolchain.toml pins the rest
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh -s -- -y
. "$HOME/.cargo/env"

# Clone + verify: the whole host-independent core compiles and its tests pass
git clone https://github.com/jpcarrascal/the-turtle.git
cd the-turtle
cargo test

# Build the daemon + the CLI. Build both: the §3 smoke test uses the CLI too.
# NB: the `turtle-cli` *package* builds a binary named `turtle` (see its
# [[bin]] name) — so it is `-p turtle-cli` to cargo but `./target/release/turtle`
# to run.
cargo build --release -p turtled -p turtle-cli
./target/release/turtled path/to/MyShow.turtle/show.toml
```

A clean native build on a Pi 4 (4 GB) takes a few minutes. `cargo test` being
green on `aarch64` revalidates the entire host-independent core (`turtle-core`,
`turtle-dsp`, and the `turtled` RT logic) on real hardware — this is the first
time any of it runs on the real target arch/OS rather than a dev Mac.

**The `cargo build -p turtled` step is itself a new check.** The ALSA backend
(`crates/turtled/src/alsa_backend.rs`) is gated behind `#[cfg(target_os =
"linux")]` and the `alsa` crate is a Linux-only dependency, so it is *never*
compiled on the dev Mac — `cargo build`/`cargo test` there validate only the
portable core. This build is the first time that code is compiled at all, so a
clean build (no ALSA errors) is the smoke test for the hardware layer until it
is wired into a runnable path.

## 3. Smoke test with a minimal bundle

No bundle is checked into the repo yet, so create a throwaway one directly on
the Pi to exercise the `turtle` CLI and `turtled`'s load/validate path:

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

./target/release/turtle validate ~/smoke/show.toml
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
**not** touch audio or MIDI I/O — `turtled`'s `main` still runs against
`NullAudio`/`NullMidi` stubs until the ALSA backend is wired into a runnable
path (below), so this output is unchanged even though the ALSA code now
compiles, and there's no sound or lights yet.

## 4. What runs where

- **The Pi** runs `turtled` and consumes finished `.turtle` bundles. No network
  is required at showtime (§12).
- **Your laptop** runs the Python converter (`tools/converter`) to turn Ableton
  projects into bundles. You do **not** need Python on the Pi.

## ALSA backend

The audio PCM loop and MIDI rawmidi I/O (spec §2/§3) are Linux-only and sit
behind the `backend` traits in `turtled`. The **first slice has landed**:
`alsa_backend.rs` opens/configures the PCM device (`AlsaAudio`) and fans MIDI
out over rawmidi (`AlsaMidi`). It builds only on the Pi (see §2) — its extra
requirement is the ALSA development headers, now folded into the §2 apt install:

```bash
sudo apt install -y libasound2-dev
```

Still to come before it is show-ready: resolving logical port labels
(`"CME:1"`) to ALSA `hw:` device names, GPIO (§8.1), and the systemd unit (§12).

## Real-time thread priorities (§3)

`turtled` asks the kernel to put its audio thread on `SCHED_FIFO` priority 80,
and the fused control/MIDI thread on 75, so neither can be preempted by ordinary
work. Without this, a background `apt upgrade` or a busy shell can delay an audio
period by tens of milliseconds and you get an xrun.

**This needs permission, and by default a normal user does not have it.** When
the request is refused, `turtled` prints a warning and keeps playing at normal
priority — the show never refuses to start over a tuning issue — so watch the
startup lines:

```
[sched] audio thread: SCHED_FIFO priority 80          <- got it
[sched] control+midi thread: SCHED_FIFO priority 75
```

versus:

```
warning: audio thread stays at normal priority: pthread_setschedparam(...)
         failed: Operation not permitted (need CAP_SYS_NICE, or an rtprio
         limit in /etc/security/limits.conf)
```

To grant it, give your user an `rtprio` limit:

```bash
# Allow RT priorities up to 95 for your login user.
sudo tee /etc/security/limits.d/99-turtle-realtime.conf >/dev/null <<'EOF'
@audio   -  rtprio  95
@audio   -  memlock unlimited
EOF

sudo usermod -aG audio "$USER"
```

Then **log out and back in** (limits are applied at login, so `su`-ing or
re-running in the same shell will not pick them up) and confirm:

```bash
ulimit -r        # should print 95, not 0
```

Verify the daemon actually got it while it runs:

```bash
# RTPRIO column shows the priority; CLS shows FF for SCHED_FIFO.
ps -Lo pid,tid,cls,rtprio,comm -p "$(pgrep turtled)"
```

To A/B it against the old behaviour — useful when diagnosing whether a glitch is
scheduling-related — start with RT off:

```bash
./target/release/turtled control ~/Tone.turtle --rt-prio 0     # normal priority
./target/release/turtled control ~/Tone.turtle --rt-prio 60    # custom priority
```

The remaining §12 system tuning (`cpu governor=performance`, `threadirqs`,
`isolcpus`) is not wired up yet and is a later pass.

## Faster iteration (later)

Native builds on the Pi are the simplest starting point. If they become a
bottleneck, cross-compile from a faster machine with
[`cross`](https://github.com/cross-rs/cross) (Docker-based) targeting
`aarch64-unknown-linux-gnu`.
