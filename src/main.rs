// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Command-line entry point for Bluetooth setup, status, and intercom routing.
//!
//! External commands are bounded by timeouts; the long-running `run` mode also
//! supports cancellation, reconnecting, push-to-talk input, and a terminal view.

mod atomic_file;
mod bluez;
mod dashboard;
mod device_status;
mod groups;
mod network;
mod pair_button;
mod pipewire;
mod process;
mod ptt;
mod router;
mod routing_status;
mod runtime_config;
mod session;
mod terminal_style;
mod transmit;
mod transport;
mod worker;

mod tui;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, OpenOptions};
use std::io::{IsTerminal, Write};
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::sync::{Arc, Mutex};
#[cfg(test)]
use std::thread;
use std::time::{Duration, Instant};

use bluez::{bluetooth_name, device_flag, normalize_address};
use clap::{CommandFactory, FromArgMatches, Parser, Subcommand};
use clap_complete::Shell;
use groups::config_path as talk_groups_path;
use network::{
	headset_path as headset_network_path, load as load_headsets, remove as remove_headset,
	runtime_directory as ptt_runtime_directory,
};
#[cfg(test)]
use network::{load_for_run as load_run_headsets, save as save_headsets};
use process::{COMMAND_TIMEOUT, command, command_cancellable};
use router::{Headset, Router};
use routing_status::RoutingStatus;
use transmit::{Mode, Transmit};
use transport::Transport;

const PTT_BEEP_WAV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ptt-beep.wav"));

#[derive(Clone, Debug)]
struct PttButton {
	address: String,
	path: String,
}

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
struct Cli {
	#[command(subcommand)]
	command: CliCommand,
}

fn cli_command() -> clap::Command {
	Cli::command().disable_version_flag(true).arg(
		clap::Arg::new("version")
			.short('V')
			.short_alias('v')
			.long("version")
			.action(clap::ArgAction::Version),
	)
}

#[derive(Subcommand)]
enum CliCommand {
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
		#[arg(value_parser = address)]
		address: String,
	},
	/// Remove a headset from the saved intercom network.
	Remove {
		#[arg(value_parser = address)]
		address: String,
	},
	/// Show Bluetooth headset audio status.
	Status {
		#[arg(required = true, value_parser = address)]
		addresses: Vec<String>,
	},
	/// Route configured headset microphones to one another.
	Run {
		#[arg(value_parser = address)]
		addresses: Vec<String>,
		/// Routing update interval in seconds.
		#[arg(long, default_value_t = 2.0, value_parser = parse_interval)]
		interval: f64,
		/// Reconnect disconnected headsets every 30 seconds.
		#[arg(long)]
		connect: bool,
		/// Pair one nearby Just Works headset on a Raspberry Pi GPIO17 button press.
		/// Requires full-duplex without --ptt; wire physical pin 11 to ground.
		#[arg(long, conflicts_with = "ptt")]
		pair_button: bool,
		/// Map a headset address to an evdev input path; repeat once per headset.
		#[arg(long, value_name = "ADDRESS=/dev/input/eventX", value_parser = parse_ptt_binding)]
		ptt: Vec<PttButton>,
		/// Transmit policy. Half-duplex requires --ptt for every headset:
		/// hold play/pause to request the FIFO floor; release to cancel/relinquish.
		/// Full-duplex mappings start in PTT; three play/pause presses within
		/// 1 second (first to third, inclusive; release between presses) toggle
		/// that headset independently between PTT and always open.
		/// Without mappings full-duplex is always open. Disconnected mapped
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
	/// Open the terminal headset and talk-group control panel.
	Tui,
}

