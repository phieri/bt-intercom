# bt-intercom

A local Bluetooth Classic intercom with selectable half-duplex and full-duplex
modes for two or more headsets on Linux. BlueZ manages Bluetooth; PipeWire and
WirePlumber provide headset audio. Each headset microphone routes to other
configured headsets in its talk groups, never back to itself. With no talk groups
configured, all headsets share the original all-to-all intercom. By default all
microphones are live; optional push-to-talk (PTT) keeps them muted until a
headset button is held. Half-duplex queues talk requests and allows only one
headset microphone per talk group at a time. Friends or coworkers can pair their
headsets with the same Linux host to join the conversation.

[Watch the illustrative CLI demo](https://phieri.github.io/bt-intercom/).

## Requirements

- Linux with Bluetooth Classic, BlueZ (`bluetoothctl`), PipeWire (`pw-dump`,
  `pw-cli`, `pw-play`)
  and WirePlumber. Run as the same user as PipeWire, **not with `sudo`**.
- At least two Bluetooth Classic headsets supporting HFP/HSP microphone and
  speaker audio. The default `--transport hfp` uses the `headset-head-unit`
  profile and leaves profiles unchanged. Check devices with `wpctl status`;
  select the duplex profile with `wpctl set-profile DEVICE_ID PROFILE_INDEX`.
  The opt-in `--transport sco-a2dp` also requires A2DP playback on every headset.

### Raspberry Pi hardware

Raspberry Pi Zero W, Zero 2 W, Pi 3, Pi 4 and Pi 5 have onboard Bluetooth
Classic radios. Pi 1, Pi 2 and the original Pi Zero need a compatible USB
adapter. The supported Linux artifact targets for these models are listed below.

**A paired/connected device is not necessarily an available voice channel.**
HFP/HSP duplex audio uses a synchronous SCO/eSCO link; A2DP playback uses an
asynchronous ACL link. Treat each consumer controller, including a Pi's onboard
radio, as having **one usable simultaneous SCO/eSCO audio link** unless you have
verified otherwise on that exact controller/firmware. Bluetooth Classic permits
more than one synchronous link in some configurations, so this is a conservative
deployment policy, not a universal Bluetooth limit. Neither the number of paired
devices nor the number of PipeWire microphone ports proves SCO capacity.

For full-duplex with N headsets, use N independent Bluetooth controllers and
pair one headset to each; merely plugging in extra USB adapters does not move
existing bonds or distribute audio. For fewer controllers, see
[SCO/A2DP half-duplex transport](#scoa2dp-half-duplex-transport) below. Additional
radios still share the 2.4 GHz spectrum; bandwidth, interference, USB power and
firmware can prevent reliable audio even with one headset per adapter.

For example, on Raspberry Pi OS with PipeWire packages:

```sh
sudo apt install bluez pipewire pipewire-bin wireplumber libspa-0.2-bluetooth
systemctl --user status pipewire wireplumber
```

Start PipeWire and WirePlumber for your login user if they are not running.
Build on the Pi:

```sh
cargo install --path . --locked
```

Alternatively, build and install the executable directly:

```sh
cargo build --release --locked
install -Dm755 target/release/bt-intercom ~/.local/bin/bt-intercom
```

Alternatively, download the executable from a successful build artifact and
install it as `~/.local/bin/bt-intercom`. Each build also publishes `.deb` and
`.rpm` packages for the same target.

### Shell completions and logging

Completions are generated from the CLI definition at runtime. Save the output in
the completion directory for your shell:

```sh
mkdir -p ~/.local/share/bash-completion/completions
bt-intercom completions bash > ~/.local/share/bash-completion/completions/bt-intercom

mkdir -p ~/.zfunc
bt-intercom completions zsh > ~/.zfunc/_bt-intercom

mkdir -p ~/.config/fish/completions
bt-intercom completions fish > ~/.config/fish/completions/bt-intercom.fish

mkdir -p ~/.config/elvish/lib
bt-intercom completions elvish > ~/.config/elvish/lib/bt-intercom.elv
```

For Zsh, add `fpath=(~/.zfunc $fpath)` before `compinit` in `~/.zshrc`. For
Elvish, add `use bt-intercom` to `~/.config/elvish/rc.elv`. Restart the shell
after configuring completions.

Runtime diagnostics are written to stderr and respect `RUST_LOG`. The default
level is `info`; add `--verbose` before or after a command (for example,
`bt-intercom run --verbose`) to enable debug diagnostics for subprocess
execution and routing decisions. An explicit `RUST_LOG` overrides this default;
set `RUST_LOG=warn` to show warnings only. `-v` remains an alias for `--version`.
A systemd user service sends stdout and stderr to the journal.

The terminal control panel uses semantic colors when the terminal supports
them; it respects `NO_COLOR`, and uses a plain style when color is unavailable.
The live dashboard only clears the screen when its output is an interactive
terminal.

### Debian and RPM packages

`Cargo.toml` includes packaging metadata for the binary, man page, runtime
requirements, an AppArmor profile, and a systemd user unit. The packages install
the profile at `/etc/apparmor.d/usr.bin.bt-intercom`; AppArmor must be enabled
on the host for it to be enforced. The profile allows the default configuration
and runtime paths, PipeWire and BlueZ access, read access to PTT input
devices, and Raspberry Pi GPIO access (not `/dev/mem`). Custom `XDG_CONFIG_HOME`
paths may need a local profile adjustment.
Custom `TMPDIR` paths may also need an adjustment for temporary PTT audio.
Install `cargo-deb` and
`cargo-generate-rpm`, then build packages with `cargo deb` and
`cargo generate-rpm` (add `--target TARGET` for a configured cross-compilation
target). The `.deb` is written to `target/debian/`; the `.rpm` is written to
`target/TARGET/generate-rpm/` when a target is specified, or
`target/generate-rpm/` otherwise. CI publishes both packages for each supported
target.

Install the package with your distribution's package manager. The systemd user
unit is installed but not enabled automatically. After configuring the saved
headset network, start it with:

```sh
systemctl --user daemon-reload
systemctl --user enable --now bt-intercom.service
```

The packaged unit runs `/usr/bin/bt-intercom`; the example unit below remains
for manual installations under `~/.local/bin`.

For Raspberry Pi, choose an artifact for both the model and OS architecture:

| Raspberry Pi model | OS | Artifact target |
| --- | --- | --- |
| Pi 1, original Zero, Zero W | 32-bit | `arm-unknown-linux-gnueabihf` (ARMv6) |
| Pi 2, Zero 2 W, Pi 3, Pi 4 | 32-bit | `armv7-unknown-linux-gnueabihf` (ARMv7) |
| Zero 2 W, Pi 3, Pi 4, Pi 5 | 64-bit | `aarch64-unknown-linux-gnu` |

Pi 5 should use a 64-bit OS. ARMv6 binaries also run on compatible 32-bit ARMv7
systems; ARMv7 binaries do not run on ARMv6. Artifacts require a compatible
glibc. These targets describe supported Raspberry Pi builds; other Linux systems
can build natively with Cargo.

## Usage

Show the installed version and UTC build datetime:

```sh
bt-intercom --version
```

Put each headset in pairing mode, then scan and pair it:

```sh
bt-intercom scan --seconds 20
bt-intercom pair AA:BB:CC:DD:EE:01
bt-intercom pair AA:BB:CC:DD:EE:02
```

Run `pair` in an interactive shell. At the BlueZ `KeyboardDisplay` prompt, enter
the printed `pair ADDRESS` command, answer any PIN or confirmation prompts, then
type `quit`; the program verifies pairing, trusts and connects the device. If a
headset is incompatible with the agent, pair it through the desktop's Bluetooth
UI or `bluetoothctl`. Verify PipeWire exposes duplex audio:

```sh
bt-intercom status AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02
```

`status` reports each headset's name/address and microphone and speaker ports;
it does not connect devices or create routes. Pair and trust only devices you
have permission to connect; friends or coworkers can pair their headsets with
the host. BlueZ stores pairing credentials; this program stores only headset
addresses.

Start the intercom with the paired addresses:

```sh
bt-intercom run AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02 --connect
```

Addresses are saved in `${XDG_CONFIG_HOME:-~/.config}/bt-intercom/headsets`.
If `XDG_CONFIG_HOME` is unset or not an absolute path, they are saved under
`~/.config/bt-intercom/headsets`. Later, run `bt-intercom run --connect` to
restore them. `--connect` retries disconnected headsets every 30 seconds; omit
it if another Bluetooth manager keeps them connected. Routing is polled every
two seconds by default; change that with `--interval SECONDS`. Ctrl-C or SIGTERM
closes links created by this process. Existing PipeWire links are not modified.
Shutdown also cancels and joins background workers, including idle headset
button readers.
Run settings, saved-network mappings and talk groups are validated before workers
start. Invalid settings or unavailable button inputs do not replace the saved
network with explicitly supplied addresses.
Remove a headset from the saved intercom network with `bt-intercom remove ADDRESS`.
This does not disconnect or unpair it; restart a running intercom or service
for the change to take effect. Removing the last address clears the saved
network.
Best-effort beeps confirm when an intercom route becomes active. Their temporary
WAV file is created under `XDG_RUNTIME_DIR` when it is an absolute path, falling
back to the system temporary directory otherwise; it is removed during shutdown.

### Raspberry Pi headless pairing button

On a supported Raspberry Pi, `run --pair-button` enables a normally-open
momentary button on **BCM GPIO17, physical header pin 11**. With the Pi powered
off, wire the button between pin 11 and **GND, physical pin 9**. The input uses
the internal 3.3 V pull-up, so pressing the button pulls it low; no external
pull-up is needed. Never connect it to 5 V. Reserve GPIO17 for this button;
do not use it with a HAT, overlay, or another GPIO application.

Run as your PipeWire user, with access to `/dev/gpiomem` (older Pis),
`/dev/gpiomem0` (Pi 5), and `/dev/gpiochip*`. Raspberry Pi OS normally grants
this through the `gpio` group. If necessary:

```sh
sudo usermod -aG gpio "$USER"
```

Log out and back in for group membership to take effect. Ensure Bluetooth is
powered on and BlueZ permits this user to pair/trust devices. Do **not** run the
intercom with `sudo`.

```sh
bt-intercom run --connect --pair-button
```

This can start without a saved network. Put **only the intended headset** in
pairing mode, then press and release the button. Each debounced press scans
Bluetooth Classic for 15 seconds and pairs only when exactly one observed,
unpaired HFP/HSP headset is found. Existing network devices, already-paired
devices, and playback-only A2DP devices are skipped; ambiguous discovery fails
without pairing. Headsets must advertise a headset/handsfree service UUID during
discovery and support **Just Works** pairing without a PIN or confirmation.
Other headsets still need interactive `pair` or a Bluetooth UI.

Successful pairing is verified, trusted, and saved in the normal headset network,
then added to the running intercom without a restart. Connection is attempted
immediately; `--connect` also retries newly enrolled headsets. Pairing runs in
the background, leaving existing audio routing active. Beeps confirm an active
intercom route, not pairing alone; one headset by itself has no intercom route.
Failures are logged and can be retried with a fresh press. Holding the button,
contact bounce, a button held at startup, and presses during pairing do not
start repeated attempts. Ctrl-C/SIGTERM cancels pairing and releases the GPIO.
Incomplete attempts remove the newly selected device's bond so it remains
eligible for a retry; if cleanup fails, the log gives a manual recovery command.
Headsets fully paired and trusted before shutdown are still saved for the next run.

This option is disabled by default, errors clearly on unsupported boards or
missing GPIO permissions, and requires full-duplex **without `--ptt`**. New
headsets still require the duplex PipeWire profile; configured talk groups are
unchanged, so add the new headset to a group when groups are in use.
Just Works does not authenticate the headset's identity: pair only in a trusted
environment and verify the enrolled address in the logs.

For the packaged headless user service, use `systemctl --user edit bt-intercom`
and add:

```ini
[Service]
ExecStart=
ExecStart=/usr/bin/bt-intercom run --connect --pair-button
```

Then run `systemctl --user daemon-reload` and
`systemctl --user restart bt-intercom`. For startup without a login, enable user
lingering with `sudo loginctl enable-linger "$USER"` and ensure PipeWire and
WirePlumber start in that user's session. Inspect pairing logs with
`journalctl --user -u bt-intercom`.

### Terminal control panel and talk groups

Start the Ratatui control panel after saving a headset network:

```sh
bt-intercom tui
```

The panel shows Bluetooth connection and signal status plus PipeWire duplex
availability. Use `Tab` to switch between the talk-group and member lists,
arrow keys to select, `n` to create a group, `d` to remove the selected group,
and `Space` to toggle headset membership. Enter saves a new group name; Esc
cancels name entry or quits the panel. Group settings are saved to
`${XDG_CONFIG_HOME:-~/.config}/bt-intercom/talk-groups.json`; changes are
picked up by a running `bt-intercom run` process on its next routing update.
A headset's microphone is routed only to other headsets sharing at least one
group with it. Headsets not assigned to a group are not routed when any groups
exist. With no groups, routing remains all-to-all. The panel is included in the
standard CLI and cross-compiled packages.

### Duplex modes and push-to-talk

Select `--mode full-duplex` (the default) or `--mode half-duplex` on `run`.
Full-duplex without button mappings starts with every microphone always open.
With mappings, each headset starts in PTT mode; its user can independently switch
between PTT and always-open by pressing the play/pause button three times within
one second. Switching back to PTT mutes the microphone when the button is
released. These choices reset when the process exits or that headset loses
duplex audio.

Map each headset to its own Linux input event device. PTT listens only for
`KEY_PLAYPAUSE` press/release events (code 164), not call/answer buttons or
keyboard input:

```sh
bt-intercom run AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02 \
  --ptt AA:BB:CC:DD:EE:01=/dev/input/event4 \
  --ptt AA:BB:CC:DD:EE:02=/dev/input/event5
```

In PTT mode, hold a headset's play/pause button to transmit from its microphone;
release to mute it. A double beep confirms when its microphone route is active.

For half-duplex, add `--mode half-duplex` to the command above. Button mappings
are required for every headset. Hold play/pause to request a turn: if another
headset is transmitting, your request waits in first-in, first-out order.
Keep holding while queued and wait for the double beep before talking.
Releasing the button cancels a queued request or ends your turn, allowing the
next waiting headset in that group to transmit. Each configured talk group has
an independent queue, so separate groups can have active talkers at the same
time. A headset in multiple groups requests a turn in each and is routed only
to groups where it currently holds the floor. With no talk groups configured,
all headsets share one queue. Triple presses do not enable always-open
microphones in half-duplex.
Half-duplex routing failures stop the run and release its owned links rather
than risk leaving the previous talker active. Existing external links remain
untouched, so exclusivity applies only to routes managed by this process.
When a mapped headset loses duplex audio, its request and always-open choice
are reset; after reconnecting, release and press again to talk.

### SCO/A2DP half-duplex transport

`--mode half-duplex` alone only gates PipeWire links: with the default HFP
transport, listeners still need SCO/eSCO for their speakers. It does **not** solve
a shared controller's synchronous-link limit.

For headsets sharing a controller, opt into profile switching:

```sh
bt-intercom run AA:BB:CC:DD:EE:01 AA:BB:CC:DD:EE:02 --connect \
  --mode half-duplex --transport sco-a2dp \
  --ptt AA:BB:CC:DD:EE:01=/dev/input/event4 \
  --ptt AA:BB:CC:DD:EE:02=/dev/input/event5
```

This transport selects advertised PipeWire profile indices rather than assuming
fixed numbers. Idle/listening headsets use A2DP; the granted talker uses HFP/HSP
for its microphone. Before granting another SCO link on a controller, the
program closes its old routes, switches listeners to A2DP, and waits until the
profile changes are observed before enabling the new talker's HFP profile.
Only allowlisted Bluetooth devices are changed. Missing controller identity or
required profiles produce actionable errors instead of assuming extra capacity.
Temporary absence of microphone ports during a switch does not cancel a held
PTT request. A genuine device disappearance retires the request.
Status/dashboard duplex availability still describes HFP microphone and speaker
ports: an idle A2DP listener is intentionally playback-only, not duplex-ready.

Talk-group routing remains isolated. Each group keeps its FIFO queue, but groups
sharing a radio also share its single SCO slot. Different radios can grant
independent talkers. A headset cannot listen through A2DP while its own HFP
profile is active, so overlapping groups may have listeners temporarily unable
to receive; no simultaneous HFP+A2DP capability is assumed. Profiles are left
in their last selected state on exit, rather than restoring multiple HFP
profiles and immediately recreating the capacity conflict. Owned links are
always released; existing external links and other applications' profile policy
are not controlled. Avoid running another profile manager against these devices.

**Hardware validation is required.** HFP/A2DP switching can interrupt audio and
take seconds, and A2DP adds codec/buffering latency. The headset must preserve
play/pause press **and release** events across profile changes; some do not.
WirePlumber automatic profile switching may compete with this policy. Disable
competing automatic switching for your deployment and test floor handoffs,
queued releases, disconnects, cancellation, and shutdown on the actual hardware.
This is not seamless voice conferencing, and ACL/A2DP capacity is not unlimited.

#### Pairing across controllers

Use an interactive `bluetoothctl` session to select each controller **before**
scanning, pairing, trusting and connecting its assigned headset:

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

Repeat with another controller for the next headset. If a headset was already
paired to the wrong adapter, remove that bond deliberately before re-pairing;
the intercom never migrates bonds. Confirm controller paths and audio profiles
with `pw-dump` and `wpctl status`. The GPIO pairing button uses BlueZ's default
controller; it is not a multi-controller enrollment/load-balancing mechanism.
`--connect` also uses BlueZ's default controller. Omit it for multi-controller
deployments and use an adapter-aware Bluetooth manager to maintain connections,
or reconnect manually after `select` in the same `bluetoothctl` session.
Selections are process-local; selecting in one invocation does not configure
the next invocation. Ordinary `info`/`connect` object-path arguments are not
portable substitutes for controller selection.

#### Why not Auracast?

Auracast is Bluetooth **LE Audio broadcast**, not an alternative codec or profile
for Bluetooth Classic A2DP/HFP. It requires a broadcast-capable LE Isochronous
controller, compatible firmware/kernel/BlueZ/PipeWire support and Auracast
receivers. An onboard Classic-capable Pi radio or an ordinary A2DP headset does
not establish those capabilities. This release does not configure broadcast
sources, broadcast discovery/assistant services, or LE Audio microphone
unicast; it never silently treats A2DP or arbitrary LE nodes as Auracast.

#### Evidence and deployment checks

The transport design follows upstream documentation, not a claimed universal
one-SCO hardware specification:

- [BlueZ SCO/eSCO protocol](https://github.com/bluez/bluez/blob/master/doc/sco-protocol.rst):
  synchronous point-to-point links reserve radio slots.
- [BlueZ HFP tracing](https://github.com/bluez/bluez/blob/master/doc/btmon-hfp.rst):
  verify synchronous connection completion and actual audio packets with `btmon`,
  rather than interpreting a successful generic `connect` as audio readiness.
- [BlueZ device API](https://github.com/bluez/bluez/blob/master/doc/org.bluez.Device.rst):
  device objects are adapter-scoped; `Connect` succeeds when at least one profile
  connects, not necessarily the voice profile.
- [WirePlumber profile switching](https://pipewire.pages.freedesktop.org/wireplumber/daemon/configuration/settings.html):
  automatic HFP switching is a separate policy and may compete with this program.
- [PipeWire hardware quirks](https://github.com/PipeWire/pipewire/blob/master/spa/plugins/bluez5/bluez-hardware.conf):
  support can depend jointly on the adapter, headset and kernel.
- [BlueZ ISO protocol](https://github.com/bluez/bluez/blob/master/doc/iso-protocol.rst):
  connected LE Audio and broadcast LE Audio use different ISO transports.

Check the documentation matching your installed versions. On real hardware,
verify one simultaneous SCO/eSCO stream per assigned radio, microphone audio
reaching every intended listener (and no other groups), profile handoff timing,
and ACL/A2DP stability under the maximum listener load. A linked PipeWire graph
and a confirmation beep establish routing state, not measured voice quality or
certified controller capacity.

Find event devices with `evtest` or `libinput debug-events`.
Use stable `/dev/input/by-id/` or `/dev/input/by-path/` paths where available.
The running user needs permission to read the devices, and the Bluetooth stack
must expose distinct play/pause press and release events. Missing or unreadable
devices stop PTT from starting; a device failure while running exits and cleans
up. PTT is not a privacy boundary: a failed mute can leave audio active until a
retry succeeds.

### Dashboard and user service

Add `--dashboard` to `run` for a live terminal view of the selected half- or
full-duplex mode, Bluetooth status, duplex availability, owned route counts and
optional RSSI. Because the dashboard runs inside `run`, it reflects that
process's mode directly without inter-process communication. It requires an
interactive terminal on stderr and is not intended for a service. Route counts
show observed, channel-compatible links between different configured headsets,
not measured speech or audio quality. Transmit confirmations use the same
route-readiness checks. Both terminal views show failed Bluetooth status queries
as unknown and report the polling error; a later successful poll clears it.

An example full-duplex user service is in `examples/bt-intercom.service`. Pair
and configure the headsets first, then install and enable it:

```sh
mkdir -p ~/.config/systemd/user
cp examples/bt-intercom.service ~/.config/systemd/user/
systemctl --user daemon-reload
systemctl --user enable --now bt-intercom.service
journalctl --user -u bt-intercom.service -f
```

The unit uses `run --connect` and saved addresses; it does not enable PTT. The
user's PipeWire/WirePlumber session must be available. To start before login,
enable lingering with `sudo loginctl enable-linger "$USER"` and configure
WirePlumber's Bluetooth seat policy for headless use. Stop the unit to release
its routes:

```sh
systemctl --user stop bt-intercom.service
```

## Limits and development

Bluetooth connection capacity and audio behavior depend on the adapter,
firmware, OS and headset; simultaneous HFP/HSP connections are not guaranteed.
The host needs no local microphone or speaker, but this is not a network
intercom. Profile switching is opt-in with `--transport sco-a2dp`; there is no
automatic bond migration, Auracast broadcast, echo cancellation or audio
processing. Headsets need acoustic isolation to avoid
feedback. Host tests use synthetic PipeWire graphs and do not verify Bluetooth
hardware or audio transport; test the actual devices, reconnects, PTT and
shutdown behavior before relying on a deployment.

Run the same checks as CI:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```
