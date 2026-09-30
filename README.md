# rpi-intercom

A local, full-duplex **Bluetooth Classic** intercom between headsets connected
to one Raspberry Pi Zero W or Zero 2 W. It uses BlueZ for pairing and
PipeWire/WirePlumber for HFP/HSP headset audio. The application does not
implement Bluetooth codecs or profiles itself. It does **not** support
Bluetooth LE Audio.

Watch an [illustrative CLI session](https://phieri.github.io/rpi-intercom/)
replayed with the asciinema player. The `Deploy Pages` workflow downloads
the pinned player from its GitHub release during the website build and
deploys it alongside `docs/`; the player is not stored in the repository.
Set Pages source to **GitHub Actions** in the repository settings.

Each allowlisted headset microphone is connected to the speakers of every
*other* allowlisted headset. Audio is never sent to its own headset. The
router automatically follows headset disconnections and reconnections; it
only removes links it created. Each link belongs to a monitored `pw-cli`
connection; closing that connection releases the link without deleting a
numeric ID that PipeWire could have reused. Per-instance ownership properties
also identify the links in diagnostic snapshots.

## Requirements

- Raspberry Pi Zero W or Zero 2 W with its onboard Bluetooth radio. An
  original Pi Zero needs a USB Bluetooth Classic adapter instead.
- Linux with BlueZ (`bluetoothctl`), PipeWire (`pw-dump`, `pw-cli`, `pw-link`),
  and WirePlumber. Run the intercom in the **same user session** as PipeWire.
  Install the distribution's Bluetooth/PipeWire packages and enable the
  Bluetooth and user audio services.
- Two or more Bluetooth Classic headsets offering **both** a microphone and
  speaker to PipeWire. Select the bidirectional HFP/HSP headset profile
  (`headset-head-unit`); A2DP is playback-only. Check `wpctl status` and, if
  necessary, choose the duplex profile with
  `wpctl set-profile DEVICE_ID PROFILE_INDEX`. The router only uses nodes
  reporting `api.bluez5.profile = headset-head-unit`; it ignores other
  profiles even if they provide audio ports.

On a Debian-based Raspberry Pi OS with PipeWire packages available, install
BlueZ and the PipeWire Bluetooth plugin:

```sh
sudo apt install bluez pipewire wireplumber libspa-0.2-bluetooth
```

Start PipeWire and WirePlumber for your login user. Verify that
`systemctl --user status pipewire wireplumber` shows running services before
starting the intercom. Do **not** start this app with `sudo`: root's PipeWire
session will not have your headset nodes.

Install Rust and build the command-line binary on the Pi:

```sh
cargo build --release --locked
install -Dm755 target/release/rpi-intercom ~/.local/bin/rpi-intercom
```

Alternatively, download the appropriate binary from a successful GitHub Actions
build artifact and install it as `~/.local/bin/rpi-intercom`. Use
`arm-unknown-linux-gnueabihf` for Raspberry Pi Zero W (32-bit Raspberry Pi OS)
or `aarch64-unknown-linux-gnu` for Zero 2 W running 64-bit Raspberry Pi OS.
For Zero 2 W running 32-bit Raspberry Pi OS, use the 32-bit artifact.
Build artifacts are dynamically linked against glibc; build on the Pi if the
artifact is incompatible with your OS. The original Pi Zero also needs a USB
Bluetooth adapter.

## Usage

Discover devices, place each headset in pairing mode, and pair each one:

```sh
rpi-intercom scan --seconds 20
rpi-intercom pair AA:BB:CC:DD:EE:01
rpi-intercom pair AA:BB:CC:DD:EE:02
```

Only pair devices you own and recognize. Pairing uses BlueZ's local
KeyboardDisplay agent in an interactive shell. Enter the displayed
`pair ADDRESS` command, answer PIN/confirmation prompts, then enter `quit`;
the app verifies pairing before trusting and connecting that address. The shell
has a five-minute limit. Some headsets must instead be
paired using the desktop's Bluetooth UI or `bluetoothctl`. BlueZ stores bonds
and trust settings; this program does not store credentials.

Check whether PipeWire sees duplex audio for each headset before routing:

```sh
rpi-intercom status AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02
```

This reports microphone and speaker port counts, or that an address is not
found in PipeWire. It does not create links or connect devices. A device with
no duplex audio needs its Bluetooth connection and HFP/HSP profile checked
using `bluetoothctl info ADDRESS` and `wpctl status`.

Start the intercom with the paired addresses:

```sh
rpi-intercom run AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02 --connect
```

Leave it running in the foreground; Ctrl-C removes the links created by this
process. With `--connect`, a background worker checks BlueZ and retries
disconnected headsets, waiting 30 seconds between passes; omit it if your
Bluetooth manager connects devices automatically. Each headset can take up to
50 seconds to check/connect, but this does not block routing or headset-button
push-to-talk. Shutdown cancels an in-flight Bluetooth command. Each audio
link uses one monitored `pw-cli` subprocess. Each extra headset increases the
number of simultaneous audio links. The onboard adapter's ability to maintain
two or more concurrent HFP/HSP headset connections depends on firmware,
controller capacity and the installed audio stack; it is **not guaranteed**,
and has not been verified on Zero W or Zero 2 W hardware.

For opt-in push-to-talk, map **each** headset's play/pause button to its Linux
input event device. The app listens only for `KEY_PLAYPAUSE` (code 164), not
the headset's call/answer button. For example, if both headsets expose
play/pause through separate `/dev/input/event*` devices:

```sh
rpi-intercom run AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02 \
  --ptt AA:BB:CC:DD:EE:01=/dev/input/event4 \
  --ptt AA:BB:CC:DD:EE:02=/dev/input/event5
```

Identify each headset's play/pause event device with `evtest` or
`libinput debug-events`; prefer a stable `/dev/input/by-id/` or
`/dev/input/by-path/` symlink when available. The account running the intercom
needs permission to read those devices. Bluetooth headset buttons are **not**
universally exposed as Linux input events: this mode only works when your
headset and Bluetooth stack expose distinct `KEY_PLAYPAUSE` press and release
events for each headset. A headset that exposes only a call button is not
supported for PTT.
Do not map a keyboard input device. An unmapped or unreadable button prevents
PTT from starting.

All headset microphones start muted. Holding a headset's play/pause button connects
**that headset's microphone** to the other headsets; releasing disconnects it.
Repeated key events are ignored. Enter on the Pi does nothing. If an event
device closes or fails, the command exits and releases its links. Only links
created by this process are controlled; pre-existing PipeWire links between
headsets are not modified. Routing is polled every two seconds (adjust with
`--interval SECONDS`), with button events checked every 100 ms. PipeWire
command execution can add latency; this is not hard real-time PTT.
Routing failures are logged and retried, including when PipeWire restarts.
If a mute operation fails, audio may continue until a retry succeeds: PTT is
not a privacy/security boundary.

For an opt-in live console view, add `--dashboard` to `run` in an interactive
terminal (it requires a terminal on stderr). The view refreshes as routing and
Bluetooth status change and shows each requested headset's paired/connected
status, duplex availability, owned active PipeWire link counts (outgoing and
incoming), and Bluetooth RSSI when BlueZ reports it. `?` means Bluetooth
status has not been obtained; `unknown` signal means RSSI is unavailable,
not necessarily a poor connection. Link counts show established routes,
**not** measured speech, throughput, or packet loss. The display does not
change pairing, connections, profiles, or audio routing. Omit `--dashboard`
for a systemd service or redirected logs. It works with headset-button PTT.

If a headset is silent, inspect `wpctl status` and `pw-dump` to confirm that it
has both `Audio/Source` and `Audio/Sink` nodes and that the duplex profile is
active. This program does not force a Bluetooth profile: doing so could
override an existing user session. Without a duplex profile there is no
microphone to route. For a persistent installation, run the command as a
systemd **user** service after PipeWire and WirePlumber start, not as root.

### Unattended user service

An example unit is supplied in `examples/rpi-intercom.service`. Install the
binary as above, pair/trust the headsets, and select their duplex profiles first.
From the repository directory:

```sh
mkdir -p ~/.config/systemd/user ~/.config/rpi-intercom
cp examples/rpi-intercom.service ~/.config/systemd/user/
printf 'HEADSETS="AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02"\n' \
  > ~/.config/rpi-intercom/environment
# Replace the example addresses with your own before starting.
systemctl --user daemon-reload
systemctl --user enable --now rpi-intercom.service
journalctl --user -u rpi-intercom.service -f
```

The unit uses full-duplex mode, not headset-button PTT. For startup without an
interactive login, an administrator can enable lingering with
`sudo loginctl enable-linger "$USER"`. WirePlumber's Bluetooth seat policy must
also permit this user's headsets when no graphical session is active; consult
your installed WirePlumber version's BlueZ monitor/seat-monitoring settings.
Do not run competing desktop and headless audio sessions for the same adapter.
Stop with `systemctl --user stop rpi-intercom.service` to allow link cleanup.
The service manager also terminates helper processes if the main process
crashes. Outside the service, SIGKILL can leave orphaned `pw-cli -m` helpers:
terminate those specific helpers to release their links. New processes do not
take ownership of an earlier process's links. Normal cleanup does not require
a working `pw-dump`.

The Pi needs no local microphone or speaker. Audio remains on the Pi and its
paired headsets; this is not a network intercom or walkie-talkie protocol.
Codec negotiation, encryption and connection limits are determined by BlueZ
and PipeWire. Headsets need acoustic isolation to avoid feedback.

## Hardware validation and remaining limits

The tests exercise synthetic PipeWire graphs and subprocess behavior, not
Bluetooth radios or audio transport. Before relying on a deployment:

1. Verify each headset individually exposes both microphone and speaker ports
   with `status`, then verify both remain duplex-ready when connected together.
2. Test speech in both directions, isolation from each headset's own microphone,
   and PTT mute/unmute. Keep volume low initially to avoid feedback.
3. Power-cycle each headset, change its profile, and restart PipeWire; check that
   routing recovers and unrelated user-created links remain intact.
4. Test SIGINT/SIGTERM cleanup and the user service after a reboot without login.
5. Measure latency, dropouts, CPU use, and simultaneous SCO/eSCO connection
   capacity on the actual controller/firmware. More than two participants also
   needs verification of PipeWire input mixing, levels, and clipping.

No GPIO button control, automatic profile switching, echo cancellation,
gain normalization, or network transport is implemented. GPIO PTT needs a
specified pin/wiring and control policy; profile and audio policy remain with
BlueZ/WirePlumber. Direct links use PipeWire's format negotiation and mixing;
the application does not add a separate resampling or DSP pipeline.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

GitHub Actions runs these checks on Linux and cross-compiles release binaries
for both Raspberry Pi architectures.