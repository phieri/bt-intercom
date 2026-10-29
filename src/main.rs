// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Command-line entry point for Bluetooth setup, status, and intercom routing.
//!
//! External commands are bounded by timeouts; the long-running `run` mode also
//! supports cancellation, reconnecting, push-to-talk input, and a terminal view.

mod atomic_file;
mod bluez;
mod cli;
mod dashboard;
mod device_status;
mod group_editor;
mod groups;
mod network;
mod pair_button;
mod pipewire;
mod process;
mod ptt;
mod ptt_discovery;
mod route_policy;
mod router;
mod routing_status;
mod runtime_config;
mod session;
mod terminal_style;
mod transmit;
mod transport;
mod transport_model;
mod worker;

mod tui;

use std::collections::BTreeSet;
use std::io::{self, IsTerminal};
use std::process::{Command, Stdio};
#[cfg(test)]
use std::sync::atomic::{AtomicBool, Ordering};
#[cfg(test)]
use std::sync::mpsc::{self, TryRecvError};
#[cfg(test)]
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::thread;
use std::time::Duration;
#[cfg(test)]
use std::time::Instant;

#[cfg(test)]
use bluez::parse_address;
use bluez::{Address, bluetooth_name, device_flag};
use clap::FromArgMatches;
use cli::{Cli, CliCommand, cli_command};
use groups::config_path as talk_groups_path;
use network::{
	headset_path as headset_network_path, load as load_headsets, remove as remove_headset,
};
use process::{COMMAND_TIMEOUT, command};
#[cfg(test)]
use ptt::KEY_PLAYPAUSE;
use router::Router;
#[cfg(test)]
use transmit::Mode;
#[cfg(test)]
use transmit::Transmit;
#[cfg(test)]
use transport::Transport;

/// Opens an interactive BlueZ agent session to pair the specified device.
fn pair_command(device: &str) -> Result<(), String> {
	if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
		return Err("pair requires an interactive terminal; run it from a terminal session".into());
	}
	eprintln!("At the bluetoothctl prompt, enter: pair {device}");
	eprintln!("Follow any PIN/confirmation prompts; when pairing finishes, enter: quit");
	let mut child = Command::new("bluetoothctl")
		.args(["--agent", "KeyboardDisplay"])
		.stdin(Stdio::inherit())
		.stdout(Stdio::inherit())
		.stderr(Stdio::inherit())
		.spawn()
		.map_err(|error| format!("bluetoothctl: {error}"))?;
	let status =
		process::wait_interactive(&mut child, "bluetoothctl", Duration::from_secs(300), None)?;
	if status.success() {
		Ok(())
	} else {
		Err(format!("bluetoothctl pair exited with {status}"))
	}
}

/// Scans for devices, powering on the adapter first when it is off.
fn scan_command(seconds: u64) -> Result<String, String> {
	if let Ok(info) = command(&["bluetoothctl", "show"], COMMAND_TIMEOUT)
		&& !info.contains("No default controller")
		&& !device_flag(&info, "Powered")
	{
		{
			eprintln!("Bluetooth adapter is powered off; powering it on");
			command(&["bluetoothctl", "power", "on"], COMMAND_TIMEOUT)
				.map_err(|error| format!("failed to power on the Bluetooth adapter: {error}"))?;
		}
	}
	command(
		&["bluetoothctl", "--timeout", &seconds.to_string(), "scan", "on"],
		Duration::from_secs(seconds + 5),
	)
	.map_err(|error| {
		if error.contains("NotReady") {
			format!(
				"{error}\nThe Bluetooth adapter is not ready. Check `rfkill list` and run `sudo rfkill unblock bluetooth`, then `bluetoothctl power on`; on a Raspberry Pi also confirm bluetooth.service is running and the UART/firmware (hciuart) has finished initializing"
			)
		} else {
			error
		}
	})
}

/// Parses and validates one or more Bluetooth addresses.
fn addresses(args: &[Address]) -> Result<BTreeSet<Address>, String> {
	if args.is_empty() {
		return Err("at least one Bluetooth address is required".into());
	}
	Ok(args.iter().cloned().collect())
}

