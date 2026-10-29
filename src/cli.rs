// Copyright (C) 2026 Philip Eriksson. All rights reserved.

use crate::bluez::{Address, parse_address};
use crate::ptt::PttButton;
use crate::runtime_config::parse_interval;
use crate::transmit::Mode;
use crate::transport::Transport;
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;

#[derive(Parser)]
#[command(
	name = "bt-intercom",
	version = concat!(
		env!("CARGO_PKG_VERSION"),
		"\nBuild datetime: ",
		env!("BT_INTERCOM_BUILD_DATETIME")
	),
	long_version = concat!(
		env!("CARGO_PKG_VERSION"),
		"\nBuild datetime: ",
		env!("BT_INTERCOM_BUILD_DATETIME")
	),
	about = "A Bluetooth headset intercom for Linux",
	after_help = "Quick start (with two HFP/HSP headsets):\n  bt-intercom scan\n  bt-intercom pair ADDRESS\n  bt-intercom status ADDRESS_1 ADDRESS_2\n  bt-intercom run ADDRESS_1 ADDRESS_2\n\nRun `bt-intercom <command> --help` for options; `pair` and `panel` need an interactive terminal.",
	arg_required_else_help = true
)]
pub(crate) struct Cli {
	/// Enable detailed diagnostics on stderr (unless RUST_LOG is set).
	#[arg(long, global = true)]
	pub(crate) verbose: bool,
	#[command(subcommand)]
	pub(crate) command: CliCommand,
}

pub(crate) fn cli_command() -> clap::Command {
	Cli::command().disable_version_flag(true).arg(
		clap::Arg::new("version")
			.short('v')
			.long("version")
			.action(clap::ArgAction::Version),
	)
}

#[derive(Subcommand)]
pub(crate) enum CliCommand {
	/// Scan for nearby Bluetooth devices.
	Scan {
		/// Scan duration in seconds.
		#[arg(
			long,
			default_value_t = 15,
			value_parser = clap::value_parser!(u64).range(1..=300)
		)]
		seconds: u64,
	},
	/// Pair, trust, and connect a headset.
	Pair {
		#[arg(value_parser = parse_address)]
		address: Address,
	},
	/// Remove a headset from the saved intercom network.
	Remove {
		#[arg(value_parser = parse_address)]
		address: Address,
	},
	/// Show Bluetooth headset audio status.
	Status {
		#[arg(required = true, value_parser = parse_address)]
		addresses: Vec<Address>,
	},
	/// Route configured headset microphones to one another, optionally showing the live dashboard.
	Run {
		#[arg(value_parser = parse_address)]
		addresses: Vec<Address>,
		/// Routing update interval in seconds (minimum: 1).
		#[arg(long, default_value_t = 2.0, value_parser = parse_interval)]
		interval: f64,
		/// Do not connect headsets at startup or reconnect them every 30 seconds.
		#[arg(long)]
		no_connect: bool,
		/// Maximum simultaneous SCO/eSCO links per Bluetooth controller (default: 1).
		/// Increase only after verifying the controller, firmware, and headset capacity.
		#[arg(long, default_value_t = 1, value_parser = parse_sco_limit)]
		sco_limit: usize,
		/// Map a headset address to an evdev input path; repeat once per headset.
		/// Defaults to matching every headset automatically. Use ADDRESS=auto, or a
		/// lone `auto`, to request automatic discovery explicitly.
		#[arg(long, value_name = "ADDRESS=/dev/input/eventX|auto", value_parser = parse_ptt_binding)]
		ptt: Vec<PttButton>,
		/// Transmit policy (default: half-duplex).
		/// Half-duplex uses PTT for every headset:
		/// hold play/pause to request the FIFO floor; release to cancel/relinquish.
		/// Full-duplex mappings start in PTT; three play/pause presses within
		/// 1 second (first to third, inclusive; release between presses) toggle
		/// that headset independently between PTT and always open.
		/// Without active mappings full-duplex is always open. Disconnected mapped
		/// headsets reset to PTT and require a fresh press after reconnection.
		#[arg(long, value_enum)]
		mode: Option<Mode>,
		/// Audio transport: hfp leaves profiles unchanged; sco-a2dp switches
		/// the talker to HFP and listeners to A2DP (requires half-duplex PTT).
		/// Default: sco-a2dp in half-duplex, hfp in full-duplex.
		#[arg(long, value_enum)]
		transport: Option<Transport>,
		/// Show the live terminal dashboard.
		#[arg(long)]
		dashboard: bool,
	},
	/// Generate shell completion definitions.
	Completions {
		#[arg(value_enum)]
		shell: Shell,
	},
	/// Open the interactive terminal control panel.
	Panel,
}

