# rpi-intercom

A local, full-duplex intercom between Bluetooth headsets connected to one
Raspberry Pi. It uses BlueZ for pairing, and PipeWire/WirePlumber for the
Bluetooth audio profiles and audio transport. Unlike the Pico feasibility
probe, this application does not implement Bluetooth codecs or profiles
itself: PipeWire provides Classic HFP/HSP and, where supported by the installed
BlueZ/PipeWire stack and adapter, LE Audio BAP.

Each allowlisted headset microphone is connected to the speakers of every
*other* allowlisted headset. Audio is never sent to its own headset. The
router automatically follows headset disconnections and reconnections; it
only removes links it created.

## Requirements

- Raspberry Pi Zero W/2 W or larger, with a Bluetooth adapter capable of the
  required profile(s); an original Pi Zero needs a USB Bluetooth adapter.
- Linux with BlueZ (`bluetoothctl`), PipeWire (`pw-dump`, `pw-link`),
  WirePlumber, and Python 3.10+ running in the **same user session** as
  PipeWire. Install the distribution's Bluetooth/PipeWire packages and enable
  the Bluetooth and user audio services.
- Two or more Bluetooth headsets offering **both** a microphone and speaker
  to PipeWire. For Classic devices, select the bidirectional HFP/HSP headset
  profile (A2DP is playback-only); check `wpctl status` and, if necessary,
  select the duplex profile with `wpctl set-profile DEVICE_ID PROFILE_INDEX`.
  LE Audio requires a working BAP-capable controller, headset, BlueZ and
  PipeWire build. Classic support does not imply LE Audio support.

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
automatically. Repeat the command for any number of headsets. Each extra
headset increases the number of simultaneous audio links; adapter bandwidth
and profile limits may prevent large groups or mixed Classic/LE configurations.

If a headset is silent, inspect `wpctl status` and `pw-dump` to confirm that it
has both `Audio/Source` and `Audio/Sink` nodes and that the duplex profile is
active. This program does not force a Bluetooth profile: doing so could
override an existing user session. Without a duplex profile there is no
microphone to route. For a persistent installation, run the command as a
systemd **user** service after PipeWire and WirePlumber start, not as root.

The Pi needs no local microphone or speaker. Audio remains on the Pi and its
paired headsets; this is not a network intercom, a walkie-talkie protocol, or
an LE Audio implementation for unsupported hardware. Codec negotiation,
encryption and connection limits are determined by BlueZ and PipeWire.
Headsets need acoustic isolation to avoid feedback.

## Development

```sh
python3 -m unittest discover -s tests -v
```