/// Executes one CLI action, including the main polling and routing loop.
fn run(action: CliCommand) -> Result<(), String> {
	match action {
		CliCommand::Panel => {
			let network_path = headset_network_path()?;
			let allowed = load_headsets(&network_path)?;
			tui::run(allowed, talk_groups_path(&network_path)?)?;
		}
		CliCommand::Scan { seconds } => {
			print!("{}", scan_command(seconds)?);
		}
		CliCommand::Pair { address: device } => {
			pair_command(&device)?;
			if !device_flag(
				&command(&["bluetoothctl", "info", &device], COMMAND_TIMEOUT)?,
				"Paired",
			) {
				return Err(format!("pairing did not succeed for {device}"));
			}
			for action in ["trust", "connect"] {
				print!(
					"{}",
					command(
						&["bluetoothctl", "--timeout", "60", action, &device],
						Duration::from_secs(65)
					)?
				);
			}
		}
		CliCommand::Remove { address: device } => {
			let network_path = headset_network_path()?;
			remove_headset(&network_path, &device)?;
			println!(
				"Removed {device} from the saved intercom network. Bluetooth pairing is unchanged; restart any running intercom to apply the change."
			);
		}
		CliCommand::Status { addresses: devices } => {
			let allowed = addresses(&devices)?;
			let mut router = Router::new(allowed);
			let (headsets, _) = router.inspect()?;
			let transport_devices = router.transport_devices()?;
			for address in &router.allowed {
				let info = command(&["bluetoothctl", "info", address], COMMAND_TIMEOUT);
				let (identity, bluetooth) = bluetooth_summary(address, &info);
				println!("{identity}: Bluetooth {bluetooth}");
				match headsets.get(address) {
					Some(headset) => println!(
						"  PipeWire: {} ({} microphone ports, {} speaker ports)",
						if headset.has_duplex_audio() {
							"duplex ready"
						} else {
							"duplex unavailable"
						},
						headset.sources.len(),
						headset.sinks.len()
					),
					None => println!("  PipeWire: headset not found"),
				}
				if let Some(device) = transport_devices.get(address) {
					println!(
						"  controller: {}; HFP: {}; A2DP: {}",
						device.controller, device.hfp_available, device.a2dp_available
					);
				}
			}
			for warning in transport::controller_warnings(&transport_devices, 1) {
				log::warn!("{warning}");
			}
		}
		CliCommand::Run {
			addresses: devices,
			interval,
			connect,
			sco_limit,
			pair_button,
			ptt: buttons,
			mode,
			transport,
			dashboard: show_dashboard,
		} => {
			let config = runtime_config::RunConfig::resolve(
				runtime_config::RunOptions {
					addresses: devices,
					interval,
					connect,
					sco_limit,
					pair_button,
					buttons,
					mode,
					transport,
					dashboard: show_dashboard,
				},
				std::io::stderr().is_terminal(),
			)?;
			session::Session::start(config)?.run()?;
		}
		CliCommand::Completions { .. } => unreachable!("completions are handled before run"),
	}
	Ok(())
}

fn bluetooth_summary(address: &Address, info: &Result<String, String>) -> (String, String) {
	match info {
		Ok(info) => {
			let identity = bluetooth_name(info)
				.map_or_else(|| address.to_string(), |name| format!("{name} ({address})"));
			let connected = match bluez::property(info, "Connected") {
				Some("yes") => "connected",
				Some("no") => "disconnected",
				_ => "unknown (BlueZ did not report connection state)",
			};
			(identity, connected.to_string())
		}
		Err(error) => {
			let error = error
				.chars()
				.filter(|character| !character.is_control())
				.collect::<String>();
			(address.to_string(), format!("status query failed: {error}"))
		}
	}
}

