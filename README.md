# rpi-intercom

A local, full-duplex Bluetooth Classic intercom for two or more headsets on one
Raspberry Pi. BlueZ manages Bluetooth; PipeWire and WirePlumber provide headset
audio. Each headset microphone routes to every other configured headset, never
back to itself. By default all microphones are live; optional push-to-talk (PTT)
keeps them muted until a headset button is held.

[Watch the illustrative CLI demo](https://phieri.github.io/rpi-intercom/).

## Requirements

- A Raspberry Pi with Bluetooth Classic: Zero W, Zero 2 W, Pi 3, Pi 4 and Pi 5
  have onboard radios. Pi 1, Pi 2 and the original Pi Zero need a compatible USB
  adapter.
- Linux with BlueZ (`bluetoothctl`), PipeWire (`pw-dump`, `pw-cli`, `pw-play`)
  and WirePlumber. Run as the same user as PipeWire, **not with `sudo`**.
- At least two Bluetooth Classic headsets that expose both microphone and
  speaker audio through the HFP/HSP `headset-head-unit` profile. A2DP alone is
  playback-only. Check profiles with `wpctl status`; select the duplex profile
  with `wpctl set-profile DEVICE_ID PROFILE_INDEX`. The program does not change
  profiles.

On Raspberry Pi OS with PipeWire packages:

```sh
sudo apt install bluez pipewire wireplumber libspa-0.2-bluetooth
systemctl --user status pipewire wireplumber
```

Start PipeWire and WirePlumber for your login user if they are not running.
Build on the Pi:

```sh
cargo build --release --locked
install -Dm755 target/release/rpi-intercom ~/.local/bin/rpi-intercom
```

Alternatively, download the executable from a successful build artifact and
install it as `~/.local/bin/rpi-intercom`.

Choose an artifact for both the Pi and OS architecture:

| Pi model | OS | Artifact target |
| --- | --- | --- |
| Pi 1, original Zero, Zero W | 32-bit | `arm-unknown-linux-gnueabihf` (ARMv6) |
| Pi 2, Zero 2 W, Pi 3, Pi 4 | 32-bit | `armv7-unknown-linux-gnueabihf` (ARMv7) |
| Zero 2 W, Pi 3, Pi 4, Pi 5 | 64-bit | `aarch64-unknown-linux-gnu` |

Pi 5 should use a 64-bit OS. ARMv6 binaries also run on compatible 32-bit ARMv7
systems; ARMv7 binaries do not run on ARMv6. Artifacts require a compatible
glibc.

## Usage

Put each headset in pairing mode, then scan and pair it:

```sh
rpi-intercom scan --seconds 20
rpi-intercom pair AA:BB:CC:DD:EE:01
rpi-intercom pair AA:BB:CC:DD:EE:02
```

Run `pair` in an interactive shell. At the BlueZ `KeyboardDisplay` prompt, enter
the printed `pair ADDRESS` command, answer any PIN or confirmation prompts, then
type `quit`; the program verifies pairing, trusts and connects the device. If a
headset is incompatible with the agent, pair it through the desktop's Bluetooth
UI or `bluetoothctl`. Verify PipeWire exposes duplex audio:

```sh
rpi-intercom status AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02
```

`status` reports each headset's name/address and microphone and speaker ports;
it does not connect devices or create routes. Pair and trust only devices you
own. BlueZ stores pairing credentials; this program stores only headset
addresses.

Start the intercom with the paired addresses:

```sh
rpi-intercom run AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02 --connect
```

Addresses are saved in `${XDG_CONFIG_HOME:-~/.config}/rpi-intercom/headsets`.
If `XDG_CONFIG_HOME` is unset or not an absolute path, they are saved under
`~/.config/rpi-intercom/headsets`. Later, run `rpi-intercom run --connect` to
restore them. `--connect` retries disconnected headsets every 30 seconds; omit
it if another Bluetooth manager keeps them connected. Routing is polled every
two seconds by default; change that with `--interval SECONDS`. Ctrl-C or SIGTERM
closes links created by this process. Existing PipeWire links are not modified.
Best-effort beeps confirm when an intercom route becomes active.

### Push-to-talk

Map each headset to its own Linux input event device. PTT listens only for
`KEY_PLAYPAUSE` press/release events (code 164), not call/answer buttons or
keyboard input:

```sh
rpi-intercom run AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02 \
  --ptt AA:BB:CC:DD:EE:01=/dev/input/event4 \
  --ptt AA:BB:CC:DD:EE:02=/dev/input/event5
```

Hold a headset's play/pause button to transmit from that headset's microphone;
release to mute it. A double beep confirms when its microphone route is active.
Find event devices with `evtest` or `libinput debug-events`.
Use stable `/dev/input/by-id/` or `/dev/input/by-path/` paths where available.
The running user needs permission to read the devices, and the Bluetooth stack
must expose distinct play/pause press and release events. Missing or unreadable
devices stop PTT from starting; a device failure while running exits and cleans
up. PTT is not a privacy boundary: a failed mute can leave audio active until a
retry succeeds.

### Dashboard and user service

Add `--dashboard` to `run` for a live terminal view of Bluetooth status, duplex
availability, owned route counts and optional RSSI. It requires an interactive
terminal on stderr and is not intended for a service. Route counts show links,
not measured speech or audio quality.

An example full-duplex user service is in `examples/rpi-intercom.service`. Pair
and configure the headsets first, then install and enable it:

```sh
mkdir -p ~/.config/systemd/user
cp examples/rpi-intercom.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now rpi-intercom.service
journalctl --user -u rpi-intercom.service -f
```

The unit uses `run --connect` and saved addresses; it does not enable PTT. The
user's PipeWire/WirePlumber session must be available. To start before login,
enable lingering with `sudo loginctl enable-linger "$USER"` and configure
WirePlumber's Bluetooth seat policy for headless use. Stop the unit to release
its routes:

```sh
systemctl --user stop rpi-intercom.service
```

## Limits and development

Bluetooth connection capacity and audio behavior depend on the adapter,
firmware, OS and headset; simultaneous HFP/HSP connections are not guaranteed.
The Pi needs no local microphone or speaker, but this is not a network
intercom. There is no GPIO control, automatic profile switching, echo
cancellation or audio processing. Headsets need acoustic isolation to avoid
feedback. Host tests use synthetic PipeWire graphs and do not verify Bluetooth
hardware or audio transport; test the actual devices, reconnects, PTT and
shutdown behavior before relying on a deployment.

Run the same checks as CI:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```