/// Opens an interactive BlueZ agent session to pair the specified device.
fn pair_command(device: &str) -> Result<(), String> {
	eprintln!("At the Bluetooth prompt, enter: pair {device}");
	eprintln!("Answer any PIN/confirmation prompts, then enter: quit");
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

/// Validates and normalizes a Bluetooth MAC address.
fn address(value: &str) -> Result<String, String> {
	normalize_address(value)
		.ok_or_else(|| format!("expected a Bluetooth MAC address (XX:XX:XX:XX:XX:XX): {value}"))
}

fn parse_interval(value: &str) -> Result<f64, String> {
	let interval = value
		.parse::<f64>()
		.map_err(|_| "--interval must be a positive finite number")?;
	if !interval.is_finite() || interval <= 0.0 || interval >= u64::MAX as f64 {
		return Err("--interval must be a positive finite number".into());
	}
	if Duration::from_secs_f64(interval).is_zero() {
		return Err("--interval must be at least one nanosecond".into());
	}
	Ok(interval)
}

fn parse_ptt_binding(value: &str) -> Result<PttButton, String> {
	let (device, path) = value
		.split_once('=')
		.ok_or("--ptt must be ADDRESS=/dev/input/eventX")?;
	if path.is_empty() {
		return Err("--ptt requires an input device path".into());
	}
	Ok(PttButton {
		address: address(device)?,
		path: path.into(),
	})
}

/// Parses and validates one or more Bluetooth addresses.
fn addresses(args: &[String]) -> Result<BTreeSet<String>, String> {
	if args.is_empty() {
		return Err("at least one Bluetooth address is required".into());
	}
	args.iter().map(|value| address(value)).collect()
}

/// Linux evdev key code emitted by supported headset play/pause buttons.
const KEY_PLAYPAUSE: u16 = 164;

/// A button state update or an input-device failure.
type PttEvent = Result<(String, bool, Instant), String>;

/// WAV feedback played to the headset whose microphone became active.
struct PttBeep {
	path: PathBuf,
}

impl PttBeep {
	fn new() -> Result<Self, String> {
		let runtime_dir = ptt_runtime_directory(env::var_os("XDG_RUNTIME_DIR").as_deref());
		let nonce = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap_or_default()
			.as_nanos();
		let path = runtime_dir.join(format!(
			"bt-intercom-ptt-{}-{nonce}.wav",
			std::process::id()
		));
		let mut file = OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&path)
			.map_err(|error| format!("could not create PTT beep: {error}"))?;
		if let Err(error) = file.write_all(PTT_BEEP_WAV) {
			drop(file);
			let _ = fs::remove_file(&path);
			return Err(format!("could not write PTT beep: {error}"));
		}
		Ok(Self { path })
	}

	fn play(&self, speaker_node: u64, stopped: &AtomicBool) -> Result<(), String> {
		let target = format!("--target={speaker_node}");
		let path = self.path.to_string_lossy().into_owned();
		command_cancellable(
			&["pw-play", &target, &path],
			Duration::from_secs(2),
			Some(stopped),
		)
		.map(|_| ())
	}
}

impl Drop for PttBeep {
	fn drop(&mut self) {
		let _ = fs::remove_file(&self.path);
	}
}

fn confirm_transmissions(
	mode: Mode,
	transmit: &mut Transmit,
	input: &Receiver<PttEvent>,
	headsets: &BTreeMap<String, Headset>,
	links: &BTreeSet<(u64, u64)>,
	beep: Option<&PttBeep>,
	stopped: &AtomicBool,
) -> Result<bool, String> {
	let status = RoutingStatus::new(headsets, links);
	let ready = transmit
		.pending
		.iter()
		.filter_map(|address| {
			let headset = headsets.get(address)?;
			(transmit.sources().contains(address) && status.has_active_source_route(address))
				.then(|| (address.clone(), headset.speaker_node()))
		})
		.collect::<Vec<_>>();
	for (address, speaker_node) in ready {
		// Playback itself can block, so re-check input before *each* beep.
		if drain_ptt(input, transmit)? || stopped.load(Ordering::SeqCst) {
			return Ok(true);
		}
		let Some(speaker_node) = speaker_node else {
			let error = format!("could not play PTT confirmation for {address}: no speaker node");
			if mode == Mode::HalfDuplex {
				return Err(error);
			}
			log::warn!("{error}");
			continue;
		};
		let result = beep
			.ok_or_else(|| "confirmation beep is unavailable".to_string())
			.and_then(|beep| beep.play(speaker_node, stopped));
		match result {
			Ok(()) => {
				transmit.pending.remove(&address);
			}
			Err(_error) if stopped.load(Ordering::SeqCst) => return Ok(true),
			Err(error) if mode == Mode::HalfDuplex => {
				return Err(format!(
					"could not play PTT confirmation for {address}: {error}"
				));
			}
			Err(error) => {
				log::warn!("Could not play PTT confirmation for {address}: {error}");
			}
		}
	}
	Ok(false)
}

