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
			.short('V')
			.short_alias('v')
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
		/// Routing update interval in seconds.
		#[arg(long, default_value_t = 2.0, value_parser = parse_interval)]
		interval: f64,
		/// Reconnect disconnected headsets every 30 seconds.
		#[arg(long)]
		connect: bool,
		/// Maximum simultaneous SCO/eSCO links per Bluetooth controller (default: 1).
		/// Increase only after verifying the controller, firmware, and headset capacity.
		#[arg(long, default_value_t = 1, value_parser = parse_sco_limit)]
		sco_limit: usize,
		/// Pair one nearby Just Works headset on a Raspberry Pi GPIO17 button press.
		/// Requires full-duplex without --ptt; wire physical pin 11 to ground.
		#[arg(long, conflicts_with = "ptt")]
		pair_button: bool,
		/// Map a headset address to an evdev input path; repeat once per headset.
		/// Defaults to matching every headset automatically. Use ADDRESS=auto, or a
		/// lone `auto`, to request automatic discovery explicitly.
		#[arg(long, value_name = "ADDRESS=/dev/input/eventX|auto", value_parser = parse_ptt_binding)]
		ptt: Vec<PttButton>,
		/// Transmit policy. Half-duplex requires --ptt for every headset:
		/// hold play/pause to request the FIFO floor; release to cancel/relinquish.
		/// Full-duplex mappings start in PTT; three play/pause presses within
		/// 1 second (first to third, inclusive; release between presses) toggle
		/// that headset independently between PTT and always open.
		/// Without active mappings full-duplex is always open. Disconnected mapped
		/// headsets reset to PTT and require a fresh press after reconnection.
		#[arg(long, value_enum, default_value_t = Mode::FullDuplex)]
		mode: Mode,
		/// Audio transport: hfp leaves profiles unchanged; sco-a2dp switches
		/// the talker to HFP and listeners to A2DP (requires half-duplex PTT).
		#[arg(long, value_enum, default_value_t = Transport::Hfp)]
		transport: Transport,
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
	use clap::FromArgMatches;

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
}
