# bt-intercom

A local Bluetooth Classic intercom with selectable semi-duplex and full-duplex
modes for two or more headsets on Linux. BlueZ manages Bluetooth; PipeWire and
WirePlumber provide headset audio. Each headset microphone routes to other
configured headsets in its talk groups, never back to itself. With no talk groups
configured, all headsets share the original all-to-all intercom. By default all
microphones are live; optional push-to-talk (PTT) keeps them muted until a
headset button is held. Semi-duplex queues talk requests and allows only one
headset microphone per talk group at a time.

[Watch the illustrative CLI demo](https://phieri.github.io/bt-intercom/).

## Requirements

- Linux with Bluetooth Classic, BlueZ (`bluetoothctl`), PipeWire (`pw-dump`,
  `pw-cli`, `pw-play`)
  and WirePlumber. Run as the same user as PipeWire, **not with `sudo`**.
- At least two Bluetooth Classic headsets that expose both microphone and
  speaker audio through the HFP/HSP `headset-head-unit` profile. A2DP alone is
  playback-only. Check profiles with `wpctl status`; select the duplex profile
  with `wpctl set-profile DEVICE_ID PROFILE_INDEX`. The program does not change
  profiles.

### Raspberry Pi hardware

Raspberry Pi Zero W, Zero 2 W, Pi 3, Pi 4 and Pi 5 have onboard Bluetooth
Classic radios. Pi 1, Pi 2 and the original Pi Zero need a compatible USB
adapter. The supported Linux artifact targets for these models are listed below.

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
level is `info`; set `RUST_LOG=warn` to show warnings only. A systemd user
service sends stdout and stderr to the journal.

The terminal control panel uses semantic colors when the terminal supports
them; it respects `NO_COLOR`, and uses a plain style when color is unavailable.
The live dashboard only clears the screen when its output is an interactive
terminal.

### Debian and RPM packages

`Cargo.toml` includes packaging metadata for the binary, man page, runtime
requirements, an AppArmor profile, and a systemd user unit. The packages install
the profile at `/etc/apparmor.d/usr.bin.bt-intercom`; AppArmor must be enabled
on the host for it to be enforced. The profile allows the default configuration
and runtime paths, PipeWire and BlueZ access, and read access to PTT input
devices. Custom `XDG_CONFIG_HOME` paths may need a local profile adjustment.
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
own. BlueZ stores pairing credentials; this program stores only headset
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
Remove a headset from the saved intercom network with `bt-intercom remove ADDRESS`.
This does not disconnect or unpair it; restart a running intercom or service
for the change to take effect. Removing the last address clears the saved
network.
Best-effort beeps confirm when an intercom route becomes active. Their temporary
WAV file is created under `XDG_RUNTIME_DIR` when it is an absolute path, falling
back to the system temporary directory otherwise; it is removed during shutdown.

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

Select `--mode full-duplex` (the default) or `--mode semi-duplex` on `run`.
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

For semi-duplex, add `--mode semi-duplex` to the command above. Button mappings
are required for every headset. Hold play/pause to request a turn: if another
headset is transmitting, your request waits in first-in, first-out order.
Keep holding while queued and wait for the double beep before talking.
Releasing the button cancels a queued request or ends your turn, allowing the
next waiting headset in that group to transmit. Each configured talk group has
an independent queue, so separate groups can have active talkers at the same
time. A headset in multiple groups requests a turn in each and is routed only
to groups where it currently holds the floor. With no talk groups configured,
all headsets share one queue. Triple presses do not enable always-open
microphones in semi-duplex.
Semi-duplex routing failures stop the run and release its owned links rather
than risk leaving the previous talker active. Existing external links remain
untouched, so exclusivity applies only to routes managed by this process.
When a mapped headset loses duplex audio, its request and always-open choice
are reset; after reconnecting, release and press again to talk.

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
