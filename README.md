# bt-intercom

A local Bluetooth Classic intercom for two or more headsets on Linux, with
selectable half-duplex and full-duplex modes. BlueZ manages Bluetooth; PipeWire
and WirePlumber provide headset audio. Each headset microphone is routed to the
other headsets, never back to itself.

By default, each headset's play/pause button is matched automatically and its
microphone stays muted until the button is held. Half-duplex queues talk
requests and allows one talker at a time. Friends or coworkers
can pair their headsets with the same host to join.

[Watch the illustrative CLI demo](https://phieri.github.io/bt-intercom/) ·
[Headset user briefing](https://phieri.github.io/bt-intercom/slides.html)

## Requirements

- Linux with Bluetooth Classic, BlueZ (`bluetoothctl`), PipeWire (`pw-dump`,
  `pw-cli`, `pw-play`) and WirePlumber. Run as the same user as PipeWire,
  **not with `sudo`**.
- At least two Bluetooth Classic headsets with HFP/HSP microphone and speaker
  audio. The default half-duplex transport (`sco-a2dp`) also needs A2DP
  playback on every headset. Check devices with `wpctl status`.

Raspberry Pi Zero W, Zero 2 W, Pi 3, Pi 4 and Pi 5 have onboard Bluetooth
Classic radios. Pi 1, Pi 2 and the original Zero need a compatible USB adapter.

On Raspberry Pi OS:

```sh
sudo apt install bluez pipewire pipewire-bin wireplumber libspa-0.2-bluetooth
systemctl --user status pipewire wireplumber
```

### Controller capacity

A paired device is not necessarily an available voice channel. HFP/HSP duplex
audio uses a synchronous SCO/eSCO link, and by default bt-intercom budgets
**one such link per Bluetooth controller**, including a Pi's onboard radio.
Raise this with `run --sco-limit N` (and `--a2dp-limit N` for A2DP streams, default 1) only after verifying the exact controller,
firmware and headset combination.

- **Full-duplex** needs at least two SCO/eSCO slots in total: by default, one
  independent controller per headset. `run` rejects full-duplex when BlueZ
  reports too few. Extra USB adapters do not move existing bonds, and
  bandwidth, interference, USB power and firmware can still limit quality.
- **Half-duplex** with the default `sco-a2dp` transport needs fewer controllers:
  BlueZ allows one active A2DP stream per controller in addition to its SCO
  budget, so each controller supports two headsets (one talker, one listener).
  A Pi with only its onboard radio supports two; each USB adapter adds two.
  `run` refuses to start when the headsets exceed what the detected controllers
  (or any already-connected controller's headsets) support; `status` and
  `diagnose` warn. See [SCO/A2DP transport](#scoa2dp-half-duplex-transport).

## Installation

Build on the device:

```sh
cargo install --path . --locked
```

Or download an executable from a successful build artifact and install it as
`~/.local/bin/bt-intercom`. Each build also publishes `.deb` and `.rpm`
packages. Choose the artifact for both the Pi model and the OS bitness:

| Raspberry Pi model | OS | Artifact target |
| --- | --- | --- |
| Pi 1, original Zero, Zero W | 32-bit | `arm-unknown-linux-gnueabihf` (ARMv6) |
| Pi 2, Zero 2 W, Pi 3, Pi 4 | 32-bit | `armv7-unknown-linux-gnueabihf` (ARMv7) |
| Zero 2 W, Pi 3, Pi 4, Pi 5 | 64-bit | `aarch64-unknown-linux-gnu` |

Pi 5 needs a 64-bit OS. ARMv6 binaries also run on ARMv7 systems, but not the
reverse. Artifacts need a compatible glibc; other Linux systems can build
natively with Cargo.

### Packages

Install the `.deb` or `.rpm` with your package manager. To build them
yourself, install `cargo-deb` and `cargo-generate-rpm`, then run `cargo deb`
or `cargo generate-rpm` (add `--target TARGET` when cross-compiling). Output
goes to `target/debian/` and `target/[TARGET/]generate-rpm/`.

Packages include a man page and a systemd user unit that is installed but not
enabled.

### Shell completions

Generate completions at runtime and save them where your shell looks for them,
for example:

```sh
mkdir -p ~/.local/share/bash-completion/completions
bt-intercom completions bash > ~/.local/share/bash-completion/completions/bt-intercom
```

Supported shells: `bash`, `zsh`, `fish`, `elvish` (and others listed by
`bt-intercom completions --help`).

## Usage

### Quick start

Put each headset in pairing mode, then scan, pair, check and run:

```sh
bt-intercom scan
bt-intercom pair AA:BB:CC:DD:EE:01
bt-intercom pair AA:BB:CC:DD:EE:02
bt-intercom status AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02
bt-intercom run AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02
```

Replace the example addresses with those from your scan. Run `bt-intercom --help`
or `bt-intercom run --help` for all options, and `bt-intercom --version` for
the version and UTC build datetime.

- `pair` needs an interactive terminal. At the `bluetoothctl` prompt, enter the
  printed `pair ADDRESS` command, answer any PIN or confirmation prompt, then
  enter `quit`. The program then verifies pairing and trusts and connects the
  device. If a headset is incompatible with the agent, pair it with
  `bluetoothctl` or your desktop's Bluetooth UI instead.
- `status` reports each headset's Bluetooth connection state, name and PipeWire
  microphone and speaker ports. It does not connect devices or create routes.
- If `status` shows Bluetooth disconnected, `run` connects headsets
  automatically. If duplex audio is unavailable, check that the headset is
  connected and using its HFP/HSP profile in PipeWire (select it with
  `wpctl set-profile DEVICE_ID PROFILE_INDEX`).
- Pair and trust only devices you have permission to connect. BlueZ stores
  pairing credentials; bt-intercom stores only headset addresses.

### Running

`run` saves its addresses to `${XDG_CONFIG_HOME:-~/.config}/bt-intercom/headsets`
(`~/.config` is used if `XDG_CONFIG_HOME` is unset or not absolute). Later,
`bt-intercom run` with no addresses restores them. Remove one with
`bt-intercom remove ADDRESS`; this neither disconnects nor unpairs it, and a
running intercom or service must be restarted. Invalid settings or unavailable
button inputs never overwrite the saved network.

- Disconnected headsets are connected at startup and retried every 30 seconds.
  Use `--no-connect` if another Bluetooth manager keeps them connected.
- Routing is polled every 2 seconds; change this with `--interval SECONDS`
  (minimum 1).
- Ctrl-C or SIGTERM closes the links this process created and stops all
  workers. Existing PipeWire links are never modified.
- `--sco-limit N` sets the SCO/eSCO budget per controller (default 1). With
  `sco-a2dp` each controller also hosts `--a2dp-limit` A2DP listeners; with
  `hfp` it only warns when the number of headsets on a controller exceeds the
  budget.
- `--a2dp-limit N` sets the assumed A2DP streams per controller (default 1).
  With `sco-a2dp`, talkers use HFP/SCO and listeners stream A2DP up to this
  limit; further listeners on the same controller keep HFP/SCO (receiving only),
  so a controller supports `--a2dp-limit` plus `--sco-limit` headsets in total,
  for example one A2DP listener and one HFP listener while a talker on another
  adapter transmits. `run` rejects networks exceeding this at startup, and routing reports
  a listener capacity conflict instead of switching profiles if more headsets
  connect later. Also accepted by `diagnose`; raise only after verifying hardware.
- Best-effort beeps confirm that a route is active. The temporary WAV file is
  created under an absolute `XDG_RUNTIME_DIR`, else the system temporary
  directory, and is removed at shutdown.

Diagnostics go to stderr at `info` level. Add `--verbose` for debug output, or
set `RUST_LOG` (for example `RUST_LOG=warn`), which overrides `--verbose`.
`-v` is an alias for `--version`. Terminal views use color when supported,
respect `NO_COLOR`, and only clear the screen on an interactive terminal.

### Duplex modes and push-to-talk

`run` defaults to half-duplex; choose with `--mode half-duplex|full-duplex`.
Every headset starts in push-to-talk (PTT) using its play/pause button, which is
matched automatically. The headsets must be connected when `run` starts, and
startup fails if an input device cannot be matched uniquely.

PTT listens only for play/pause press and release events: `KEY_PLAYPAUSE` (164)
and the `KEY_PLAYCD` (200)/`KEY_PAUSECD` (201) codes BlueZ emits for AVRCP
PLAY/PAUSE, not call/answer buttons or keyboard input. No MPRIS player is
needed. Hold the button to transmit; release to
mute. A double beep confirms that the microphone route is active.

In **full-duplex**, press play/pause three times within one second to switch a
headset between PTT and always-open. These choices reset when the process exits
or the headset loses duplex audio.

In **half-duplex**, holding the button requests a turn. If another headset is
talking, the request waits in first-in, first-out order; keep holding and wait
for the double beep before talking. Releasing cancels a queued request or ends
your turn. All headsets share one queue. Triple presses do nothing. A routing
failure stops the run and releases its links rather than leave the previous
talker active. After a headset loses duplex audio and reconnects, release and
press again to talk.

To override automatic matching, map each headset to a Linux input event device:

```sh
bt-intercom run AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02 \
  --ptt AA:BB:CC:DD:EE:01=/dev/input/event4 \
  --ptt AA:BB:CC:DD:EE:02=/dev/input/event5
```

Use `--ptt ADDRESS=auto` (or a lone `--ptt auto`) to request matching
explicitly. Find devices with `evtest` or `libinput debug-events`, and prefer
stable `/dev/input/by-id/` or `by-path/` names. The running user must be able to
read the devices, and the headset must send distinct press and release events.
Missing or unreadable devices stop PTT from starting. If a device disappears
while running (BlueZ recreates it when a headset reconnects or AVRCP restarts),
the button is released and the reader reattaches by headset address once the
device reappears. PTT is not a privacy boundary: a failed mute can
leave audio active until a retry succeeds.

### Diagnostics

`bt-intercom diagnose [ADDRESS...]` (saved network when omitted) tests the real
hardware. It powers on the adapter, connects each headset, switches each one to
A2DP and HFP and checks that PipeWire exposes working ports, then switches one
headset per controller after another to HFP to **measure how many simultaneous
SCO links that controller sustains** (use the result as `--sco-limit`). It
reports headsets connected at once, per-profile failures and timings, then the
BlueZ/PipeWire summary (pairing, trust, controller roles, capacity bounds).
Stop any running intercom first: audio is interrupted for several minutes at
most, profile changes are unsaved and every headset ends with all profiles released
(PipeWire `off`; the Bluetooth connection stays up), so none holds the
controller's single A2DP stream. `run` selects profiles itself. The measured
limit is bounded by the number of headsets you pass; add more to probe further.
`--passive` only inspects state without connecting or switching. `run` logs the
passive findings at startup as non-fatal warnings.

Capacity model: by default one active A2DP stream and one SCO link are
assumed per adapter (override with `--a2dp-limit` and `--sco-limit`), so half-duplex supports two headsets per adapter (one talker, one
listener). A Pi with only its internal chip supports two headsets; each added
USB Bluetooth adapter adds two more.

### SCO/A2DP half-duplex transport

Half-duplex defaults to `--transport sco-a2dp` (full-duplex defaults to `hfp`).
With `hfp`, half-duplex only gates PipeWire links: listeners still use SCO/eSCO,
so it does **not** relieve a controller's synchronous-link limit.

`sco-a2dp` keeps idle listeners on A2DP and switches granted talkers to HFP/HSP,
within the `--sco-limit` budget per controller. Before granting a SCO link, it closes
old routes, switches listeners to A2DP and waits for the profile changes to
appear. Profile indices come from PipeWire, and SBC is preferred for
compatibility and latency (voice needs no high-resolution audio), then SBC-XQ,
AAC and other codecs; high-resolution codecs (LDAC, aptX HD) are used only when a
headset offers nothing else. WirePlumber negotiates the actual bitrate. The
choice is made per headset, so mixed headsets each use their own best match. `bt-intercom
diagnose` lists the codecs each connected headset offers and warns when BlueZ
reports a profile that PipeWire hides, usually because WirePlumber's
`bluez5.codecs` or `bluez5.roles` exclude it. Headsets often bring up HFP and
A2DP separately, so startup diagnosis waits up to about 6 seconds and asks BlueZ
to connect a connected headset's listed but missing profile before warning.
Diagnosis also warns when another program's PipeWire stream is linked to an
allowlisted headset (links not created by bt-intercom). Only allowlisted devices are changed, and missing controller identity
or required profiles produce errors instead of assumed capacity.

```sh
bt-intercom run AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02 \
  --mode half-duplex --transport sco-a2dp
```

Notes:

- A headset cannot listen over A2DP while its own HFP profile is active, so
  it may briefly be unable to receive while it talks.
- A short absence of microphone ports during a switch does not cancel a held
  request; a genuine device disappearance does.
- Half-duplex uses one profile per headset at a time, so only the profile for
  its current role must be available: HFP for the talker, A2DP for listeners.
  Some headsets (for example CMF Buds 2) bring up HFP and A2DP separately; the
  session waits up to 30 seconds for the needed profile, asks BlueZ every 5
  seconds to connect it (`bluetoothctl connect-profile`), and the error names
  the missing profile and any BlueZ refusal. `bluetoothctl` often exits 0 on
  failure, so its output is checked.
- Reconnection runs one headset at a time, powers on a powered-off adapter,
  trusts untrusted devices, and skips (with a warning) unpaired or blocked ones.
- `status` and the dashboard report HFP duplex readiness, so an idle A2DP
  listener appears playback-only.
- Profiles stay in their last state on exit; owned links are always released.
  Avoid running another profile manager against these devices.

**Hardware validation is required.** Switching can interrupt audio for seconds,
A2DP adds latency, and some headsets drop play/pause release events across
profile changes. WirePlumber's automatic profile switching may compete with this
policy: disable it for your deployment and test floor handoffs, disconnects and
shutdown on the real devices.

#### Pairing across controllers

Select each controller **before** pairing its headset in an interactive
`bluetoothctl` session:

```text
list
select CONTROLLER_MAC
scan on
pair HEADSET_MAC
trust HEADSET_MAC
connect HEADSET_MAC
scan off
quit
```

Repeat for each controller. A headset already bonded to the wrong adapter must
be removed before re-pairing; bt-intercom never migrates bonds. Automatic
reconnection uses BlueZ's default controller, so use `--no-connect` and an
adapter-aware Bluetooth manager (or reconnect after `select`) for
multi-controller setups. Confirm controllers and profiles with `pw-dump` and
`wpctl status`.

#### Verifying your hardware

Support varies by adapter, headset and kernel, so verify on real hardware: the
SCO/eSCO stream count per radio (`btmon` shows synchronous connection
completion; see the BlueZ
[SCO](https://github.com/bluez/bluez/blob/master/doc/sco-protocol.rst) and
[HFP tracing](https://github.com/bluez/bluez/blob/master/doc/btmon-hfp.rst)
docs), microphone audio reaching every intended listener,
profile handoff timing, and A2DP stability under full listener load. A linked
PipeWire graph and a confirmation beep show routing state, not voice quality or
certified controller capacity. Auracast (LE Audio broadcast) is a different
transport and is not supported.

## Monitoring

### Dashboard

Add `--dashboard` to `run` for a live view of the mode, per-headset PTT, floor
and queue state, Bluetooth status, duplex availability, owned route counts, and
battery and RSSI. It needs an interactive terminal on stderr and is not meant
for services. Route counts show observed links between configured headsets, not
speech or audio quality. Failed Bluetooth queries show as unknown with the
error until a later poll succeeds. Battery and RSSI come from `bluetoothctl
info` and appear only if the headset exposes them (battery over HFP may need
`bluetoothd --experimental`; RSSI on BR/EDR links is often absent), otherwise
shown as "—".

### User service

The packaged unit runs `/usr/bin/bt-intercom run` with the saved addresses and
default options. After configuring the headset network:

```sh
systemctl --user daemon-reload
systemctl --user enable --now bt-intercom.service
journalctl --user -u bt-intercom.service -f
```

For a manual install under `~/.local/bin`, copy `examples/bt-intercom.service`
to `~/.config/systemd/user/` first. The user's PipeWire/WirePlumber session must
be available; to start before login, run `sudo loginctl enable-linger "$USER"`
and configure WirePlumber's Bluetooth seat policy for headless use. Stop the
unit with `systemctl --user stop bt-intercom.service` to release its routes.

## Limits and development

Capacity and audio behavior depend on the adapter, firmware, OS and headset;
simultaneous HFP/HSP connections are not guaranteed. This is not a network
intercom, and there is no automatic bond migration, Auracast, echo cancellation
or audio processing. Headsets need acoustic isolation to avoid feedback. The
host needs no local microphone or speaker.

Host tests use synthetic PipeWire graphs and do not verify Bluetooth hardware or
audio transport, so test devices, reconnects, PTT and shutdown before relying
on a deployment. Run the same checks as CI:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```
