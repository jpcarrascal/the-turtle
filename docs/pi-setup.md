# Raspberry Pi setup

How to bring up a Pi for The Turtle and build/run the daemon: flashing the OS,
building, the [ALSA backend](#alsa-backend), [real-time
priorities](#real-time-thread-priorities-3),
[running it as a service](#running-as-a-service-12), and
[CPU tuning](#cpu-tuning-governor-threadirqs-isolcpus-12).

**Shortcuts:** [`turtle ports`](#finding-your-device-names-turtle-ports) prints the
device strings to put in `show.toml`, and
[`turtle doctor`](#preflight-turtle-doctor-10) checks most of what the sections
below tell you to verify by hand.

Everything from "real-time priorities" onwards is **optional tuning** — the show
plays without any of it. Do the read-only rootfs step last of all, since it makes
further changes not persist.

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

[ports]
CME = "H4MIDIWC"          # your card id, from `turtle ports`

[[destinations]]
name = "lights"
port = "CME:1"

[control]
input_port = "CME:1"
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

## Finding your device names: `turtle ports`

Before editing `show.toml`, get the device strings from the box itself:

```bash
turtle ports
```

```
Paste these into show.toml. They are stable across reboots and
replugs, unlike the hw:<index> form that `amidi -l` prints.

audio  ([audio] device)
  hw:CARD=Headphones  bcm2835 Headphones
  hw:CARD=vc4hdmi0    vc4-hdmi-0
  hw:CARD=vc4hdmi1    vc4-hdmi-1
  hw:CARD=L6          ZOOM Corporation L6 at usb-0000:01:00.0-1.2, high speed

midi   ([control] input_port, [[destinations]] port)
  L6 — ZOOM Corporation L6 at usb-0000:01:00.0-1.2, high speed
    hw:CARD=L6,DEV=0,SUBDEV=0        IO  L6 MIDI I/O Port
    hw:CARD=L6,DEV=0,SUBDEV=1        IO  L6 Mixer Control Port
    hw:CARD=L6,DEV=0,SUBDEV=2        IO  L6 for L6 Editor Port
  H4MIDIWC — CME Pro H4MIDI-WC at usb-0000:01:00.0-1.3, full speed
    hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0  IO  H4MIDI-WC Port 1
    hw:CARD=H4MIDIWC,DEV=0,SUBDEV=1  IO  H4MIDI-WC Port 2
    hw:CARD=H4MIDIWC,DEV=0,SUBDEV=2  IO  H4MIDI-WC Port 3
    hw:CARD=H4MIDIWC,DEV=0,SUBDEV=3  IO  H4MIDI-WC Port 4
```

Note the audio list includes the Pi's own headphone jack and HDMI outputs. Those
are real playback devices, so they are listed rather than filtered — but the spec
(§2) rules out the 3.5 mm PWM output, so on this box the USB interface is the one
you want.

**Use these, not the `hw:1,0,0` form `amidi -l` prints.** ALSA assigns card
*indices* in enumeration order, so an index-based name silently comes to mean a
different device after a replug or a reboot in a different order — and since
`turtled` treats a missing MIDI input as fatal, a config that worked yesterday
stops the show from starting today with nothing to blame. Card *ids* never move.

Two details this saves you working out by hand:

- **The port name beside each string.** `SUBDEV=0` on its own does not tell you it
  is the socket labelled "Port 1" on the box.
- **`SUBDEV` is always included.** It defaults to `-1` ("any"), so an unqualified
  `hw:CARD=H4MIDIWC` on a four-port interface gives whichever port ALSA picks
  first — which works right up until it doesn't.

`--toml` prints a ready-to-paste starting config. It guesses that your first MIDI
port is both the control input and the first destination, and says so — check it
rather than trusting it:

```bash
turtle ports --toml
```

### Logical port labels (`[ports]`)

Writing full ALSA addresses on every destination works, but it repeats the card id
everywhere and leaks sound-system addressing into what should describe a *show*.
The `[ports]` table (§5/§7.1) gives them short names:

```toml
[ports]
CME = "H4MIDIWC"          # alias -> card id, from `turtle ports`

[[destinations]]
name = "lights"
port = "CME:1"            # -> hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0

[control]
input_port = "CME:1"      # ports are duplex; direction follows how it is used
```

`CME:1` is the port the hardware calls "Port 1" and `turtle ports` lists first —
**1-based, matching the label on the box** rather than the `SUBDEV` number beneath
it. Swap to a different interface and only the `[ports]` line changes.

Three things worth knowing:

- **Raw addresses still work.** Anything ALSA already understands passes through
  untouched, so existing show files need no migration. You can mix both.
- **Typos fail at `turtle validate`**, not at showtime:
  ```
  destination lights: port "CEM:1": no [ports] entry named "CEM" (known aliases: CME)
  ```
- **`turtle doctor` shows the resolution**, so a label is never opaque:
  ```
  ok   destination "lights": CME:1 -> hw:CARD=H4MIDIWC,DEV=0,SUBDEV=0
  ```

The mapping is arithmetic — `:n` becomes `DEV=0,SUBDEV=n-1` — rather than a lookup
against the live hardware. That keeps it pure, so `turtle validate` can check a
show file on a laptop with no soundcard, and the rules are the same everywhere. It
assumes one subdevice per port on device 0, which is how both interfaces on this
rig enumerate. If a device does not fit that shape, write its full address in
`port` — no new syntax needed.

## Looping a song (§14)

A song repeats seamlessly until stopped when its `song.toml` says so:

```toml
[song]
name = "Opener"
bpm  = 122.0
length_samples = 14112000
loop = true
```

What changes while a song loops:

- **It never ends**, so the gapless auto-advance to the next setlist entry never
  fires. **Stop is the only way out** — that is the point, not an oversight.
- **`turtle status` says so**, which matters because a looping song never reaches
  `ended`, so "the position stopped rising" and "the song is over" would otherwise
  be indistinguishable:
  ```
  song:   opener (looping)
  ```
- **MIDI keeps firing.** Each destination's scheduler rewinds at the seam, so
  lights and pedal cues repeat with the audio rather than playing once.

To try it without editing anything:

```bash
turtle gen-tone /media/shows/Loop.turtle 4 440 --loop
```

then point its `show.toml` at your devices. **Re-running `gen-tone` over an
existing bundle is refused**, because it rewrites `show.toml` and `song.toml` from
templates and would silently discard those device settings — the failure would
show up much later as a device that will not open. Pass `--force` if you really do
want to start over.

The seam is sample-accurate: the wrap happens **inside** the audio buffer, not at
its boundary. Wrapping only at buffer boundaries would quantise the loop point to
the period size — about 21 ms at 1024 frames — which you would hear as a gap every
time round. Seamlessness beyond that is a property of your stems: they must be an
exact whole number of bars, which Ableton's bounce already gives you.

`turtled play` is a fixed-duration tool and stops after one pass even for a
looping song; use `turtled control` (or the service) to hear it repeat.

## Delay starting values (§6)

The shared delay comes up ready to use rather than blank, so raising a send is
enough to hear it — no dialling in feedback, return, cutoff and Q first. Override in
`show.toml` if you want different ones:

```toml
[delay]
time      = "1/4"   # note division: "1/16" ... "1/1", dots as "1/4."
feedback  = 64      # CC value, ~half
return    = 100     # CC value; the gain taper puts unity at 100
cutoff    = 89      # CC value, ~2.5 kHz (the sweep is exponential)
resonance = 0       # CC value; 0 is the floor, no resonant peak
```

Two things worth knowing:

- **The continuous controls are CC values, not percentages or Hz.** That is
  deliberate: a default and a live pedal move then go through the *identical*
  mapping, so they cannot drift apart. `time` is the exception, written as a note
  division because it is a discrete musical choice rather than a swept control.
- **Sends still default to zero, so the delay is silent** until you send something
  to it. Its settings are live; the delay itself is opt-in. A show that never
  touches a send never hears it.

## The delay tail after Stop (§6)

Stopping the transport no longer cuts the delay dead. The stems stop and the
position freezes, but the delay keeps recirculating, so whatever was ringing decays
away naturally.

**How long it lasts is the feedback knob.** There is deliberately no automatic
cut-off: at high feedback the tail sustains indefinitely, and bringing feedback
down is how you end it. That was a deliberate choice — a delay that silences itself
after some threshold takes away a performance gesture.

To kill it instantly, **press Stop a second time**. That is already the panic
gesture (`transport.rs`: first Stop = clean release + rewind, second = panic), and
the explicit panic binding does the same thing — they route to the same action, so
there is one rule rather than two. A third Stop is a no-op.

Two asymmetries worth knowing:

- **MIDI stops immediately, audio rings.** Stop emits note-offs, so lights and
  pedals do not echo. Only audio has a tail. That is intended.
- **`turtle status` will say `stopped` while sound is still coming out.** The
  transport really has stopped; the tail is audio, not playback, and the position
  does not advance while it rings.

One limitation: **a song switch clears the tail.** The delay belongs to the song —
it is tempo-synced to that song's BPM — so switching songs (including a gapless
auto-advance) starts with an empty delay. Keeping a tail across a song change would
mean separating the delay's buffer from its timing, which is a real cost for a case
that may never come up in practice.

## Preflight: `turtle doctor` (§10)

Everything below this point adds a "run this and check the output" step. `doctor`
is all of those checks in one command, so you can confirm a box is ready without
working through the sections by hand:

```bash
# Point it at the bundle the *service* loads (see `systemctl show -P ExecStart
# turtled`), not a copy in your home directory. A stale copy next to the live one
# is the easiest way to spend an hour debugging a file nobody is playing.
turtle doctor /media/shows/Tone.turtle
```

```
The Turtle — preflight

show
  ok   loaded "Tone Test": 1 song(s), 1 destination(s), 48000 Hz
  ok   bundle validates

stems
  ok   all stems present and readable (45.2 MB across 1 song(s))

audio
  ok   device "hw:CARD=HXStomp" opens
  ok   supports 48000 Hz

midi
  FAIL destination "lights" -> "CME:1" not found
       -> available: hw:1,0,0, hw:1,0,1
  warn some ports look like spec-style logical labels (e.g. "CME:1")
       -> logical-label resolution is not implemented yet — use the real ALSA name from `amidi -l`

realtime
  ok   RT priority available (rtprio limit 95)
  ok   memory locking available (memlock unlimited)

system
  ok   CPU governor: performance
  warn no isolated CPUs (isolcpus not set)
       -> optional, and usually unnecessary — see docs/pi-setup.md before enabling

daemon
  ok   turtled responding on /run/turtle/control.sock: Idle, song tone

1 FAILURE(S), 2 warning(s)
```

Three things to know about reading it:

- **`FAIL` and `warn` mean different things.** `FAIL` = the show cannot play.
  `warn` = it will play, but something is untuned or unusual. Only failures set a
  non-zero exit code, so `turtle doctor` is safe to use in a script without
  tripping over the optional CPU tuning.
- **It reports everything**, rather than stopping at the first problem — you want
  the whole list before you start fixing things.
- **`?` means "could not check here"**, not "fine". You will see it for the ALSA
  and RT checks when running on a Mac, where those cannot be probed.

The show argument is optional. Without it, the device and MIDI checks are skipped
(they come from `show.toml`) but the box-level ones still run, which answers "is
this Pi set up?" when you have no bundle to hand:

```bash
turtle doctor
```

Running it **while the daemon is up** is fine and expected: `turtled` holds the
audio device exclusively, so doctor reports it as *present but busy* rather than
broken.

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
# Before the service is installed these live wherever you built and unpacked
# them; afterwards, use the installed binary and the bundle under /media/shows,
# and `systemctl stop turtled` first so the two do not fight over the device.
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

### Upgrading once it runs as a service

Pulling a new version is no longer just `cargo build` — the binary in
`/usr/local/bin` is a *copy*, and the running service is holding it:

```bash
cd ~/the-turtle

# Merged already? Pull main. Testing an unmerged PR? Check its branch out
# instead -- `git pull` on main will NOT have it, and the failure is confusing:
# a missing deploy/ file, or a binary silently lacking the new behaviour.
git pull                          # ...or: git fetch && git checkout <branch>

cargo build --release -p turtled -p turtle-cli

# Stop first. Replacing an executable that is currently running fails
# ("Text file busy"), and stopping is instant here anyway.
sudo systemctl stop turtled
sudo install -m755 target/release/turtled target/release/turtle /usr/local/bin/
sudo systemctl start turtled

systemctl status turtled          # armed, and on the new build
```

Two things that will bite otherwise:

- **If a unit file changed**, re-copy it and `sudo systemctl daemon-reload` before
  starting — systemd caches unit files, so an edited `turtled.service` is ignored
  until you do. Note that `systemctl edit --full` copies the unit into
  `/etc/systemd/system/`, so a later `git pull` does **not** update it; re-apply
  your `ExecStart` path after copying a new version in.
- **If you enabled the read-only overlay**, none of the above persists across a
  reboot. Disable the overlay (`raspi-config`), upgrade, re-enable.

To go back to a hand-started daemon for debugging, stop the service first — two
`turtled` processes would fight over the audio device, and the second would fail to
bind the control socket:

```bash
sudo systemctl stop turtled
./target/release/turtled control /media/shows/Tone.turtle -v
```

### Why `Type=notify` and not `simple`

With `Type=simple`, systemd calls the unit started the moment `execve` returns —
before the stems are loaded or the transport is armed. `turtled` instead sends
`READY=1` only once it can actually serve a request, so `systemctl start` blocks
until the show is genuinely ready, and a start-up failure reads as a failed start
rather than "started, then crashed".

### Waiting for devices at startup

`turtled` waits up to **15 seconds** for its audio and MIDI devices before giving
up. This is not politeness — it fixes a real failure:

```
13:09:47  Cannot get card index for L6 ... No such device (19)
13:09:47  Main process exited, code=exited, status=1/FAILURE
13:09:48  Scheduled restart job, restart counter is at 1697.
13:09:48  armed "Tone Test" on hw:L6            <- succeeded one second later
```

A device open used to be fatal, and `Restart=always` has no ceiling, so a device
that was merely *late* became an unbounded crash-loop at one attempt per second.
Two ways that happens: at boot, USB enumeration races the service (`After=sound.target`
means *some* sound device exists, not that your interface has appeared); and on a
restart, the outgoing process may not have released the device yet.

It recovers either way, which is why it went unnoticed for so long — but it floods
the journal, and each loop is a failed `READY=1`, so `systemctl start` reports a
failure for a daemon that is about to be fine.

In the journal a late device now costs one line:

```
waiting up to 15s for audio device 'hw:L6': ... 'No such device (19)'
```

`systemctl status` shows the same text while it waits, so a slow start explains
itself. To change or disable the window:

```bash
turtled control <bundle> --wait-devices 30   # be more patient
turtled control <bundle> --wait-devices 0    # fail immediately (the old behaviour)
```

`turtle doctor` reports the restart count, so you never have to find a number like
1697 by reading the journal:

```
service
  ok   turtled.service active (result: success)
  warn 12 automatic restart(s) — the daemon has exited unexpectedly at least once
       -> journalctl -u turtled -b | grep -i 'exited\|failed' to see why
```

The count is per-run: a clean `systemctl stop`/`start` resets it, so a non-zero
value describes the *current* run rather than all history.

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

**You must be in the `audio` group to use the CLI against the service.** The socket
belongs to the service user and is mode `0660`, so access is granted by group —
the same group that already gates opening the audio device. Without it, *every*
`turtle` verb fails with `Permission denied`, not just `doctor`:

```bash
sudo usermod -aG audio "$USER"
# then log out and back in — group membership is applied at login
id -nG | tr ' ' '\n' | grep -x audio    # confirm before reconnecting
turtle status
```

This is easy to miss if you skipped the `limits.conf` step earlier on the grounds
that it does not apply to services: that step also added you to `audio`, and this
is the other reason to be in it.

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

Do this **genuinely last** — after the show runs *and* after the
[CPU tuning](#cpu-tuning-governor-threadirqs-isolcpus-12) below, which edits
`cmdline.txt` and installs another unit. With the overlay on, changes do not
persist, so develop first and seal afterwards. To make a change later, disable the
overlay, edit, re-enable.

Note the stems live on the USB SSD, not the overlay, so bundle size is unaffected.

## CPU tuning: governor, threadirqs, isolcpus (§12)

The last of §12's tuning, and the part you are most likely to skip.

**Read this before doing any of it.** The main defence against xruns in this design
is not tuning at all — it is the **large audio buffers** (§3.1). Latency is
irrelevant here, so we run 1024-frame buffers, which already absorb the delays this
section guards against. What follows is insurance on top, and it is not all worth
the same:

| | Cost | Recommendation |
|---|---|---|
| **Governor** → `performance` | One command, reversible | Worth it. Removes clock-ramp jitter for free. |
| **`threadirqs`** | Kernel cmdline + reboot | Optional. Modest benefit. |
| **`isolcpus`** | Kernel cmdline + reboot; surrenders a core | **Skip unless you observe xruns.** |

`isolcpus` is deliberately last and deliberately discouraged: it is the most
invasive change here (a malformed `cmdline.txt` is a Pi that will not boot) and it
permanently gives a quarter of the CPU to one thread that, on an otherwise-idle Pi
4 with large buffers, very likely does not need it. Reach for it when you are
*diagnosing* xruns, not in advance.

`turtled` needs no configuration either way — it adapts to whatever you have done
(or not done) and reports it.

`turtled` prints what it observes at startup, so this is also how you check the
tuning took:

```
[sched] cpu: governor performance, isolated CPUs [3]
```

versus an untuned box:

```
[sched] cpu: governor ondemand, no isolated CPUs (isolcpus not set)
```

### 1. CPU governor → `performance`

The default `ondemand` governor raises the clock *in response to* load, which is
the wrong shape for audio: the ramp latency lands on the first periods after a
quiet passage, and the frequency transitions add jitter of their own. Install the
oneshot unit that pins the cores at full clock:

```bash
sudo cp deploy/turtle-tuning.service /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now turtle-tuning
journalctl -u turtle-tuning     # "governor now: performance"
```

It is a **separate unit from `turtled` on purpose**: writing the governor needs
root, and `turtled` deliberately runs unprivileged with `NoNewPrivileges=true`.
Hoisting the one privileged action into a unit that runs once and exits is better
than granting the daemon — the process exposed to a control socket and to stem
files — the privilege to keep. `turtled.service` only `Wants=` it, so a failure
here never stops a show.

On a Pi 4 this means four cores at full clock: a heat and power question, not a
stability one, and this is a mains-powered appliance.

### 2. `threadirqs` — make IRQ handling preemptible

By default, hard IRQ handlers run in a context your `SCHED_FIFO` thread cannot
preempt. `threadirqs` moves them into kernel threads (priority 50, below our 80),
so a burst of USB or network interrupt work can no longer delay an audio period.

### 3. `isolcpus` — give the audio thread a core of its own

**`isolcpus` alone does nothing for us.** It removes a core from the general
scheduler, but no thread lands there unless explicitly placed — so without the
matching affinity call it reserves a core that simply goes unused. `turtled` does
the pinning: at startup it reads `/sys/devices/system/cpu/isolated` and pins the
audio thread to the highest isolated CPU, so **the kernel parameter is all you
have to configure.** Nothing to pass, and nothing happens if you skip it.

Both settings are kernel command line. Edit `/boot/firmware/cmdline.txt` — it is a
**single line**; append to it, do not add new lines:

```bash
sudo cp /boot/firmware/cmdline.txt /boot/firmware/cmdline.txt.bak
# Append to the existing line. isolcpus=3 reserves the last of the Pi 4's 4 cores.
sudo sed -i '1 s/$/ threadirqs isolcpus=3/' /boot/firmware/cmdline.txt
cat /boot/firmware/cmdline.txt      # eyeball it before rebooting
sudo reboot
```

> A malformed `cmdline.txt` can leave the Pi unbootable, which is why the backup
> above is worth the two seconds. Recovery is to mount the SD card's boot
> partition on another machine and restore `cmdline.txt.bak`.

After the reboot:

```bash
cat /sys/devices/system/cpu/isolated        # 3
grep -o 'threadirqs' /proc/cmdline          # threadirqs

# The audio thread should now be alone on CPU 3. PSR is the CPU it last ran on.
ps -Lo pid,tid,cls,rtprio,psr,comm -p "$(pgrep turtled)"
```

`turtled` logs `[sched] audio thread: pinned to CPU 3` when it takes effect.

### Overriding the pinning

Auto-detection is the default; both overrides exist for diagnosis:

```bash
turtled control <bundle> --audio-cpu none    # never pin, even with isolcpus set
turtled control <bundle> --audio-cpu 2       # pin to a specific core
```

`--audio-cpu none` is the useful one: it isolates whether a glitch is
pinning-related, on a box where `isolcpus` is configured.

### Is any of this worth it? Measure.

The honest answer is that these help under *load*, and the whole point of the
large buffers (§3.1) is to make the unloaded case fine already. To find out on
your hardware, load the Pi while a song plays and compare:

```bash
# In one shell: four spinners plus some I/O, roughly a worst-case stage moment.
for i in 1 2 3 4; do (while :; do :; done) & done; sudo apt-get -qq update

# In another, A/B the tuning:
# `systemctl stop turtled` first: two daemons cannot share the audio device.
turtled control /media/shows/Tone.turtle                        # fully tuned
turtled control /media/shows/Tone.turtle --rt-prio 0 --audio-cpu none  # untuned
kill %1 %2 %3 %4
```

If both are clean, you have headroom — good, and worth knowing before you need it.

## Faster iteration (later)

Native builds on the Pi are the simplest starting point. If they become a
bottleneck, cross-compile from a faster machine with
[`cross`](https://github.com/cross-rs/cross) (Docker-based) targeting
`aarch64-unknown-linux-gnu`.