/// Reports command-line errors and exits unsuccessfully.
fn main() {
	let matches = cli_command().get_matches();
	let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
	env_logger::Builder::from_env(
		env_logger::Env::default().default_filter_or(if cli.verbose { "debug" } else { "info" }),
	)
	.format_timestamp_secs()
	.init();
	match cli.command {
		CliCommand::Completions { shell } => {
			let mut command = cli_command();
			clap_complete::generate(
				shell,
				&mut command,
				"bt-intercom",
				&mut std::io::stdout().lock(),
			);
		}
		command => {
			if let Err(error) = run(command) {
				eprintln!("bt-intercom: {error}");
				if error.contains("pw-") && error.contains("can't connect") {
					// SAFETY: geteuid has no preconditions and cannot fail.
					let root = unsafe { libc::geteuid() } == 0;
					if root || std::env::var_os("SUDO_USER").is_some() {
						eprintln!(
							"hint: PipeWire runs per user; do not run bt-intercom with sudo or as root. Run it as the desktop/login user that owns the PipeWire session"
						);
					} else {
						eprintln!(
							"hint: cannot reach PipeWire; check `systemctl --user status pipewire wireplumber`. Over SSH or at boot, enable lingering with `sudo loginctl enable-linger \"$USER\"` and set XDG_RUNTIME_DIR=/run/user/$(id -u)"
						);
					}
				}
				std::process::exit(1);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::ptt::PttButton;
	use crate::ptt::{BEEP_WAV, confirm_transmissions, drain_events, read_input_events};
	use crate::runtime_config::{validate_mode, validate_ptt};
	use crate::session::{connect_disconnected, guard_half_update, reconnect_worker};
	use crate::transport::{
		refresh_transmit_requests as refresh_transport_requests, validate_controller_count,
		validate_transport,
	};
	use clap::{CommandFactory, Parser};

	fn address(value: &str) -> Address {
		Address::parse(value).unwrap()
	}

	mod status {
		use super::*;

		#[test]
		fn status_summary_distinguishes_connection_state_from_query_failure() {
			let address = address("AA:BB:CC:DD:EE:01");
			assert_eq!(
				bluetooth_summary(
					&address,
					&Ok("Alias: Dispatch headset\nConnected: yes\n".into())
				),
				(
					"Dispatch headset (AA:BB:CC:DD:EE:01)".into(),
					"connected".into()
				)
			);
			assert_eq!(
				bluetooth_summary(
					&address,
					&Ok("Alias: Dispatch headset\nConnected: no\n".into())
				)
				.1,
				"disconnected"
			);
			assert_eq!(
				bluetooth_summary(&address, &Ok("Alias: Dispatch headset\n".into())).1,
				"unknown (BlueZ did not report connection state)"
			);
			assert_eq!(
				bluetooth_summary(&address, &Err("bluetoothctl unavailable\n".into())).1,
				"status query failed: bluetoothctl unavailable"
			);
		}
	}

	mod transport_reconnect {
		use super::*;

		#[test]
		fn transport_disappearance_then_reconnect_requires_a_fresh_ptt_press() {
			let address = address("AA:BB:CC:DD:EE:01");
			for disappear_at in [2, 3] {
				let current = if disappear_at == 3 { 41 } else { 17 };
				let connected = serde_json::json!([{
					"type":"PipeWire:Interface:Device", "id":10,
					"info":{
						"props":{
							"api.bluez5.address":address.to_string(),
							"api.bluez5.path":"/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01",
							"object.serial":1010
						},
						"params":{
							"EnumProfile":[
								{"index":17,"name":"headset-head-unit","available":"yes"},
								{"index":41,"name":"a2dp-sink","available":"yes"}
							],
							"Profile":[{"index":current}]
						}
					}
				}]);
				let mut snapshots = 0;
				let mut router =
					Router::with_executor([address.clone()].into(), |args: &[&str]| {
						assert_eq!(args, ["pw-dump"]);
						snapshots += 1;
						Ok(if snapshots == disappear_at {
							"[]".into()
						} else {
							connected.to_string()
						})
					});
				router.set_transport(Transport::ScoA2dp);
				let mut transmit = Transmit::new(Mode::HalfDuplex, [address.clone()]);
				let before = router.transport_devices().unwrap();
				refresh_transport_requests(&mut transmit, &before, &BTreeSet::new());
				let at = Instant::now();
				assert!(transmit.event(&address, true, at));
				assert_eq!(transmit.sources(), &[address.clone()].into());

				// Exercise disappearance in both prepare's initial snapshot and
				// the identity snapshot immediately before profile promotion.
				assert!(!router.prepare_transport(transmit.sources()).unwrap());
				assert!(!router.prepare_transport(transmit.sources()).unwrap());
				let reconnected = router.transport_devices().unwrap();
				assert!(reconnected.contains_key(&address));
				let disconnected = router.take_transport_disconnects();
				assert_eq!(disconnected, [address.clone()].into());
				assert!(router.take_transport_disconnects().is_empty());
				assert!(refresh_transport_requests(
					&mut transmit,
					&reconnected,
					&disconnected
				));
				assert!(transmit.sources().is_empty());
				assert!(transmit.pending.is_empty());
				assert!(!refresh_transport_requests(
					&mut transmit,
					&reconnected,
					&BTreeSet::new()
				));
				assert!(!transmit.event(&address, true, at + Duration::from_millis(1)));
				assert!(transmit.event(&address, false, at + Duration::from_millis(2)));
				assert!(transmit.event(&address, true, at + Duration::from_millis(3)));
				assert_eq!(transmit.sources(), &[address.clone()].into());
			}
		}

		#[test]
		fn queued_transport_disappearance_is_retired_even_after_reconnection() {
			let a = address("AA:BB:CC:DD:EE:01");
			let b = address("AA:BB:CC:DD:EE:02");
			for disappear_at in [2, 3] {
				let device = |id: u64, address: &str, current: u64| {
					serde_json::json!({
						"type":"PipeWire:Interface:Device", "id":id,
						"info":{
							"props":{
								"api.bluez5.address":address,
								"api.bluez5.path":format!("/org/bluez/hci0/dev_{}", address.replace(':', "_")),
								"object.serial":id + 1000
							},
							"params":{
								"EnumProfile":[
									{"index":17,"name":"headset-head-unit","available":"yes"},
									{"index":41,"name":"a2dp-sink","available":"yes"}
								],
								"Profile":[{"index":current}]
							}
						}
					})
				};
				let source = device(10, &a, if disappear_at == 3 { 41 } else { 17 });
				let connected = serde_json::json!([source.clone(), device(20, &b, 41)]);
				let absent = serde_json::json!([source]);
				let mut snapshots = 0;
				let mut router =
					Router::with_executor([a.clone(), b.clone()].into(), |args: &[&str]| {
						if args[0] == "pw-dump" {
							snapshots += 1;
							Ok(if snapshots == disappear_at {
								absent.to_string()
							} else {
								connected.to_string()
							})
						} else {
							assert_eq!(&args[..2], ["pw-cli", "set-param"]);
							Ok(String::new())
						}
					});
				router.set_transport(Transport::ScoA2dp);
				let mut transmit = Transmit::new(Mode::HalfDuplex, [a.clone(), b.clone()]);
				refresh_transport_requests(
					&mut transmit,
					&router.transport_devices().unwrap(),
					&BTreeSet::new(),
				);
				let at = Instant::now();
				transmit.event(&a, true, at);
				transmit.event(&b, true, at + Duration::from_millis(1));
				assert_eq!(transmit.sources(), &[a.clone()].into());
				assert!(!router.prepare_transport(transmit.sources()).unwrap());
				let reconnected = router.transport_devices().unwrap();
				assert!(reconnected.contains_key(&b));
				let disconnected = router.take_transport_disconnects();
				assert_eq!(disconnected, [b.clone()].into());
				assert!(!refresh_transport_requests(
					&mut transmit,
					&reconnected,
					&disconnected
				));
				transmit.event(&a, false, at + Duration::from_millis(2));
				assert!(transmit.sources().is_empty());
				assert!(!transmit.event(&b, true, at + Duration::from_millis(3)));
				transmit.event(&b, false, at + Duration::from_millis(4));
				transmit.event(&b, true, at + Duration::from_millis(5));
				assert_eq!(transmit.sources(), &[b.clone()].into());
			}
		}
	}

	mod cli_validation {
		use super::*;

		#[test]
		fn validates_address_before_invoking_commands() {
			assert!(parse_address("invalid; rm -rf /").is_err());
			assert_eq!(
				parse_address("aa:bb:cc:dd:ee:ff").unwrap().as_str(),
				"AA:BB:CC:DD:EE:FF"
			);
		}

		#[test]
		fn mixed_transport_is_opt_in_and_requires_half_duplex() {
			let CliCommand::Run { transport, .. } =
				Cli::try_parse_from(["bt-intercom", "run"]).unwrap().command
			else {
				panic!("expected run");
			};
			assert_eq!(transport, Transport::Hfp);
			let CliCommand::Run {
				transport, mode, ..
			} = Cli::try_parse_from([
				"bt-intercom",
				"run",
				"--transport",
				"sco-a2dp",
				"--mode",
				"half-duplex",
			])
			.unwrap()
			.command
			else {
				panic!("expected run");
			};
			assert!(validate_transport(mode, transport).is_ok());
			assert!(validate_transport(Mode::FullDuplex, transport).is_err());
			assert!(validate_transport(Mode::HalfDuplex, Transport::Hfp).is_ok());
			assert!(
				Cli::try_parse_from(["bt-intercom", "run", "--transport", "auracast"]).is_err()
			);
		}

		#[test]
		fn sco_limit_defaults_to_one_and_accepts_positive_overrides() {
			let CliCommand::Run { sco_limit, .. } =
				Cli::try_parse_from(["bt-intercom", "run"]).unwrap().command
			else {
				panic!("expected run");
			};
			assert_eq!(sco_limit, 1);
			let CliCommand::Run { sco_limit, .. } =
				Cli::try_parse_from(["bt-intercom", "run", "--sco-limit", "3"])
					.unwrap()
					.command
			else {
				panic!("expected run");
			};
			assert_eq!(sco_limit, 3);
			assert!(Cli::try_parse_from(["bt-intercom", "run", "--sco-limit", "0"]).is_err());
			assert!(validate_controller_count(Mode::FullDuplex, 1, 1).is_err());
			assert!(validate_controller_count(Mode::FullDuplex, 1, 2).is_ok());
			assert!(validate_controller_count(Mode::FullDuplex, 2, 1).is_ok());
			assert!(validate_controller_count(Mode::HalfDuplex, 1, 1).is_ok());
		}

		#[test]
		fn validates_address_lists_and_rejects_mixed_invalid_input() {
			let input = [
				"aa:bb:cc:dd:ee:01".into(),
				"AA:BB:CC:DD:EE:02".into(),
				"aa:bb:cc:dd:ee:01".into(),
			];
			assert_eq!(
				addresses(&input).unwrap(),
				BTreeSet::from([address("AA:BB:CC:DD:EE:01"), address("AA:BB:CC:DD:EE:02"),])
			);
			assert!(addresses(&[]).is_err());
			assert!(parse_address("invalid").is_err());
		}

		#[test]
		fn pairing_button_is_opt_in_and_incompatible_with_ptt() {
			let CliCommand::Run { pair_button, .. } =
				Cli::try_parse_from(["bt-intercom", "run"]).unwrap().command
			else {
				panic!("expected run");
			};
			assert!(!pair_button);
			let CliCommand::Run { pair_button, .. } =
				Cli::try_parse_from(["bt-intercom", "run", "--pair-button"])
					.unwrap()
					.command
			else {
				panic!("expected run");
			};
			assert!(pair_button);
			assert!(
				Cli::try_parse_from([
					"bt-intercom",
					"run",
					"--pair-button",
					"--ptt",
					"AA:BB:CC:DD:EE:01=/dev/input/event4",
				])
				.is_err()
			);
			assert!(validate_mode(Mode::HalfDuplex, &[], &BTreeSet::new()).is_err());
		}

		#[test]
		fn connects_only_when_disconnected_or_info_fails() {
			let device = "AA:BB:CC:DD:EE:01";
			let allowed = BTreeSet::from([address(device)]);
			for (info, should_connect) in [
				(Ok("Connected: yes\n"), false),
				(Ok("Connected: no\n"), true),
				(Err("bluetoothctl info failed"), true),
			] {
				let calls = Mutex::new(Vec::new());
				connect_disconnected(&allowed, &AtomicBool::new(false), |args, timeout| {
					calls.lock().unwrap().push((
						args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
						timeout,
					));
					if args[1] == "info" {
						info.map(str::to_string).map_err(str::to_string)
					} else {
						Ok(String::new())
					}
				});
				let mut expected = vec![(
					vec![
						"bluetoothctl".to_string(),
						"info".to_string(),
						device.to_string(),
					],
					COMMAND_TIMEOUT,
				)];
				if should_connect {
					expected.push((
						vec![
							"bluetoothctl".to_string(),
							"--timeout".to_string(),
							"30".to_string(),
							"connect".to_string(),
							device.to_string(),
						],
						Duration::from_secs(35),
					));
				}
				assert_eq!(*calls.lock().unwrap(), expected);
			}
		}

		#[test]
		fn mode_parsing_validation_and_documented_window() {
			for (name, expected) in [
				("half-duplex", Mode::HalfDuplex),
				("full-duplex", Mode::FullDuplex),
			] {
				let cli = Cli::try_parse_from(["bt-intercom", "run", "--mode", name]).unwrap();
				let CliCommand::Run { mode, .. } = cli.command else {
					panic!("expected run");
				};
				assert_eq!(mode, expected);
			}
			assert!(Cli::try_parse_from(["bt-intercom", "run", "--mode", "semi-duplex"]).is_err());
			assert!(Cli::try_parse_from(["bt-intercom", "run", "--mode", "half"]).is_err());
			let allowed = BTreeSet::from(["AA:BB:CC:DD:EE:01".into()]);
			assert!(validate_mode(Mode::FullDuplex, &[], &allowed).is_ok());
			assert!(validate_mode(Mode::HalfDuplex, &[], &allowed).is_err());
			assert!(validate_mode(Mode::HalfDuplex, &[], &BTreeSet::new()).is_err());
			let buttons = [PttButton {
				address: Some(address("AA:BB:CC:DD:EE:01")),
				path: "/dev/input/event1".into(),
			}];
			assert!(validate_mode(Mode::HalfDuplex, &buttons, &allowed).is_ok());
			let two = BTreeSet::from(["AA:BB:CC:DD:EE:01".into(), "AA:BB:CC:DD:EE:02".into()]);
			assert!(validate_mode(Mode::HalfDuplex, &buttons, &two).is_err());
			let single_controller_error =
				validate_controller_count(Mode::FullDuplex, 1, 1).unwrap_err();
			assert!(single_controller_error.contains("at least two SCO/eSCO slots"));
			assert!(single_controller_error.contains("--mode half-duplex"));
			assert!(validate_controller_count(Mode::FullDuplex, 2, 1).is_ok());
			assert!(validate_controller_count(Mode::HalfDuplex, 1, 1).is_ok());
			let mut command = Cli::command();
			let help = command
				.find_subcommand_mut("run")
				.unwrap()
				.render_long_help()
				.to_string();
			assert!(help.contains("1 second"));
			assert!(help.contains("first to third, inclusive"));
		}
	}

	mod routing_confirmation {
		use super::*;

		#[test]
		fn half_update_failure_closes_owned_clients_before_returning_fatal_error() {
			let mut owned_clients_closed = false;
			let update: Result<(), String> = Err("pw-dump timed out".into());
			let result = guard_half_update(Mode::HalfDuplex, &update, || {
				owned_clients_closed = true;
			});
			assert!(owned_clients_closed);
			let error = result.unwrap_err();
			assert!(error.contains("released owned links"));
			assert!(error.contains("unsafe floor handoff"));
			assert!(error.contains("pw-dump timed out"));
		}

		#[test]
		fn full_update_failure_remains_nonfatal_and_success_keeps_owned_clients() {
			for (mode, update) in [
				(Mode::FullDuplex, Err("snapshot unavailable".into())),
				(Mode::FullDuplex, Ok(())),
				(Mode::HalfDuplex, Ok(())),
			] {
				assert!(
					guard_half_update(mode, &update, || {
						panic!(
							"must not close clients for a successful update or full-duplex warning"
						)
					})
					.is_ok()
				);
			}
		}

		#[test]
		fn confirmations_require_a_granted_source_and_owned_live_route() {
			use serde_json::json;
			let a = address("AA:BB:CC:DD:EE:01");
			let b = address("AA:BB:CC:DD:EE:02");
			let mut objects = Vec::new();
			for (base, address) in [(10, a.clone()), (20, b.clone())] {
				objects.extend([
				json!({"type":"PipeWire:Interface:Device","id":base,"info":{"props":{"api.bluez5.address":address.to_string()}}}),
				json!({"type":"PipeWire:Interface:Node","id":base+1,"info":{"props":{"device.id":base.to_string(),"media.class":"Audio/Source","api.bluez5.profile":"headset-head-unit"}}}),
				json!({"type":"PipeWire:Interface:Node","id":base+2,"info":{"props":{"device.id":base.to_string(),"media.class":"Audio/Sink","api.bluez5.profile":"headset-head-unit"}}}),
				json!({"type":"PipeWire:Interface:Port","id":base+3,"info":{"props":{"node.id":(base+1).to_string(),"port.direction":"out","audio.channel":"MONO"}}}),
				json!({"type":"PipeWire:Interface:Port","id":base+4,"info":{"props":{"node.id":(base+2).to_string(),"port.direction":"in","audio.channel":"MONO"}}}),
			]);
			}
			let allowed = BTreeSet::from([a.clone(), b.clone()]);
			let (headsets, _) = router::topology(&json!(objects), &allowed).unwrap();
			let stopped = AtomicBool::new(false);
			let mut transmit = Transmit::new(Mode::HalfDuplex, allowed);
			let now = Instant::now();
			transmit.event(&a, true, now);
			transmit.event(&b, true, now);
			let (sender, input) = mpsc::channel();
			// A queued microphone's live route (or an incoming-only route to the
			// floor holder) must not earn a confirmation.
			confirm_transmissions(
				Mode::HalfDuplex,
				&mut transmit,
				&input,
				&headsets,
				&BTreeSet::from([(23, 14)]),
				None,
				&stopped,
			)
			.unwrap();
			assert_eq!(transmit.pending, BTreeSet::from([a.clone()]));
			confirm_transmissions(
				Mode::HalfDuplex,
				&mut transmit,
				&input,
				&headsets,
				&BTreeSet::new(),
				None,
				&stopped,
			)
			.unwrap();
			assert_eq!(transmit.pending, BTreeSet::from([a.clone()]));
			assert_eq!(
			confirm_transmissions(
				Mode::HalfDuplex,
				&mut transmit,
				&input,
				&headsets,
				&BTreeSet::from([(13, 24)]),
				None,
				&stopped,
			),
			Err("could not play PTT confirmation for AA:BB:CC:DD:EE:01: confirmation beep is unavailable".into())
		);
			assert_eq!(transmit.pending, BTreeSet::from([a.clone()]));
			// Release arriving during routing retires the old grant and defers
			// confirmation of the next source until its own routing update.
			sender
				.send(Ok((a.clone(), false, now + Duration::from_millis(1))))
				.unwrap();
			transmit.pending.insert(a.clone());
			assert!(
				confirm_transmissions(
					Mode::HalfDuplex,
					&mut transmit,
					&input,
					&headsets,
					&BTreeSet::from([(13, 24)]),
					None,
					&stopped
				)
				.unwrap()
			);
			assert_eq!(transmit.sources(), &BTreeSet::from([b.clone()]));
			assert_eq!(transmit.pending, BTreeSet::from([b]));
		}
	}

	mod ptt_input {
		use super::*;

		#[test]
		fn draining_delayed_input_uses_capture_timestamps_not_processing_time() {
			let input_address = address("AA:BB:CC:DD:EE:01");
			let mut transmit = Transmit::new(Mode::FullDuplex, [input_address.clone()]);
			let now = Instant::now();
			let (sender, input) = mpsc::channel();
			for ms in [0, 500, 1001] {
				sender
					.send(Ok((
						input_address.clone(),
						true,
						now + Duration::from_millis(ms),
					)))
					.unwrap();
				sender
					.send(Ok((
						input_address.clone(),
						false,
						now + Duration::from_millis(ms + 1),
					)))
					.unwrap();
			}
			assert!(drain_events(&input, &mut transmit).unwrap());
			assert!(transmit.sources().is_empty());
			assert!(transmit.pending.is_empty());
			assert!(!drain_events(&input, &mut transmit).unwrap());
			sender.send(Err("lost input".into())).unwrap();
			assert_eq!(
				drain_events(&input, &mut transmit),
				Err("lost input".into())
			);
		}

		#[test]
		fn validates_ptt_bindings_against_restored_network() {
			let first = address("AA:BB:CC:DD:EE:01");
			let second = address("AA:BB:CC:DD:EE:02");
			let network = BTreeSet::from([first.clone(), second.clone()]);
			let buttons = vec![
				PttButton {
					address: Some(first.clone()),
					path: "/dev/input/event4".into(),
				},
				PttButton {
					address: Some(second.clone()),
					path: "/dev/input/event5".into(),
				},
			];
			assert!(validate_ptt(&buttons, &network).is_ok());
			assert!(validate_ptt(&buttons[..1], &network).is_err());
			let duplicate_path = vec![
				buttons[0].clone(),
				PttButton {
					address: Some(second.clone()),
					path: "/dev/input/event4".into(),
				},
			];
			assert!(validate_ptt(&duplicate_path, &network).is_err());
			let duplicate_address = vec![
				buttons[0].clone(),
				PttButton {
					address: Some(first.clone()),
					path: "/dev/input/event5".into(),
				},
			];
			assert!(validate_ptt(&duplicate_address, &network).is_err());
			assert!(validate_ptt(&[buttons[0].clone()], &BTreeSet::from([first, second])).is_err());
		}

		#[test]
		fn creates_a_short_double_beep_waveform() {
			let wav = BEEP_WAV;
			assert_eq!(&wav[..4], b"RIFF");
			assert_eq!(
				u32::from_le_bytes(wav[4..8].try_into().unwrap()),
				wav.len() as u32 - 8
			);
			assert_eq!(&wav[8..12], b"WAVE");
			assert_eq!(u32::from_le_bytes(wav[24..28].try_into().unwrap()), 48_000);
			assert_eq!(&wav[36..40], b"data");
			assert_eq!(
				u32::from_le_bytes(wav[40..44].try_into().unwrap()),
				48_000_u32 * 255 / 1_000 * 2
			);
		}

		#[test]
		fn ptt_input_only_follows_matching_button_press_and_release() {
			fn event(kind: u16, key: u16, value: i32) -> Vec<u8> {
				let mut bytes = vec![0; std::mem::size_of::<libc::timeval>()];
				bytes.extend(kind.to_ne_bytes());
				bytes.extend(key.to_ne_bytes());
				bytes.extend(value.to_ne_bytes());
				bytes
			}
			let mut events = Vec::new();
			for (kind, key, value) in [
				(1, 28, 1),  // Pi keyboard Enter
				(1, 169, 1), // KEY_PHONE (call button)
				(1, 169, 0),
				(1, 0x1bd, 1), // KEY_PICKUP_PHONE
				(1, 0x1bd, 0),
				(1, KEY_PLAYPAUSE, 1), // headset play/pause pressed
				(1, KEY_PLAYPAUSE, 2), // autorepeat
				(1, KEY_PLAYPAUSE, 1),
				(0, KEY_PLAYPAUSE, 0),
				(1, KEY_PLAYPAUSE, 0), // headset play/pause released
			] {
				events.extend(event(kind, key, value));
			}
			let (sender, receiver) = mpsc::channel();
			let input_address = address("AA:BB:CC:DD:EE:01");
			read_input_events(std::io::Cursor::new(events), input_address.clone(), sender);

			let (address, pressed, press_at) = receiver.recv().unwrap().unwrap();
			assert_eq!((address, pressed), (input_address.clone(), true));
			let (address, pressed, release_at) = receiver.recv().unwrap().unwrap();
			assert_eq!((address, pressed), (input_address.clone(), false));
			assert!(release_at >= press_at);
			assert!(receiver.recv().unwrap().is_err());
			assert!(matches!(
				receiver.try_recv(),
				Err(TryRecvError::Disconnected)
			));

			let (sender, receiver) = mpsc::channel();
			read_input_events(std::io::Cursor::new(event(0, 3, 0)), input_address, sender);
			assert!(receiver.recv().unwrap().is_err());
		}
	}

	mod reconnect_worker {
		use super::*;

		#[test]
		fn connection_cancellation_skips_remaining_devices() {
			let stopped = AtomicBool::new(false);
			let allowed = [
				"AA:BB:CC:DD:EE:01".into(),
				"AA:BB:CC:DD:EE:02".into(),
				"AA:BB:CC:DD:EE:03".into(),
				"AA:BB:CC:DD:EE:04".into(),
			]
			.into();
			let calls = Mutex::new(0);
			connect_disconnected(&allowed, &stopped, |args, _| {
				assert_eq!(args[1], "info");
				*calls.lock().unwrap() += 1;
				stopped.store(true, Ordering::SeqCst);
				Err("cancelled".into())
			});
			assert!(calls.lock().unwrap().le(&2));
		}

		#[test]
		fn reconnect_checks_run_with_bounded_concurrency() {
			use std::sync::atomic::AtomicUsize;

			let allowed = [
				"AA:BB:CC:DD:EE:01".into(),
				"AA:BB:CC:DD:EE:02".into(),
				"AA:BB:CC:DD:EE:03".into(),
			]
			.into();
			let active = AtomicUsize::new(0);
			let maximum = AtomicUsize::new(0);
			connect_disconnected(&allowed, &AtomicBool::new(false), |args, _| {
				if args[1] == "info" {
					return Ok("Connected: no\n".into());
				}
				assert!(args.contains(&"connect"));
				let current = active.fetch_add(1, Ordering::SeqCst) + 1;
				maximum.fetch_max(current, Ordering::SeqCst);
				thread::sleep(Duration::from_millis(20));
				active.fetch_sub(1, Ordering::SeqCst);
				Ok(String::new())
			});
			assert_eq!(maximum.load(Ordering::SeqCst), 2);
		}

		#[test]
		fn reconnect_worker_retries_and_wakes_on_shutdown() {
			let (shutdown, receiver) = mpsc::channel();
			let (attempt, attempts) = mpsc::channel();
			let worker = thread::spawn(move || {
				reconnect_worker(
					Arc::new(Mutex::new(["AA:BB:CC:DD:EE:01".into()].into())),
					Arc::new(AtomicBool::new(false)),
					receiver,
					Duration::from_millis(10),
					|_, _| {
						attempt.send(()).unwrap();
						Ok("Connected: yes\n".into())
					},
				);
			});
			attempts.recv_timeout(Duration::from_secs(5)).unwrap();
			attempts.recv_timeout(Duration::from_secs(5)).unwrap();
			shutdown.send(()).unwrap();
			worker.join().unwrap();
		}

		#[test]
		fn reconnect_worker_picks_up_newly_enrolled_devices() {
			let devices = Arc::new(Mutex::new(BTreeSet::from(["AA:BB:CC:DD:EE:01".into()])));
			let updated = Arc::clone(&devices);
			let stopped = Arc::new(AtomicBool::new(false));
			let cancelled = Arc::clone(&stopped);
			let (_shutdown, receiver) = mpsc::channel();
			let calls = Mutex::new(Vec::new());
			reconnect_worker(devices, stopped, receiver, Duration::ZERO, |args, _| {
				let mut calls = calls.lock().unwrap();
				calls.push(args[2].to_string());
				if calls.len() == 1 {
					updated.lock().unwrap().insert("AA:BB:CC:DD:EE:02".into());
				} else if args[2] == "AA:BB:CC:DD:EE:02" {
					cancelled.store(true, Ordering::SeqCst);
				}
				Ok("Connected: yes\n".into())
			});
			let calls = calls.lock().unwrap();
			assert_eq!(calls[0], "AA:BB:CC:DD:EE:01");
			assert!(calls.contains(&"AA:BB:CC:DD:EE:02".to_string()));
		}
	}
}