fn confirm_connections(
	routed_headsets: &mut BTreeSet<String>,
	headsets: &BTreeMap<String, Headset>,
	links: &BTreeSet<(u64, u64)>,
	beep: Option<&PttBeep>,
	stopped: &AtomicBool,
) {
	let status = RoutingStatus::new(headsets, links);
	let current: BTreeSet<_> = headsets
		.keys()
		.filter(|address| status.has_active_intercom_connection(address))
		.cloned()
		.collect();
	for address in current.difference(routed_headsets) {
		let Some(speaker_node) = headsets[address].speaker_node() else {
			log::warn!("Could not play intercom connection beep for {address}: no speaker node");
			continue;
		};
		if let Some(beep) = beep
			&& let Err(error) = beep.play(speaker_node, stopped)
		{
			log::warn!("Could not play intercom connection beep for {address}: {error}");
		}
	}
	*routed_headsets = current;
}

/// Validates that PTT bindings uniquely cover the configured headset network.
fn validate_ptt(buttons: &[PttButton], allowed: &BTreeSet<String>) -> Result<(), String> {
	let mut configured = BTreeSet::new();
	let mut paths = BTreeSet::new();
	for button in buttons {
		if (!allowed.is_empty() && !allowed.contains(&button.address))
			|| !configured.insert(button.address.clone())
		{
			return Err("--ptt requires one unique button mapping per listed headset".into());
		}
		if !paths.insert(&button.path) {
			return Err("--ptt requires a separate input device for each headset".into());
		}
	}
	if !buttons.is_empty() && !allowed.is_empty() && configured != *allowed {
		return Err("--ptt requires a button mapping for every listed headset".into());
	}
	Ok(())
}

fn validate_mode(
	mode: Mode,
	buttons: &[PttButton],
	allowed: &BTreeSet<String>,
) -> Result<(), String> {
	validate_ptt(buttons, allowed)?;
	if mode == Mode::HalfDuplex && buttons.is_empty() {
		return Err("--mode half-duplex requires --ptt for every headset".into());
	}
	Ok(())
}

fn validate_transport(mode: Mode, transport: Transport) -> Result<(), String> {
	if transport == Transport::ScoA2dp && mode != Mode::HalfDuplex {
		return Err("--transport sco-a2dp requires --mode half-duplex and --ptt for every headset; use one controller per headset for full-duplex".into());
	}
	Ok(())
}

fn controller_warnings(devices: &BTreeMap<String, transport::Device>) -> Vec<String> {
	let mut controllers: BTreeMap<&str, Vec<&str>> = BTreeMap::new();
	for (address, device) in devices {
		controllers
			.entry(&device.controller)
			.or_default()
			.push(address);
	}
	controllers
		.into_iter()
		.filter(|(_, addresses)| addresses.len() > 1)
		.map(|(controller, addresses)| {
			format!(
				"{} headsets share {controller} ({}). HFP listeners also need SCO/eSCO; full-duplex requires verified synchronous-link capacity, normally one headset per controller. Use --mode half-duplex --transport sco-a2dp with PTT when sharing a radio.",
				addresses.len(),
				addresses.join(", ")
			)
		})
		.collect()
}

fn refresh_transport_requests(
	transmit: &mut Transmit,
	devices: &BTreeMap<String, transport::Device>,
	disconnected: &BTreeSet<String>,
) -> bool {
	let before = transmit.sources().clone();
	let available: BTreeSet<_> = devices.keys().cloned().collect();
	transmit.topology(&available.difference(disconnected).cloned().collect());
	transmit.set_controllers(
		&devices
			.iter()
			.map(|(address, device)| (address.clone(), device.controller.clone()))
			.collect(),
	);
	transmit.topology(&available);
	before != *transmit.sources()
}

/// Drain timestamped transitions before selecting sources and again after slow
/// routing work, so a released/cancelled request never earns a stale beep.
fn drain_ptt(input: &Receiver<PttEvent>, transmit: &mut Transmit) -> Result<bool, String> {
	let mut changed = false;
	loop {
		match input.try_recv() {
			Ok(Ok((address, pressed, at))) => {
				changed |= transmit.event(&address, pressed, at);
			}
			Ok(Err(error)) => return Err(error),
			Err(TryRecvError::Empty) => return Ok(changed),
			Err(TryRecvError::Disconnected) => return Err("PTT input closed".into()),
		}
	}
}

