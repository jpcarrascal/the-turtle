# Raspberry Pi setup

How to bring up a Pi for The Turtle and build/run the daemon: flashing the OS,
building, the [ALSA backend](#alsa-backend), [real-time
priorities](#real-time-thread-priorities-3), and
[running it as a service](#running-as-a-service-12).

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
(`"CME:1"`) to ALSA `hw:` device names, and GPIO (§8.1).

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

`turtled` also locks its memory (`mlockall`) at startup, for a related but
distinct reason: priority decides who gets the CPU, but a **page fault** stalls
you regardless of priority, because you are waiting on the SD card rather than on
the scheduler. Expect a third line, `[sched] memory locked (mlockall)`.
`--rt-prio 0` turns off both, so it is a genuinely untuned baseline to A/B
against.

> **Running as a service? Skip this next part.** `limits.conf` is applied by PAM
> at *login*, and a systemd service has no login session — so the file below does
> nothing for `turtled.service`. The unit grants the same two privileges with
> `LimitRTPRIO=` and `LimitMEMLOCK=` instead. You only need the limits file for
> running `turtled` by hand from a shell.

To grant it for hand-started runs, give your user an `rtprio` limit:

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

## Running as a service (§12)

Up to here `turtled` has been started by hand. The appliance model wants it to
come up on boot, restart itself, and be recoverable when it wedges — that is
`deploy/turtled.service`.

### Install

```bash
# A dedicated unprivileged user. `audio` group for ALSA device access; no shell
# and no home, because it never logs in.
sudo useradd -r -g audio -s /usr/sbin/nologin turtle

# The binaries somewhere outside your home directory (the unit sets
# ProtectHome=true, so /home is invisible to the service).
sudo install -m755 target/release/turtled target/release/turtle /usr/local/bin/

# Bundles where the unit expects them, readable by the service user. Note this
# is a *copy*, not a path change: a bundle left in your home directory will not
# work, because ProtectHome=true makes /home invisible to the service (the
# failure looks like "bundle not found" for a bundle you can plainly see).
sudo mkdir -p /media/shows
sudo cp -r ~/Tone.turtle /media/shows/
sudo chown -R turtle:audio /media/shows

sudo cp deploy/turtled.service /etc/systemd/system/
# Edit ExecStart's bundle path to match what you just copied.
sudo systemctl edit --full turtled

sudo systemctl daemon-reload
sudo systemctl enable --now turtled
```

`/media/shows` is where the USB SSD is expected to be mounted (§12), but nothing
here requires that yet — a plain directory on the SD card works for testing. The
unit's `RequiresMountsFor=` adds a dependency on whatever mount actually covers
the path, so it is satisfied either way.

Check it came up, and that the two privileges landed:

```bash
systemctl status turtled
systemctl show turtled -p LimitRTPRIO -p LimitMEMLOCK -p WatchdogUSec
# Expect: LimitRTPRIO=95  LimitMEMLOCK=infinity  WatchdogUSec=15s
journalctl -u turtled -b | grep sched
```

`systemctl status` shows the daemon's own status line (`armed "MyShow"`), because
the unit is `Type=notify` and `turtled` reports it.

### Why `Type=notify` and not `simple`

With `Type=simple`, systemd calls the unit started the moment `execve` returns —
before the stems are loaded or the transport is armed. `turtled` instead sends
`READY=1` only once it can actually serve a request, so `systemctl start` blocks
until the show is genuinely ready, and a start-up failure reads as a failed start
rather than "started, then crashed".

### The watchdog is the part worth understanding

`Restart=always` can only notice a process that *exited*. A deadlocked audio
thread or a wedged control loop leaves the process very much alive, and on stage
that is indistinguishable from a dead one. So `turtled` pings systemd from **the
control loop itself** — the same loop that dispatches MIDI — which is what makes
a successful ping mean "the show is still running" rather than merely "a thread is
still alive". Miss the 15 s deadline and systemd aborts and restarts it.

To see it work, pause the process so the loop stops pinging:

```bash
sudo kill -STOP "$(systemctl show -P MainPID turtled)"
journalctl -u turtled -f      # within ~15 s: "Watchdog timeout (limit 15s)!"
                              # then: "Scheduled restart job"
systemctl show -P NRestarts turtled    # should have incremented
```

### The control socket moves

The unit sets `TURTLE_SOCKET=/run/turtle/control.sock` and creates that directory
via `RuntimeDirectory=`, so a crash cannot leave a stale socket behind and it
works on a read-only rootfs. Both `turtled` and the `turtle` CLI read
`TURTLE_SOCKET`, and the CLI additionally *searches* — the system path first, then
`/tmp/turtle.sock` — so plain `turtle status` over SSH finds whichever daemon is
running with no flag either way.

### Hardware watchdog (survives a kernel hang)

The service watchdog above needs systemd alive to act. For the case where the
whole kernel wedges, enable the Pi's hardware watchdog so the board resets itself:

```bash
# Broadcom watchdog device
echo 'dtparam=watchdog=on' | sudo tee -a /boot/firmware/config.txt

# Hand it to systemd: it pets the hardware watchdog while the system is healthy.
# The drop-in directory does not exist on a fresh install, hence the mkdir.
sudo mkdir -p /etc/systemd/system.conf.d
sudo tee /etc/systemd/system.conf.d/10-watchdog.conf >/dev/null <<'EOF'
[Manager]
RuntimeWatchdogSec=10
RebootWatchdogSec=2min
EOF
```

Both changes need a **reboot**: `dtparam` is read by the firmware at boot, and
`RuntimeWatchdogSec` is picked up by PID 1 at startup (`daemon-reload` will not
do it — that only rereads unit files, not systemd's own config). Afterwards:

```bash
# The device, its timeout, and who is petting it.
wdctl

# Confirm PID 1 actually took the setting (10s = 10000000us).
systemctl show -p RuntimeWatchdogUSec
```

If `wdctl` reports no device, `dtparam=watchdog=on` did not take — check it
landed in `/boot/firmware/config.txt` and not an older `/boot/config.txt`.

> **This is a different watchdog from the service one.** `WatchdogSec=` in the
> unit catches a wedged `turtled` and restarts *the daemon*; this one catches a
> wedged **kernel** and resets *the board*. The service watchdog needs systemd
> alive to act, which is exactly what this covers for.

### Read-only rootfs (§12)

The goal is that a yanked power cord mid-set cannot corrupt anything. Two layers:

- **The service** already writes nowhere: the unit sets `ProtectSystem=strict`,
  so even `/media/shows` is read-only to it, and the only writable path is the
  socket directory. That holds whether or not you do the next part.
- **The SD card** via an overlay: `sudo raspi-config` →
  *Performance Options* → *Overlay File System* → enable, and also set the boot
  partition read-only. Writes then land in a RAM overlay and are discarded at
  reboot.

Do this **last**, once the show runs. With the overlay on, changes do not
persist — including the unit file edits above — so develop first and seal
afterwards. To make a change later, disable the overlay, edit, re-enable.

Note the stems live on the USB SSD, not the overlay, so bundle size is unaffected.

### Remaining tuning

`cpu governor=performance`, `threadirqs`, and `isolcpus` for the audio core are
not wired up yet and are a later pass.

## Faster iteration (later)

Native builds on the Pi are the simplest starting point. If they become a
bottleneck, cross-compile from a faster machine with
[`cross`](https://github.com/cross-rs/cross) (Docker-based) targeting
`aarch64-unknown-linux-gnu`.
