// Copyright (C) 2026 Philip Eriksson. All rights reserved.

use crate::bluez::{Address, parse_address};
use crate::host_ptt::Keyboards;
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
	after_help = "Quick start (with two HFP/HSP headsets):\n  bt-intercom scan\n  bt-intercom pair ADDRESS\n  bt-intercom status ADDRESS_1 ADDRESS_2\n  bt-intercom run ADDRESS_1 ADDRESS_2\n\nRun `bt-intercom <command> --help` for options; `pair` needs an interactive terminal.",
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
	/// Test adapters and headsets on real hardware: connects them, exercises
	/// A2DP and HFP, and measures simultaneous SCO links per controller.
	/// Uses the saved network when no address is given. Stop any running
	/// intercom first: profiles are switched and released (unsaved).
	Diagnose {
		/// Only inspect BlueZ and PipeWire state; do not connect or switch profiles.
		#[arg(long)]
		passive: bool,
		#[arg(value_parser = parse_address)]
		addresses: Vec<Address>,
		/// Assumed simultaneous SCO/eSCO links per controller (default: 1).
		#[arg(long, default_value_t = 1, value_parser = parse_sco_limit)]
		sco_limit: usize,
		/// Assumed simultaneous A2DP streams per controller (default: 1).
		#[arg(long, default_value_t = 1, value_parser = parse_a2dp_limit)]
		a2dp_limit: usize,
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
		/// Maximum simultaneous A2DP streams per Bluetooth controller (default: 1).
		/// Used by the sco-a2dp transport for listeners; increase only after verification.
		#[arg(long, default_value_t = 1, value_parser = parse_a2dp_limit)]
		a2dp_limit: usize,
		/// Map a headset address to an evdev input path; repeat once per headset.
		/// Defaults to matching every headset automatically. Use ADDRESS=auto, or a
		/// lone `auto`, to request automatic discovery explicitly.
		#[arg(long, value_name = "ADDRESS=/dev/input/eventX|auto", value_parser = parse_ptt_binding)]
		ptt: Vec<PttButton>,
		/// Transmit policy (default: half-duplex).
		/// Half-duplex uses PTT for every headset:
		/// tap play to request the FIFO floor; tap pause to cancel/relinquish.
		/// Full-duplex lets every headset that tapped play transmit until it
		/// taps pause. A single toggling play/pause key alternates each tap.
		/// Disconnected headsets are muted and require a fresh play tap after
		/// reconnection.
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
		/// Also join the intercom from this computer through its wired headset
		/// (non-Bluetooth microphone and speakers in PipeWire). Hold the
		/// spacebar to talk; it shares the half-duplex floor with the headsets.
		#[arg(long)]
		host: bool,
		/// Keyboard event device whose spacebar is the host's push-to-talk,
		/// or `auto` for every attached keyboard (default). Needs `input` group access.
		#[arg(
			long,
			requires = "host",
			value_name = "/dev/input/eventX|auto",
			default_value = "auto",
			value_parser = Keyboards::parse
		)]
		host_keyboard: Keyboards,
		/// PipeWire node.name of the host's microphone (default: the PipeWire
		/// default source if it is not Bluetooth).
		#[arg(long, requires = "host", value_name = "NODE_NAME")]
		host_source: Option<String>,
		/// PipeWire node.name of the host's speakers (default: the PipeWire
		/// default sink if it is not Bluetooth).
		#[arg(long, requires = "host", value_name = "NODE_NAME")]
		host_sink: Option<String>,
	},
	/// Generate shell completion definitions.
	Completions {
		#[arg(value_enum)]
		shell: Shell,
	},
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

fn parse_a2dp_limit(value: &str) -> Result<usize, String> {
	let limit = value
		.parse::<usize>()
		.map_err(|_| "--a2dp-limit must be a positive integer")?;
	if limit == 0 {
		return Err("--a2dp-limit must be at least 1".into());
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
	fn host_options_require_host_and_default_to_automatic_keyboards() {
		let CliCommand::Run {
			host,
			host_keyboard,
			host_source,
			host_sink,
			..
		} = Cli::try_parse_from(["bt-intercom", "run"]).unwrap().command
		else {
			panic!("expected run");
		};
		assert!(!host);
		assert_eq!(host_keyboard, Keyboards::Auto);
		assert!(host_source.is_none() && host_sink.is_none());
		for option in ["--host-keyboard", "--host-source", "--host-sink"] {
			assert!(Cli::try_parse_from(["bt-intercom", "run", option, "x"]).is_err());
		}
		let CliCommand::Run {
			host,
			host_keyboard,
			host_source,
			host_sink,
			..
		} = Cli::try_parse_from([
			"bt-intercom",
			"run",
			"--host",
			"--host-keyboard",
			"/dev/input/event3",
			"--host-source",
			"in",
			"--host-sink",
			"out",
		])
		.unwrap()
		.command
		else {
			panic!("expected run");
		};
		assert!(host);
		assert_eq!(host_keyboard, Keyboards::Path("/dev/input/event3".into()));
		assert_eq!(host_source.as_deref(), Some("in"));
		assert_eq!(host_sink.as_deref(), Some("out"));
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
	fn top_level_help_mentions_dashboard() {
		let help = cli_command().render_help().to_string();
		assert!(help.contains("dashboard"));
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