/// A failed snapshot can leave the previous floor's owned routes untouched.
/// Fail closed before inspection/confirmation in half-duplex; preserve the
/// existing non-fatal update warnings in full-duplex.
fn guard_half_update<T>(
	mode: Mode,
	update: &Result<T, String>,
	close_owned: impl FnOnce(),
) -> Result<(), String> {
	if mode == Mode::HalfDuplex
		&& let Err(error) = update
	{
		close_owned();
		return Err(format!(
			"Half-duplex routing failed; released owned links and stopped to prevent an unsafe floor handoff: {error}"
		));
	}
	Ok(())
}

#[cfg(test)]
fn ptt_input_from<R: std::io::Read>(input: R, address: String, sender: mpsc::Sender<PttEvent>) {
	let (_wake, receiver) = mpsc::channel();
	ptt::read_events(input, address, sender, &AtomicBool::new(false), &receiver);
}

/// Connects allowlisted devices that are currently disconnected or unavailable.
fn connect_disconnected(
	allowed: &BTreeSet<String>,
	stopped: &AtomicBool,
	mut execute: impl FnMut(&[&str], Duration) -> Result<String, String>,
) {
	for device in allowed {
		if stopped.load(Ordering::SeqCst) {
			break;
		}
		let connected = execute(&["bluetoothctl", "info", device], COMMAND_TIMEOUT)
			.is_ok_and(|info| device_flag(&info, "Connected"));
		if !connected
			&& !stopped.load(Ordering::SeqCst)
			&& let Err(error) = execute(
				&["bluetoothctl", "--timeout", "30", "connect", device],
				Duration::from_secs(35),
			) {
			log::warn!("Could not connect {device}: {error}");
		}
	}
}

/// Repeats connection checks until cancelled or explicitly woken for shutdown.
fn reconnect_worker(
	allowed: Arc<Mutex<BTreeSet<String>>>,
	stopped: Arc<AtomicBool>,
	shutdown: Receiver<()>,
	retry: Duration,
	mut execute: impl FnMut(&[&str], Duration) -> Result<String, String>,
) {
	loop {
		let devices = allowed.lock().unwrap().clone();
		connect_disconnected(&devices, &stopped, &mut execute);
		if stopped.load(Ordering::SeqCst) {
			break;
		}
		if !matches!(
			shutdown.recv_timeout(retry),
			Err(mpsc::RecvTimeoutError::Timeout)
		) {
			break;
		}
	}
}

