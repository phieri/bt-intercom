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
  WirePlumber, and Python 3.10+ running in the **same user session** as
  PipeWire. Install the distribution's Bluetooth/PipeWire packages and enable
  the Bluetooth and user audio services.
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

Install in the PipeWire user's environment (for example, a virtual environment):

```sh
python3 -m pip install .
```

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

Start the intercom with the paired addresses:

```sh
rpi-intercom run --connect AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02
```

Leave it running in the foreground; Ctrl-C removes the links created by this
process. Omit `--connect` if your Bluetooth manager connects devices
automatically. Each extra headset increases the number of simultaneous audio
links. The onboard adapter's ability to maintain two or more concurrent
HFP/HSP headset connections depends on firmware, controller capacity and the
installed audio stack; it is **not guaranteed**, and has not been verified on
Zero W or Zero 2 W hardware.

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
python3 -m unittest discover -s tests -v
```