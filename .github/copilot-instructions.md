# Copilot instructions

These instructions are for coding agents working in `bt-intercom`. Keep them
focused on implementation and contribution guidance. The README is the source
of truth for user-facing setup, CLI behavior, supported hardware, and limits.
When a change affects those topics, update the README as well.

## Working in this repository

- Inspect the affected implementation and nearby tests before changing behavior.
- Make the smallest complete change that addresses the task; avoid unrelated
  cleanup.
- Add or update tests where practical, following the existing test patterns.
- Preserve cleanup on success, failure, cancellation, and shutdown paths.
- Keep errors actionable. Warnings should remain non-fatal when recovery is
  intended.

## Project structure

- `src/main.rs`: entry point and command dispatch; `src/cli.rs`: Clap
  definitions and parsers; `src/runtime_config.rs`: read-only resolution of
  `run` options before persistence or workers start.
- `src/session.rs`, `src/session/`: session lifecycle, event loop, route
  coordination (`routing.rs`) and bounded reconnection (`reconnect.rs`).
- `src/process.rs`: bounded subprocess execution, cancellation, and process
  cleanup; `src/worker.rs`: owned workers with cancellation and joined teardown.
- `src/bluez.rs`: parsing of `bluetoothctl` output shared by the CLI and
  dashboard; `src/device_status.rs`: shared status sampling.
- `src/network.rs`: persistence and validation for the headset network.
- `src/pipewire.rs`: typed PipeWire topology; `src/route_policy.rs`: pure route
  selection; `src/router.rs`: discovery and owned managed links;
  `src/routing_status.rs`: metrics from observed links.
- `src/transmit.rs`: transmit state and half-duplex floor/queue behavior.
- `src/transport.rs`, `src/transport_model.rs`, `src/transport/profile_plan.rs`:
  `sco-a2dp` profile transitions (pure planning, then verified mutation).
- `src/host_ptt.rs`, `src/evdev.rs`: optional host spacebar PTT and shared evdev
  record framing.
- `src/ptt.rs`, `src/ptt_discovery.rs`: evdev PTT readers and automatic
  matching of each headset's play/pause device.
- `src/diagnostics.rs`, `src/diagnostics/probe.rs`: passive and active hardware
  diagnosis (`diagnose`).
- `src/dashboard.rs`, `src/terminal_style.rs`: terminal status view and styling.
- `src/atomic_file.rs`: atomic replacement for persisted files.
- `man/`, `packaging/`, `examples/`: man page and systemd
  units. Update the man page alongside README changes to CLI behavior.

Unit tests are colocated with implementation in `#[cfg(test)]` modules.
Prefer the existing synthetic PipeWire fixtures and injected command executors
over requiring Bluetooth hardware or a live PipeWire session.

## Behavioral invariants

- Route only allowlisted Bluetooth devices exposing both source and sink ports
  with the `headset-head-unit` profile. Never route a headset to itself or
  modify links not owned by this process.
- Each created PipeWire link belongs to a monitored `pw-cli -m` client and uses
  `object.linger=false`. Release the link by closing its owning client; do not
  delete links or ports by numeric ID because PipeWire may reuse IDs.
- Run subprocesses through the bounded and cancellable mechanisms in
  `src/process.rs`. Preserve process-group cleanup for cancellable commands;
  interactive children that share the caller's terminal have different
  job-control constraints.
- PTT is driven by mapped Linux evdev devices and `KEY_PLAYPAUSE`, `KEY_PLAYCD` and
  `KEY_PAUSECD` (BlueZ AVRCP) press/release events only. Headset call/answer keys must not activate it. Only the opt-in `--host` reader (`src/host_ptt.rs`, `src/evdev.rs`) may use keyboard `KEY_SPACE`, as hold-to-talk for the routing computer's wired headset.
- With `--transport sco-a2dp`, change profiles only per headset after
  re-verifying its PipeWire identity, respect per-controller `--sco-limit` and
  `--a2dp-limit`, and never assume capacity when controller identity is missing.
- Routing is all-to-all among allowlisted headsets; half-duplex uses one shared
  floor queue.
- Use `crate::atomic_file::write` when replacing persisted configuration files.

## Validation

Run the same host checks as CI:

```sh
cargo fmt --check
cargo clippy --all-targets --locked -- -D warnings
cargo test --locked
```

These checks do not validate real Bluetooth connections or audio transport.
Changes affecting headset or audio behavior also need hardware validation where
available.

## Runtime and build

The application invokes `bluetoothctl`, `pw-dump`, `pw-cli`, and `pw-play`.
It must run as the same user as PipeWire, not under `sudo`. Host tests need
neither these utilities nor live devices.

CI cross-builds `arm-unknown-linux-gnueabihf` (ARMv6),
`armv7-unknown-linux-gnueabihf`, and `aarch64-unknown-linux-gnu`. For ARMv6,
use `cross` version 0.2.5; Ubuntu's ARMhf linker defaults to ARMv7 and cannot
replace the ARMv6 target. Choose the target for both the device and OS bitness
as documented in the README.

## Documentation and workflows

The build workflow runs formatting, Clippy, tests, and cross-compilation. The
Pages workflow generates the accessible transcript from `docs/demo.cast`,
downloads pinned asciinema-player assets, verifies their checksums, and deploys
`docs/` from `main` or by manual dispatch. Generated assets in `docs/vendor/`
are ignored; do not commit them.