/// Executes one CLI action, including the main polling and routing loop.
fn run(action: CliCommand) -> Result<(), String> {
	match action {
		CliCommand::Tui => {
			let network_path = headset_network_path()?;
			let allowed = load_headsets(&network_path)?;
			tui::run(allowed, talk_groups_path(&network_path)?)?;
		}
		CliCommand::Scan { seconds } => {
			print!(
				"{}",
				command(
					&[
						"bluetoothctl",
						"--timeout",
						&seconds.to_string(),
						"scan",
						"on"
					],
					Duration::from_secs(seconds + 5)
				)?
			);
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
				let name = command(&["bluetoothctl", "info", address], COMMAND_TIMEOUT)
					.ok()
					.and_then(|info| bluetooth_name(&info));
				let identity =
					name.map_or_else(|| address.clone(), |name| format!("{name} ({address})"));
				match headsets.get(address) {
					Some(headset) => println!(
						"{identity}: {} ({} microphone ports, {} speaker ports)",
						if headset.has_duplex_audio() {
							"duplex ready"
						} else {
							"duplex unavailable"
						},
						headset.sources.len(),
						headset.sinks.len()
					),
					None => println!("{identity}: not found in PipeWire"),
				}
				if let Some(device) = transport_devices.get(address) {
					println!(
						"  controller: {}; HFP: {}; A2DP: {}",
						device.controller, device.hfp_available, device.a2dp_available
					);
				}
			}
			for warning in controller_warnings(&transport_devices) {
				log::warn!("{warning}");
			}
		}
		CliCommand::Run {
			addresses: devices,
			interval,
			connect,
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

/// Reports command-line errors and exits unsuccessfully.
fn main() {
	env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
		.format_timestamp_secs()
		.init();

	let matches = cli_command().get_matches();
	let cli = Cli::from_arg_matches(&matches).unwrap_or_else(|error| error.exit());
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
				std::process::exit(1);
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::ffi::OsStr;

	#[test]
	fn transport_disappearance_then_reconnect_requires_a_fresh_ptt_press() {
		let address = "AA:BB:CC:DD:EE:01".to_owned();
		for disappear_at in [2, 3] {
			let current = if disappear_at == 3 { 41 } else { 17 };
			let connected = serde_json::json!([{
				"type":"PipeWire:Interface:Device", "id":10,
				"info":{
					"props":{
						"api.bluez5.address":address,
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
			let mut router = Router::with_executor([address.clone()].into(), |args: &[&str]| {
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
		let a = "AA:BB:CC:DD:EE:01".to_owned();
		let b = "AA:BB:CC:DD:EE:02".to_owned();
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

	#[test]
	fn validates_address_before_invoking_commands() {
		assert!(address("invalid; rm -rf /").is_err());
		assert_eq!(address("aa:bb:cc:dd:ee:ff").unwrap(), "AA:BB:CC:DD:EE:FF");
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
		assert!(Cli::try_parse_from(["bt-intercom", "run", "--transport", "auracast"]).is_err());
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
			BTreeSet::from([
				"AA:BB:CC:DD:EE:01".to_string(),
				"AA:BB:CC:DD:EE:02".to_string(),
			])
		);
		assert!(addresses(&[]).is_err());
		assert!(addresses(&["AA:BB:CC:DD:EE:01".into(), "invalid".into()]).is_err());
	}

	fn temporary_network_path() -> PathBuf {
		let nonce = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap()
			.as_nanos();
		env::temp_dir().join(format!(
			"bt-intercom-headsets-{}-{nonce}/headsets",
			std::process::id()
		))
	}

	#[test]
	fn uses_absolute_xdg_runtime_dir_for_ptt_beeps() {
		assert_eq!(
			ptt_runtime_directory(Some(OsStr::new("/run/user/1000"))),
			PathBuf::from("/run/user/1000")
		);
		assert_eq!(
			ptt_runtime_directory(Some(OsStr::new("relative"))),
			env::temp_dir()
		);
		assert_eq!(ptt_runtime_directory(None), env::temp_dir());
	}

	#[test]
	fn saves_and_restores_headset_network() {
		let path = temporary_network_path();
		let allowed = BTreeSet::from([
			"AA:BB:CC:DD:EE:02".to_string(),
			"AA:BB:CC:DD:EE:01".to_string(),
		]);
		save_headsets(&path, &allowed).unwrap();
		assert_eq!(load_headsets(&path).unwrap(), allowed);
		assert_eq!(
			fs::read_to_string(&path).unwrap(),
			"AA:BB:CC:DD:EE:01\nAA:BB:CC:DD:EE:02\n"
		);
		fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	#[test]
	fn removes_headsets_from_saved_network() {
		let path = temporary_network_path();
		let allowed = BTreeSet::from([
			"AA:BB:CC:DD:EE:01".to_string(),
			"AA:BB:CC:DD:EE:02".to_string(),
		]);
		save_headsets(&path, &allowed).unwrap();

		remove_headset(&path, "AA:BB:CC:DD:EE:01").unwrap();

		assert_eq!(
			load_headsets(&path).unwrap(),
			BTreeSet::from(["AA:BB:CC:DD:EE:02".to_string()])
		);
		fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	#[test]
	fn removes_last_headset_and_rejects_unknown_address_without_changes() {
		let path = temporary_network_path();
		let allowed = BTreeSet::from(["AA:BB:CC:DD:EE:01".to_string()]);
		save_headsets(&path, &allowed).unwrap();

		assert!(remove_headset(&path, "AA:BB:CC:DD:EE:02").is_err());
		assert_eq!(load_headsets(&path).unwrap(), allowed);
		remove_headset(&path, "AA:BB:CC:DD:EE:01").unwrap();
		assert!(!path.exists());
		assert!(load_headsets(&path).is_err());
		fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	#[test]
	fn loads_normalized_addresses_and_rejects_invalid_network_files() {
		let path = temporary_network_path();
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(&path, "\naa:bb:cc:dd:ee:01\nAA:BB:CC:DD:EE:01\n \n").unwrap();
		assert_eq!(
			load_headsets(&path).unwrap(),
			BTreeSet::from(["AA:BB:CC:DD:EE:01".to_string()])
		);
		fs::write(&path, "not-a-bluetooth-address\n").unwrap();
		assert!(load_headsets(&path).is_err());
		fs::write(&path, "\n \n").unwrap();
		assert!(load_headsets(&path).is_err());
		fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	#[test]
	fn missing_saved_network_explains_how_to_create_it() {
		let error = load_headsets(&temporary_network_path()).unwrap_err();
		assert!(error.contains("run with headset addresses first"));
	}

	#[test]
	fn only_button_mode_can_start_without_a_saved_network() {
		let path = temporary_network_path();
		assert!(load_run_headsets(&path, false).is_err());
		assert!(load_run_headsets(&path, true).unwrap().is_empty());
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(&path, "invalid\n").unwrap();
		assert!(load_run_headsets(&path, true).is_err());
		fs::write(&path, "AA:BB:CC:DD:EE:01\n").unwrap();
		assert_eq!(
			load_run_headsets(&path, true).unwrap(),
			BTreeSet::from(["AA:BB:CC:DD:EE:01".into()])
		);
		fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	#[test]
	fn enrollment_preserves_network_and_only_allows_saved_devices() {
		let path = temporary_network_path();
		let mut allowed = BTreeSet::from(["AA:BB:CC:DD:EE:01".into()]);
		network::enroll(&path, &mut allowed, "AA:BB:CC:DD:EE:02".into()).unwrap();
		assert_eq!(load_headsets(&path).unwrap(), allowed);
		assert_eq!(allowed.len(), 2);
		network::enroll(&path, &mut allowed, "AA:BB:CC:DD:EE:02".into()).unwrap();
		assert_eq!(allowed.len(), 2);
		let before = allowed.clone();
		assert!(
			network::enroll(
				&path.join("not-a-directory"),
				&mut allowed,
				"AA:BB:CC:DD:EE:03".into(),
			)
			.is_err()
		);
		assert_eq!(allowed, before);
		assert_eq!(load_headsets(&path).unwrap(), before);
		fs::remove_dir_all(path.parent().unwrap()).unwrap();
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
	fn confirms_bluetooth_device_flags() {
		assert!(!device_flag("  Paired: no\n", "Paired"));
		assert!(device_flag("  Paired: yes\n", "Paired"));
		assert!(!device_flag("Not Paired: yes\n", "Paired"));
		assert!(device_flag("  Connected: yes\n", "Connected"));
		assert!(!device_flag("  Paired: yes\n", "Connected"));
	}

	#[test]
	fn connects_only_when_disconnected_or_info_fails() {
		let device = "AA:BB:CC:DD:EE:01";
		let allowed = BTreeSet::from([device.to_string()]);
		for (info, should_connect) in [
			(Ok("Connected: yes\n"), false),
			(Ok("Connected: no\n"), true),
			(Err("bluetoothctl info failed"), true),
		] {
			let mut calls = Vec::new();
			connect_disconnected(&allowed, &AtomicBool::new(false), |args, timeout| {
				calls.push((
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
			assert_eq!(calls, expected);
		}
	}

	#[test]
	fn clap_cli_validates_arguments_and_accepts_completion_shells() {
		for args in [
			vec!["bt-intercom", "run", "invalid"],
			vec!["bt-intercom", "run", "--interval", "NaN"],
			vec!["bt-intercom", "run", "--interval", "0.00000000001"],
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
		let legacy_short = command
			.try_get_matches_from(["bt-intercom", "-V"])
			.err()
			.unwrap()
			.to_string();

		assert_eq!(short, long);
		assert_eq!(short, legacy_short);
		assert!(short.contains(env!("CARGO_PKG_VERSION")));
		assert!(short.contains(env!("BT_INTERCOM_BUILD_DATETIME")));
	}

	#[test]
	fn parses_ptt_without_changing_default_mode() {
		let address = "AA:BB:CC:DD:EE:01".to_string();
		let default = Cli::try_parse_from(["bt-intercom", "run", address.as_str()]).unwrap();
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
		assert_eq!(mode, Mode::FullDuplex);
		assert!(!dashboard);

		let binding = format!("{address}=/dev/input/event4");
		let configured = Cli::try_parse_from([
			"bt-intercom",
			"run",
			"--ptt",
			binding.as_str(),
			address.as_str(),
			"--connect",
			"--dashboard",
		])
		.unwrap();
		let CliCommand::Run {
			addresses,
			connect,
			ptt,
			dashboard,
			..
		} = configured.command
		else {
			panic!("expected run command");
		};
		assert_eq!(addresses, [address]);
		assert!(connect && dashboard);
		assert_eq!(ptt.len(), 1);
		assert_eq!(ptt[0].path, "/dev/input/event4");
		assert_eq!(ptt[0].address, "AA:BB:CC:DD:EE:01");
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
			address: "AA:BB:CC:DD:EE:01".into(),
			path: "/dev/input/event1".into(),
		}];
		assert!(validate_mode(Mode::HalfDuplex, &buttons, &allowed).is_ok());
		let two = BTreeSet::from(["AA:BB:CC:DD:EE:01".into(), "AA:BB:CC:DD:EE:02".into()]);
		assert!(validate_mode(Mode::HalfDuplex, &buttons, &two).is_err());
		let mut command = Cli::command();
		let help = command
			.find_subcommand_mut("run")
			.unwrap()
			.render_long_help()
			.to_string();
		assert!(help.contains("1 second"));
		assert!(help.contains("first to third, inclusive"));
	}

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
					panic!("must not close clients for a successful update or full-duplex warning")
				})
				.is_ok()
			);
		}
	}

	#[test]
	fn confirmations_require_a_granted_source_and_owned_live_route() {
		use serde_json::json;
		let a = "AA:BB:CC:DD:EE:01";
		let b = "AA:BB:CC:DD:EE:02";
		let mut objects = Vec::new();
		for (base, address) in [(10, a), (20, b)] {
			objects.extend([
				json!({"type":"PipeWire:Interface:Device","id":base,"info":{"props":{"api.bluez5.address":address}}}),
				json!({"type":"PipeWire:Interface:Node","id":base+1,"info":{"props":{"device.id":base.to_string(),"media.class":"Audio/Source","api.bluez5.profile":"headset-head-unit"}}}),
				json!({"type":"PipeWire:Interface:Node","id":base+2,"info":{"props":{"device.id":base.to_string(),"media.class":"Audio/Sink","api.bluez5.profile":"headset-head-unit"}}}),
				json!({"type":"PipeWire:Interface:Port","id":base+3,"info":{"props":{"node.id":(base+1).to_string(),"port.direction":"out","audio.channel":"MONO"}}}),
				json!({"type":"PipeWire:Interface:Port","id":base+4,"info":{"props":{"node.id":(base+2).to_string(),"port.direction":"in","audio.channel":"MONO"}}}),
			]);
		}
		let allowed = BTreeSet::from([a.into(), b.into()]);
		let (headsets, _) = router::topology(&json!(objects), &allowed).unwrap();
		let stopped = AtomicBool::new(false);
		let mut transmit = Transmit::new(Mode::HalfDuplex, allowed);
		let now = Instant::now();
		transmit.event(a, true, now);
		transmit.event(b, true, now);
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
		assert_eq!(transmit.pending, BTreeSet::from([a.into()]));
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
		assert_eq!(transmit.pending, BTreeSet::from([a.into()]));
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
		assert_eq!(transmit.pending, BTreeSet::from([a.into()]));
		// Release arriving during routing retires the old grant and defers
		// confirmation of the next source until its own routing update.
		sender
			.send(Ok((a.into(), false, now + Duration::from_millis(1))))
			.unwrap();
		transmit.pending.insert(a.into());
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
		assert_eq!(transmit.sources(), &BTreeSet::from([b.into()]));
		assert_eq!(transmit.pending, BTreeSet::from([b.into()]));
	}

	#[test]
	fn draining_delayed_input_uses_capture_timestamps_not_processing_time() {
		let mut transmit = Transmit::new(Mode::FullDuplex, ["a".into()]);
		let now = Instant::now();
		let (sender, input) = mpsc::channel();
		for ms in [0, 500, 1001] {
			sender
				.send(Ok(("a".into(), true, now + Duration::from_millis(ms))))
				.unwrap();
			sender
				.send(Ok(("a".into(), false, now + Duration::from_millis(ms + 1))))
				.unwrap();
		}
		assert!(drain_ptt(&input, &mut transmit).unwrap());
		assert!(transmit.sources().is_empty());
		assert!(transmit.pending.is_empty());
		assert!(!drain_ptt(&input, &mut transmit).unwrap());
		sender.send(Err("lost input".into())).unwrap();
		assert_eq!(drain_ptt(&input, &mut transmit), Err("lost input".into()));
	}

	#[test]
	fn validates_ptt_bindings_against_restored_network() {
		let first = "AA:BB:CC:DD:EE:01".to_string();
		let second = "AA:BB:CC:DD:EE:02".to_string();
		let network = BTreeSet::from([first.clone(), second.clone()]);
		let buttons = vec![
			PttButton {
				address: first.clone(),
				path: "/dev/input/event4".into(),
			},
			PttButton {
				address: second.clone(),
				path: "/dev/input/event5".into(),
			},
		];
		assert!(validate_ptt(&buttons, &network).is_ok());
		assert!(validate_ptt(&buttons[..1], &network).is_err());
		let duplicate_path = vec![
			buttons[0].clone(),
			PttButton {
				address: second.clone(),
				path: "/dev/input/event4".into(),
			},
		];
		assert!(validate_ptt(&duplicate_path, &network).is_err());
		let duplicate_address = vec![
			buttons[0].clone(),
			PttButton {
				address: first.clone(),
				path: "/dev/input/event5".into(),
			},
		];
		assert!(validate_ptt(&duplicate_address, &network).is_err());
		assert!(validate_ptt(&[buttons[0].clone()], &BTreeSet::from([first, second])).is_err());
	}

	#[test]
	fn creates_a_short_double_beep_waveform() {
		let wav = PTT_BEEP_WAV;
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
		ptt_input_from(std::io::Cursor::new(events), "headset".into(), sender);

		let (address, pressed, press_at) = receiver.recv().unwrap().unwrap();
		assert_eq!((address, pressed), ("headset".into(), true));
		let (address, pressed, release_at) = receiver.recv().unwrap().unwrap();
		assert_eq!((address, pressed), ("headset".into(), false));
		assert!(release_at >= press_at);
		assert!(receiver.recv().unwrap().is_err());
		assert!(matches!(
			receiver.try_recv(),
			Err(TryRecvError::Disconnected)
		));

		let (sender, receiver) = mpsc::channel();
		ptt_input_from(
			std::io::Cursor::new(event(0, 3, 0)),
			"headset".into(),
			sender,
		);
		assert!(receiver.recv().unwrap().is_err());
	}

	#[test]
	fn rejects_intervals_that_round_to_zero() {
		assert!(parse_interval("0.00000000001").is_err());
	}

	#[test]
	fn connection_cancellation_skips_remaining_devices() {
		let stopped = AtomicBool::new(false);
		let allowed = ["AA:BB:CC:DD:EE:01".into(), "AA:BB:CC:DD:EE:02".into()].into();
		let mut calls = 0;
		connect_disconnected(&allowed, &stopped, |args, _| {
			assert_eq!(args[1], "info");
			calls += 1;
			stopped.store(true, Ordering::SeqCst);
			Err("cancelled".into())
		});
		assert_eq!(calls, 1);
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
		let mut calls = Vec::new();
		reconnect_worker(devices, stopped, receiver, Duration::ZERO, |args, _| {
			calls.push(args[2].to_string());
			if calls.len() == 1 {
				updated.lock().unwrap().insert("AA:BB:CC:DD:EE:02".into());
			} else if args[2] == "AA:BB:CC:DD:EE:02" {
				cancelled.store(true, Ordering::SeqCst);
			}
			Ok("Connected: yes\n".into())
		});
		assert_eq!(
			calls,
			[
				"AA:BB:CC:DD:EE:01",
				"AA:BB:CC:DD:EE:01",
				"AA:BB:CC:DD:EE:02"
			]
		);
	}
}
