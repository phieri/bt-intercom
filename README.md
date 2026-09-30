# rpi-intercom

A local, full-duplex **Bluetooth Classic** intercom between headsets connected
to one Raspberry Pi Zero W or Zero 2 W. It uses BlueZ for pairing and
PipeWire/WirePlumber for HFP/HSP headset audio. The application does not
implement Bluetooth codecs or profiles itself. It does **not** support
Bluetooth LE Audio.

Each allowlisted headset microphone is connected to the speakers of every
*other* allowlisted headset. Audio is never sent to its own headset. The
router automatically follows headset disconnections and reconnections; it
only removes links it created.

## Requirements

- Raspberry Pi Zero W or Zero 2 W with its onboard Bluetooth radio. An
  original Pi Zero needs a USB Bluetooth Classic adapter instead.
- Linux with BlueZ (`bluetoothctl`), PipeWire (`pw-dump`, `pw-link`),
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

Only pair devices you own and recognize. Pairing uses BlueZ's local agent;
some headsets require an interactive PIN/confirmation or must instead be
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
process. With `--connect`, the intercom checks BlueZ every 30 seconds and
retries disconnected headsets; omit it if your Bluetooth manager connects
devices automatically. A connection attempt can block the audio-routing loop
for up to 35 seconds per headset, so use automatic connection management if
prompt push-to-talk responses are required. Each extra headset increases the
number of simultaneous audio links. The onboard adapter's ability to maintain
two or more concurrent HFP/HSP headset connections depends on firmware,
controller capacity and the installed audio stack; it is **not guaranteed**,
and has not been verified on Zero W or Zero 2 W hardware.

For opt-in push-to-talk operation, add `--ptt` to `run`. All microphones start
muted. With the command running in an interactive terminal, press Enter once to
transmit to the other headsets, then press Enter again to mute; repeat for each
talk burst. This is a toggle control, not a press-and-hold key. Closing stdin
ends the command and removes its links. Only links created by this process are
muted; pre-existing PipeWire links between headsets are not modified.

If a headset is silent, inspect `wpctl status` and `pw-dump` to confirm that it
has both `Audio/Source` and `Audio/Sink` nodes and that the duplex profile is
active. This program does not force a Bluetooth profile: doing so could
override an existing user session. Without a duplex profile there is no
microphone to route. For a persistent installation, run the command as a
systemd **user** service after PipeWire and WirePlumber start, not as root.

The Pi needs no local microphone or speaker. Audio remains on the Pi and its
paired headsets; this is not a network intercom or walkie-talkie protocol.
Codec negotiation, encryption and connection limits are determined by BlueZ
and PipeWire. Headsets need acoustic isolation to avoid feedback.

## Development

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

GitHub Actions runs these checks on Linux and cross-compiles release binaries
for both Raspberry Pi architectures.