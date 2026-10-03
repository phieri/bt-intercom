# Copilot instructions

## Project overview

This is the bt-intercom Rust CLI for a local Bluetooth Classic intercom with
selectable half-duplex and full-duplex modes on Linux; Raspberry Pi is one
supported platform. BlueZ handles pairing and PipeWire/WirePlumber provides
HFP/HSP headset audio. The README is the source of truth for setup, supported
hardware, and operational limitations.

## Code layout

- `src/main.rs` implements the CLI, Bluetooth commands, subprocess management,
  reconnect behavior, push-to-talk, and the main run loop.
- `src/router.rs` discovers PipeWire topology and manages inter-headset links.
- `src/dashboard.rs` polls Bluetooth status and renders the optional terminal
  dashboard.
- Unit tests live alongside their implementation in `#[cfg(test)]` modules.

## Development checks

Run the same host checks as CI:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

Tests use synthetic PipeWire graphs and mocked command behavior; they do not
require Bluetooth hardware or a live PipeWire session. Add tests
near the code they cover, using the existing fixtures and injected command
executors where practical. These checks do not replace hardware validation for
changes affecting actual headset/audio behavior.

## Important implementation constraints

- Routing is limited to allowlisted Bluetooth devices exposing both source and
  sink ports with the `headset-head-unit` profile. Never route a headset to
  itself or alter links not owned by this process.
- Each created PipeWire link is owned by a monitored `pw-cli -m` client with
  `object.linger=false`. Release links by closing their client; do not delete
  links or ports by numeric ID, since PipeWire may reuse IDs.
- External commands have timeouts and cancellation/cleanup behavior. Preserve
  process-group cleanup for cancellable commands and bounded execution when
  adding subprocess calls.
- PTT uses mapped Linux evdev devices and only `KEY_PLAYPAUSE` press/release
  events. Keep keyboard input and headset call/answer keys from activating it.
- User-facing failures are generally returned as `Result<_, String>`; warnings
  are non-fatal and logged separately. Keep failures actionable and preserve
  cleanup on errors.

## Build targets and runtime

The CI cross-build matrix is `arm-unknown-linux-gnueabihf` (ARMv6),
`armv7-unknown-linux-gnueabihf`, and `aarch64-unknown-linux-gnu`. For Pi Zero W
and other ARMv6 devices, use the ARMv6 target via `cross` version 0.2.5; the
Ubuntu ARMhf linker defaults to ARMv7 and is not a substitute. Match the target
to both the Pi model and OS bitness as documented in the README.

At runtime the application invokes `bluetoothctl`, `pw-dump`, `pw-cli`, and
`pw-play`. It must run in the same user session as PipeWire, not under `sudo`.
Host tests do not need these utilities or access to live devices.

## Documentation and workflows

The build workflow runs formatting, Clippy, tests, and cross-compilation. Its
path filters skip changes limited to `docs/`, `README.md`, and the Pages
workflow. The Pages workflow generates the accessible transcript from
`docs/demo.cast`, downloads pinned asciinema-player assets, verifies their
checksums, and deploys `docs/` from `main` (or by manual dispatch). Generated
assets under `docs/vendor/` are ignored; do not commit them.