fn parse_sco_limit(value: &str) -> Result<usize, String> {
	let limit = value
		.parse::<usize>()
		.map_err(|_| "--sco-limit must be a positive integer")?;
	if limit == 0 {
		return Err("--sco-limit must be at least 1".into());
	}
	Ok(limit)
}

fn parse_ptt_binding(value: &str) -> Result<PttButton, String> {
	if value == crate::ptt_discovery::AUTO {
		return Ok(PttButton {
			address: None,
			path: value.into(),
		});
	}
	let (device, path) = value
		.split_once('=')
		.ok_or("--ptt must be ADDRESS=/dev/input/eventX")?;
	if path.is_empty() {
		return Err("--ptt requires an input device path".into());
	}
	Ok(PttButton {
		address: Some(parse_address(device).map_err(|error| error.to_string())?),
		path: path.into(),
	})
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::bluez::Address;
	use clap::{FromArgMatches, Parser};

	fn address(value: &str) -> Address {
		Address::parse(value).unwrap()
	}

	#[test]
	fn validates_ptt_bindings() {
		let address = "AA:BB:CC:DD:EE:01";
		assert!(Cli::try_parse_from(["bt-intercom", "run", "--ptt", address]).is_err());
		assert!(
			Cli::try_parse_from(["bt-intercom", "run", "--ptt", &format!("{address}=")]).is_err()
		);
		assert!(parse_ptt_binding("auto").is_ok());
		assert!(parse_ptt_binding(&format!("{address}=/dev/input/event4")).is_ok());
	}

	#[test]
	fn cli_command_preserves_version_aliases_and_completions() {
		let command = cli_command();
		assert_eq!(
			command
				.clone()
				.try_get_matches_from(["bt-intercom", "-v"])
				.unwrap_err()
				.kind(),
			clap::error::ErrorKind::DisplayVersion
		);
		for shell in ["bash", "zsh", "fish", "elvish"] {
			assert!(Cli::try_parse_from(["bt-intercom", "completions", shell]).is_ok());
		}
		let matches = command
			.try_get_matches_from(["bt-intercom", "run"])
			.unwrap();
		assert!(Cli::from_arg_matches(&matches).is_ok());
	}

	#[test]
	fn clap_cli_validates_arguments_and_accepts_completion_shells() {
		for args in [
			vec!["bt-intercom", "run", "invalid"],
			vec!["bt-intercom", "run", "--interval", "NaN"],
			vec!["bt-intercom", "run", "--interval", "0.00000000001"],
			vec!["bt-intercom", "run", "--sco-limit", "0"],
			vec!["bt-intercom", "scan", "--seconds", "301"],
			vec!["bt-intercom", "status"],
			vec!["bt-intercom", "run", "--unknown"],
			vec!["bt-intercom", "run", "--ptt"],
			vec!["bt-intercom", "run", "--ptt", "AA:BB:CC:DD:EE:01="],
			vec!["bt-intercom", "remove"],
			vec!["bt-intercom", "remove", "invalid"],
		] {
			assert_eq!(Cli::try_parse_from(args).err().unwrap().exit_code(), 2);
		}

		for shell in ["bash", "zsh", "fish", "elvish"] {
			assert!(Cli::try_parse_from(["bt-intercom", "completions", shell]).is_ok());
		}
	}

	#[test]
	fn terminal_control_panel_uses_panel_subcommand() {
		assert!(matches!(
			Cli::try_parse_from(["bt-intercom", "panel"])
				.unwrap()
				.command,
			CliCommand::Panel
		));
		assert!(Cli::try_parse_from(["bt-intercom", "tui"]).is_err());
	}

	#[test]
	fn verbose_is_global_and_version_short_alias_is_preserved() {
		for args in [
			["bt-intercom", "--verbose", "run"],
			["bt-intercom", "run", "--verbose"],
		] {
			assert!(Cli::try_parse_from(args).unwrap().verbose);
		}
		assert!(!Cli::try_parse_from(["bt-intercom", "run"]).unwrap().verbose);
		assert_eq!(
			cli_command()
				.try_get_matches_from(["bt-intercom", "-v"])
				.unwrap_err()
				.kind(),
			clap::error::ErrorKind::DisplayVersion
		);
	}

	#[test]
	fn version_flags_print_the_long_version_with_build_datetime() {
		let command = cli_command();
		let long_version = command.get_long_version().unwrap().to_string();
		assert_eq!(command.get_version(), Some(long_version.as_str()));

		let short = command
			.clone()
			.try_get_matches_from(["bt-intercom", "-v"])
			.err()
			.unwrap()
			.to_string();
		let long = command
			.clone()
			.try_get_matches_from(["bt-intercom", "--version"])
			.err()
			.unwrap()
			.to_string();

		assert_eq!(short, long);
		assert!(short.contains(env!("CARGO_PKG_VERSION")));
		assert!(short.contains(env!("BT_INTERCOM_BUILD_DATETIME")));
	}

	#[test]
	fn top_level_help_mentions_dashboard_and_control_panel() {
		let help = cli_command().render_help().to_string();
		assert!(help.contains("dashboard"));
		assert!(help.contains("control panel"));
		assert!(help.contains("Quick start"));
		assert!(help.contains("bt-intercom status ADDRESS_1 ADDRESS_2"));
	}

	#[test]
	fn parses_ptt_without_changing_default_mode() {
		let raw_address = "AA:BB:CC:DD:EE:01";
		let default = Cli::try_parse_from(["bt-intercom", "run", raw_address]).unwrap();
		let CliCommand::Run {
			ptt,
			mode,
			dashboard,
			..
		} = default.command
		else {
			panic!("expected run command");
		};
		assert!(ptt.is_empty());
		assert_eq!(mode, None);
		assert!(!dashboard);

		let binding = format!("{raw_address}=/dev/input/event4");
		let configured = Cli::try_parse_from([
			"bt-intercom",
			"run",
			"--ptt",
			binding.as_str(),
			raw_address,
			"--dashboard",
		])
		.unwrap();
		let CliCommand::Run {
			addresses,
			ptt,
			dashboard,
			..
		} = configured.command
		else {
			panic!("expected run command");
		};
		assert_eq!(addresses, [address(raw_address)]);
		assert!(dashboard);
		assert_eq!(ptt.len(), 1);
		assert_eq!(ptt[0].path, "/dev/input/event4");
		assert_eq!(ptt[0].address, Some(address(raw_address)));
	}

	#[test]
	fn no_connect_opts_out_of_default_reconnection() {
		let parse = |args: &[&str]| {
			let CliCommand::Run { no_connect, .. } = Cli::try_parse_from(args).unwrap().command
			else {
				panic!("expected run command");
			};
			no_connect
		};
		assert!(!parse(&["bt-intercom", "run"]));
		assert!(parse(&["bt-intercom", "run", "--no-connect"]));
	}
}
