// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Command-line entry point for Bluetooth setup, status, and intercom routing.
//!
//! External commands are bounded by timeouts; the long-running `run` mode also
//! supports cancellation, reconnecting, push-to-talk input, and a terminal view.

mod atomic_file;
mod bluez;
mod dashboard;
mod groups;
mod router;
mod transmit;

mod tui;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::ffi::OsStr;
use std::fs::{self, File, OpenOptions};
use std::io::{IsTerminal, Read, Write};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use bluez::{bluetooth_name, device_flag};
use clap::{CommandFactory, Parser, Subcommand};
use clap_complete::Shell;
use dashboard::Dashboard;
use groups::{config_path as talk_groups_path, load as load_talk_groups};
use router::{Headset, Router, has_active_intercom_connection, has_active_source_route};
use transmit::{Mode, Transmit};

const PTT_BEEP_WAV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ptt-beep.wav"));

#[derive(Clone, Debug)]
struct PttButton {
	address: String,
	path: String,
}

#[derive(Parser)]
#[command(
	name = "bt-intercom",
	version,
	long_version = concat!(
		env!("CARGO_PKG_VERSION"),
		"\nBuild datetime: ",
		env!("BT_INTERCOM_BUILD_DATETIME")
	),
	about = "A full-duplex Bluetooth headset intercom for Linux",
	arg_required_else_help = true
)]
struct Cli {
	#[command(subcommand)]
	command: CliCommand,
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
		/// Map a headset address to an evdev input path; repeat once per headset.
		#[arg(long, value_name = "ADDRESS=/dev/input/eventX", value_parser = parse_ptt_binding)]
		ptt: Vec<PttButton>,
		/// Transmit policy. Semi-duplex requires --ptt for every headset:
		/// hold play/pause to request the FIFO floor; release to cancel/relinquish.
		/// Full-duplex mappings start in PTT; three play/pause presses within
		/// 1 second (first to third, inclusive; release between presses) toggle
		/// that headset independently between PTT and always open.
		/// Without mappings full-duplex is always open. Disconnected mapped
		/// headsets reset to PTT and require a fresh press after reconnection.
		#[arg(long, value_enum, default_value_t = Mode::FullDuplex)]
		mode: Mode,
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

/// Runs a command with captured output and a deadline.
fn command(args: &[&str], timeout: Duration) -> Result<String, String> {
	command_cancellable(args, timeout, None)
}

/// Which process(es) `wait_child` should terminate when the command times
/// out, is cancelled, or errors while polling.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KillMode {
	/// Kill only the direct child (used for interactive children that share
	/// the caller's controlling terminal, where grouping could trigger
	/// `SIGTTIN`/job-control issues).
	Process,
	/// Kill the child's entire process group, releasing pipes held open by
	/// descendants. Only valid for children spawned with `process_group(0)`.
	ProcessGroup,
}

/// Waits for a child process, enforcing its timeout and optional cancellation.
fn wait_child(
	child: &mut Child,
	name: &str,
	timeout: Duration,
	stopped: Option<&AtomicBool>,
	kill_mode: KillMode,
) -> Result<ExitStatus, String> {
	let start = Instant::now();
	loop {
		let cancelled = stopped.is_some_and(|flag| flag.load(Ordering::SeqCst));
		let error = match child.try_wait() {
			Ok(Some(status)) => return Ok(status),
			Ok(None) if cancelled => format!("{name} cancelled"),
			Ok(None) if start.elapsed() >= timeout => format!("{name} timed out"),
			Ok(None) => {
				thread::sleep(Duration::from_millis(20));
				continue;
			}
			Err(error) => error.to_string(),
		};
		match kill_mode {
			KillMode::ProcessGroup => kill_process_group(child),
			KillMode::Process => {
				let _ = child.kill();
			}
		}
		let _ = child.wait();
		return Err(error);
	}
}

/// Kills the child's entire process group, so descendants that keep the
/// child's inherited pipes open (e.g. backgrounded subprocesses) are also
/// terminated and release those pipes. Only valid for children spawned with
/// `process_group(0)`, which makes the child the leader of its own group.
fn kill_process_group(child: &mut Child) {
	let pid = child.id() as libc::pid_t;
	if pid <= 0 {
		return;
	}
	// SAFETY: `pid` is the child's own process ID, which was placed in its
	// own process group via `process_group(0)` when spawned, so `-pid`
	// refers to a valid process group led by that child.
	unsafe {
		libc::kill(-pid, libc::SIGKILL);
	}
}

/// Runs a cancellable command, draining both output streams while it executes.
///
/// Commands run in their own process group so timeout or cancellation also
/// terminates descendants that might otherwise keep the captured pipes open.
fn command_cancellable(
	args: &[&str],
	timeout: Duration,
	stopped: Option<&AtomicBool>,
) -> Result<String, String> {
	let Some(name) = args.first().copied().filter(|name| !name.is_empty()) else {
		return Err("command arguments must include a program".into());
	};
	if stopped.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
		return Err(format!("{name} cancelled"));
	}
	let mut child = Command::new(name)
		.args(&args[1..])
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.process_group(0)
		.spawn()
		.map_err(|e| format!("{name}: {e}"))?;
	let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
		kill_process_group(&mut child);
		let _ = child.wait();
		return Err(format!("{name}: failed to capture command output"));
	};
	let output = thread::spawn(move || {
		let mut bytes = Vec::new();
		stdout.read_to_end(&mut bytes).map(|_| bytes)
	});
	let error = thread::spawn(move || {
		let mut bytes = Vec::new();
		stderr.read_to_end(&mut bytes).map(|_| bytes)
	});
	let status = wait_child(&mut child, name, timeout, stopped, KillMode::ProcessGroup)?;
	let stdout = output
		.join()
		.map_err(|_| "stdout reader panicked".to_string())?
		.map_err(|e| e.to_string())?;
	let stderr = error
		.join()
		.map_err(|_| "stderr reader panicked".to_string())?
		.map_err(|e| e.to_string())?;
	if !status.success() {
		return Err(format!(
			"{} exited with {status}: {}",
			name,
			String::from_utf8_lossy(&stderr).trim()
		));
	}
	Ok(String::from_utf8_lossy(&stdout).into_owned())
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
	let status = wait_child(
		&mut child,
		"bluetoothctl",
		Duration::from_secs(300),
		None,
		KillMode::Process,
	)?;
	if status.success() {
		Ok(())
	} else {
		Err(format!("bluetoothctl pair exited with {status}"))
	}
}

/// Validates and normalizes a Bluetooth MAC address.
fn address(value: &str) -> Result<String, String> {
	if value.len() != 17
		|| value.split(':').count() != 6
		|| !value
			.split(':')
			.all(|part| part.len() == 2 && part.bytes().all(|b| b.is_ascii_hexdigit()))
	{
		return Err(format!(
			"expected a Bluetooth MAC address (XX:XX:XX:XX:XX:XX): {value}"
		));
	}
	Ok(value.to_ascii_uppercase())
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

/// Returns the path used to remember the configured intercom network.
fn headset_network_path() -> Result<PathBuf, String> {
	let directory = env::var_os("XDG_CONFIG_HOME")
		.map(PathBuf::from)
		.filter(|path| path.is_absolute())
		.or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
		.ok_or("could not determine the config directory; set XDG_CONFIG_HOME or HOME")?;
	Ok(directory.join("bt-intercom").join("headsets"))
}

/// Returns the session runtime directory, falling back to the system temp directory.
fn ptt_runtime_directory(xdg_runtime_dir: Option<&OsStr>) -> PathBuf {
	xdg_runtime_dir
		.map(PathBuf::from)
		.filter(|path| path.is_absolute())
		.unwrap_or_else(env::temp_dir)
}

/// Atomically saves the allowed headset addresses for the next run.
fn save_headsets(path: &Path, allowed: &BTreeSet<String>) -> Result<(), String> {
	if allowed.is_empty() {
		return Err("cannot save an empty headset network".into());
	}
	let directory = path
		.parent()
		.ok_or_else(|| format!("invalid headset network path: {}", path.display()))?;
	fs::create_dir_all(directory)
		.map_err(|error| format!("could not create {}: {error}", directory.display()))?;

	let contents = format!(
		"{}\n",
		allowed.iter().cloned().collect::<Vec<_>>().join("\n")
	);
	atomic_file::write(path, contents.as_bytes()).map_err(|error| {
		format!(
			"could not save headset network to {}: {error}",
			path.display()
		)
	})?;
	Ok(())
}

/// Removes an address from the saved headset network without changing Bluetooth pairing.
fn remove_headset(path: &Path, address: &str) -> Result<(), String> {
	let mut allowed = load_headsets(path)?;
	if !allowed.remove(address) {
		return Err(format!("{address} is not in the saved headset network"));
	}
	if allowed.is_empty() {
		fs::remove_file(path).map_err(|error| {
			format!(
				"could not remove saved headset network {}: {error}",
				path.display()
			)
		})?;
	} else {
		save_headsets(path, &allowed)?;
	}
	Ok(())
}

/// Loads and validates the headset addresses saved by an earlier run.
fn load_headsets(path: &Path) -> Result<BTreeSet<String>, String> {
	let contents = fs::read_to_string(path).map_err(|error| {
		if error.kind() == std::io::ErrorKind::NotFound {
			format!(
				"no saved headset network at {}; run with headset addresses first",
				path.display()
			)
		} else {
			format!(
				"could not read headset network from {}: {error}",
				path.display()
			)
		}
	})?;
	let mut allowed = BTreeSet::new();
	for (index, line) in contents.lines().enumerate() {
		let line = line.trim();
		if !line.is_empty() {
			allowed.insert(address(line).map_err(|error| {
				format!(
					"invalid headset network at {}:{}: {error}",
					path.display(),
					index + 1
				)
			})?);
		}
	}
	if allowed.is_empty() {
		return Err(format!(
			"saved headset network at {} is empty",
			path.display()
		));
	}
	Ok(allowed)
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
	let ready = transmit
		.pending
		.iter()
		.filter_map(|address| {
			let headset = headsets.get(address)?;
			(transmit.sources().contains(address)
				&& has_active_source_route(address, headsets, links))
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
			if mode == Mode::SemiDuplex {
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
			Err(error) if mode == Mode::SemiDuplex => {
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
	connected: &mut BTreeSet<String>,
	headsets: &BTreeMap<String, Headset>,
	links: &BTreeSet<(u64, u64)>,
	beep: Option<&PttBeep>,
	stopped: &AtomicBool,
) {
	let current: BTreeSet<_> = headsets
		.keys()
		.filter(|address| has_active_intercom_connection(address, headsets, links))
		.cloned()
		.collect();
	for address in current.difference(connected) {
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
	*connected = current;
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
	if mode == Mode::SemiDuplex && buttons.is_empty() {
		return Err("--mode semi-duplex requires --ptt for every headset".into());
	}
	Ok(())
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
/// Fail closed before inspection/confirmation in semi-duplex; preserve the
/// existing non-fatal update warnings in full-duplex.
fn guard_semi_update<T>(
	mode: Mode,
	update: &Result<T, String>,
	close_owned: impl FnOnce(),
) -> Result<(), String> {
	if mode == Mode::SemiDuplex
		&& let Err(error) = update
	{
		close_owned();
		return Err(format!(
			"Semi-duplex routing failed; released owned links and stopped to prevent an unsafe floor handoff: {error}"
		));
	}
	Ok(())
}

/// Reads evdev records, forwarding only play/pause press and release transitions.
fn ptt_input_from<R: Read>(mut input: R, address: String, sender: mpsc::Sender<PttEvent>) {
	let mut pressed = false;
	let mut event = vec![0; std::mem::size_of::<libc::timeval>() + 8];
	let offset = std::mem::size_of::<libc::timeval>();
	loop {
		if let Err(error) = input.read_exact(&mut event) {
			let _ = sender.send(Err(format!("PTT input for {address} closed: {error}")));
			break;
		}
		let at = Instant::now();
		let event_type = u16::from_ne_bytes([event[offset], event[offset + 1]]);
		let event_key = u16::from_ne_bytes([event[offset + 2], event[offset + 3]]);
		let value = i32::from_ne_bytes(event[offset + 4..offset + 8].try_into().unwrap());
		if event_type == 0 && event_key == 3 {
			let _ = sender.send(Err(format!("PTT input for {address} lost button events")));
			break;
		}
		if event_type == 1 && event_key == KEY_PLAYPAUSE && (value == 0 || value == 1) {
			let next = value == 1;
			if next != pressed {
				pressed = next;
				if sender.send(Ok((address.clone(), pressed, at))).is_err() {
					break;
				}
			}
		}
	}
}

/// Opens each configured PTT device and starts one event-reading worker per device.
fn ptt_input(buttons: Vec<PttButton>) -> Result<Receiver<PttEvent>, String> {
	let inputs = buttons
		.into_iter()
		.map(|button| {
			File::open(&button.path)
				.map(|file| (button, file))
				.map_err(|error| format!("PTT input: {error}"))
		})
		.collect::<Result<Vec<_>, _>>()?;
	let (sender, receiver) = mpsc::channel();
	for (button, file) in inputs {
		let sender = sender.clone();
		thread::spawn(move || ptt_input_from(file, button.address, sender));
	}
	Ok(receiver)
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
		let connected = execute(&["bluetoothctl", "info", device], Duration::from_secs(15))
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
	allowed: BTreeSet<String>,
	stopped: Arc<AtomicBool>,
	shutdown: Receiver<()>,
	retry: Duration,
	mut execute: impl FnMut(&[&str], Duration) -> Result<String, String>,
) {
	loop {
		connect_disconnected(&allowed, &stopped, &mut execute);
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
				&command(&["bluetoothctl", "info", &device], Duration::from_secs(15))?,
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
			for address in &router.allowed {
				let name = command(&["bluetoothctl", "info", address], Duration::from_secs(15))
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
			}
		}
		CliCommand::Run {
			addresses: devices,
			interval,
			connect,
			ptt: buttons,
			mode,
			dashboard: show_dashboard,
		} => {
			let mut allowed = devices.into_iter().collect();
			validate_mode(mode, &buttons, &allowed)?;
			let interval = Duration::from_secs_f64(interval);
			if show_dashboard && !std::io::stderr().is_terminal() {
				return Err("--dashboard requires an interactive terminal on stderr".into());
			}
			let explicit_network = !allowed.is_empty();
			let network_path = headset_network_path()?;
			if explicit_network {
				save_headsets(&network_path, &allowed)?;
			} else {
				allowed = load_headsets(&network_path)?;
				validate_mode(mode, &buttons, &allowed)?;
			}
			let groups_path = talk_groups_path(&network_path)?;
			let mut groups = load_talk_groups(&groups_path)?;
			let stopped = Arc::new(AtomicBool::new(false));
			let signal = Arc::clone(&stopped);
			ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
				.map_err(|e| e.to_string())?;
			let mut transmit =
				Transmit::new(mode, buttons.iter().map(|button| button.address.clone()));
			transmit.set_groups(&groups);
			// Open all inputs before any background workers start. A startup
			// failure must not leave a connector or dashboard running.
			let input = if buttons.is_empty() {
				None
			} else {
				Some(ptt_input(buttons)?)
			};
			let mut router = Router::new(allowed);
			let mut dashboard = show_dashboard
				.then(|| Dashboard::start(router.allowed.clone(), Arc::clone(&stopped)));
			let mut headsets = BTreeMap::new();
			let mut links = BTreeSet::new();
			let mut last_error = None;
			let mut redraw = true;
			let (shutdown, shutdown_rx) = mpsc::channel();
			let connector = if connect {
				let devices = router.allowed.clone();
				let cancelled = Arc::clone(&stopped);
				Some(thread::spawn(move || {
					reconnect_worker(
						devices,
						Arc::clone(&cancelled),
						shutdown_rx,
						Duration::from_secs(30),
						|args, timeout| command_cancellable(args, timeout, Some(&cancelled)),
					);
				}))
			} else {
				None
			};
			let beep = match PttBeep::new() {
				Ok(beep) => Some(beep),
				Err(error) => {
					log::warn!("Confirmation beeps unavailable: {error}");
					None
				}
			};
			let mut connected_headsets = BTreeSet::new();
			if input.is_some() {
				match mode {
					Mode::SemiDuplex => log::info!(
						"Semi-duplex: Hold play/pause to request the FIFO floor; release to cancel or relinquish. Double beep confirms a ready route."
					),
					Mode::FullDuplex => log::info!(
						"Full-duplex: Hold play/pause to transmit; release to mute. Three presses within 1 second (first to third, inclusive) toggle this headset's PTT/always-open mode."
					),
				}
			}
			let mut last_update: Option<Instant> = None;
			let result: Result<(), String> = (|| {
				while !stopped.load(Ordering::SeqCst) {
					if let Some(ref input) = input
						&& drain_ptt(input, &mut transmit)?
					{
						last_update = None;
						redraw = true;
					}
					if last_update.is_none_or(|updated| updated.elapsed() >= interval) {
						let config_error = match load_talk_groups(&groups_path) {
							Ok(current) => {
								transmit.set_groups(&current);
								groups = current;
								None
							}
							Err(error) => Some(error),
						};
						// Retire a disconnected floor/queued request before selecting
						// the next sources. Only successful snapshots prove absence.
						if let Some(ref input) = input {
							let snapshot = router.inspect_owned();
							drain_ptt(input, &mut transmit)?;
							if let Ok((current, _)) = snapshot {
								let available = current
									.iter()
									.filter(|(_, headset)| headset.has_duplex_audio())
									.map(|(address, _)| address.clone())
									.collect();
								transmit.topology(&available);
							}
						}
						let sources = if input.is_some() {
							transmit.sources().clone()
						} else {
							router.allowed.clone()
						};
						let update = if groups.is_empty() && input.is_none() {
							router.update(true)
						} else if mode == Mode::SemiDuplex && !groups.is_empty() {
							router.update_group_sources_in_groups(transmit.group_sources(), &groups)
						} else {
							router.update_sources_in_groups(&sources, &groups)
						};
						guard_semi_update(mode, &update, || router.close())?;
						let mut inspect_error = None;
						let mut changed = false;
						match router.inspect_owned() {
							Ok((current, active)) => {
								headsets = current;
								links = active;
								if let Some(ref input) = input {
									let available = headsets
										.iter()
										.filter(|(_, headset)| headset.has_duplex_audio())
										.map(|(address, _)| address.clone())
										.collect();
									changed |= drain_ptt(input, &mut transmit)?;
									changed |= transmit.topology(&available);
									if !changed {
										changed |= confirm_transmissions(
											mode,
											&mut transmit,
											input,
											&headsets,
											&links,
											beep.as_ref(),
											&stopped,
										)?;
									}
								} else {
									confirm_connections(
										&mut connected_headsets,
										&headsets,
										&links,
										beep.as_ref(),
										&stopped,
									);
								}
							}
							Err(error) => {
								headsets.clear();
								links.clear();
								inspect_error = Some(error);
							}
						}
						if dashboard.is_some() {
							last_error = config_error.or_else(|| update.err()).or(inspect_error);
							redraw = true;
						} else {
							if let Some(error) = inspect_error {
								log::warn!("Routing inspection failed: {error}");
							}
							if let Some(error) = config_error {
								log::warn!("Talk-group configuration unavailable: {error}");
							}
							match update {
								Ok(headsets) => {
									let active = headsets
										.values()
										.filter(|headset| headset.has_duplex_audio())
										.count();
									log::info!(
										"{active}/{} headsets with duplex audio",
										router.allowed.len()
									);
								}
								Err(error) => log::warn!("Routing update failed: {error}"),
							}
						}
						last_update = (!changed).then(Instant::now);
					}
					if let Some(ref mut dashboard) = dashboard {
						redraw |= dashboard.refresh();
						if redraw {
							dashboard
								.draw(
									&router.allowed,
									&headsets,
									&links,
									input.is_none() || !transmit.sources().is_empty(),
									last_error.as_deref(),
									&mut std::io::stderr().lock(),
								)
								.map_err(|error| error.to_string())?;
							redraw = false;
						}
					}
					thread::sleep(Duration::from_millis(100).min(interval));
				}
				Ok(())
			})();
			stopped.store(true, Ordering::SeqCst);
			let _ = shutdown.send(());
			if let Some(ref mut dashboard) = dashboard {
				dashboard.stop();
			}
			if let Some(connector) = connector {
				let _ = connector.join();
			}
			router.close();
			result?;
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

	let cli = Cli::try_parse().unwrap_or_else(|error| error.exit());
	match cli.command {
		CliCommand::Completions { shell } => {
			let mut command = Cli::command();
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

	#[test]
	fn validates_address_before_invoking_commands() {
		assert!(address("invalid; rm -rf /").is_err());
		assert_eq!(address("aa:bb:cc:dd:ee:ff").unwrap(), "AA:BB:CC:DD:EE:FF");
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
				Duration::from_secs(15),
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
	fn long_version_includes_package_version_and_build_datetime() {
		let command = Cli::command();
		assert_eq!(command.get_version(), Some(env!("CARGO_PKG_VERSION")));
		let long_version = command.get_long_version().unwrap().to_string();
		assert!(long_version.contains(env!("CARGO_PKG_VERSION")));
		assert!(long_version.contains(env!("BT_INTERCOM_BUILD_DATETIME")));
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
			("semi-duplex", Mode::SemiDuplex),
			("full-duplex", Mode::FullDuplex),
		] {
			let cli = Cli::try_parse_from(["bt-intercom", "run", "--mode", name]).unwrap();
			let CliCommand::Run { mode, .. } = cli.command else {
				panic!("expected run");
			};
			assert_eq!(mode, expected);
		}
		assert!(Cli::try_parse_from(["bt-intercom", "run", "--mode", "half"]).is_err());
		let allowed = BTreeSet::from(["AA:BB:CC:DD:EE:01".into()]);
		assert!(validate_mode(Mode::FullDuplex, &[], &allowed).is_ok());
		assert!(validate_mode(Mode::SemiDuplex, &[], &allowed).is_err());
		assert!(validate_mode(Mode::SemiDuplex, &[], &BTreeSet::new()).is_err());
		let buttons = [PttButton {
			address: "AA:BB:CC:DD:EE:01".into(),
			path: "/dev/input/event1".into(),
		}];
		assert!(validate_mode(Mode::SemiDuplex, &buttons, &allowed).is_ok());
		let two = BTreeSet::from(["AA:BB:CC:DD:EE:01".into(), "AA:BB:CC:DD:EE:02".into()]);
		assert!(validate_mode(Mode::SemiDuplex, &buttons, &two).is_err());
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
	fn semi_update_failure_closes_owned_clients_before_returning_fatal_error() {
		let mut owned_clients_closed = false;
		let update: Result<(), String> = Err("pw-dump timed out".into());
		let result = guard_semi_update(Mode::SemiDuplex, &update, || {
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
			(Mode::SemiDuplex, Ok(())),
		] {
			assert!(
				guard_semi_update(mode, &update, || {
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
		let mut transmit = Transmit::new(Mode::SemiDuplex, allowed);
		let now = Instant::now();
		transmit.event(a, true, now);
		transmit.event(b, true, now);
		let (sender, input) = mpsc::channel();
		// A queued microphone's live route (or an incoming-only route to the
		// floor holder) must not earn a confirmation.
		confirm_transmissions(
			Mode::SemiDuplex,
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
			Mode::SemiDuplex,
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
				Mode::SemiDuplex,
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
				Mode::SemiDuplex,
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
				["AA:BB:CC:DD:EE:01".into()].into(),
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
	fn commands_capture_output_errors_and_do_not_read_ptt_input() {
		let timeout = Duration::from_secs(5);
		assert_eq!(
			command(&["sh", "-c", "cat; printf output"], timeout).unwrap(),
			"output"
		);
		assert!(
			command(&["sh", "-c", "printf failure >&2; exit 7"], timeout)
				.unwrap_err()
				.contains("failure")
		);
		assert_eq!(
			command(&["head", "-c", "131072", "/dev/zero"], timeout)
				.unwrap()
				.len(),
			131072
		);
	}

	#[test]
	fn commands_reject_missing_program_names() {
		assert!(command(&[], Duration::from_secs(1)).is_err());
		assert!(command(&[""], Duration::from_secs(1)).is_err());
	}

	#[test]
	fn commands_time_out_and_can_be_cancelled() {
		assert!(
			command(&["sleep", "30"], Duration::from_millis(20))
				.unwrap_err()
				.contains("timed out")
		);
		let stopped = Arc::new(AtomicBool::new(false));
		let cancelled = Arc::clone(&stopped);
		let worker = thread::spawn(move || {
			command_cancellable(&["sleep", "30"], Duration::from_secs(60), Some(&cancelled))
		});
		thread::sleep(Duration::from_millis(50));
		stopped.store(true, Ordering::SeqCst);
		assert!(worker.join().unwrap().unwrap_err().contains("cancelled"));
	}

	#[test]
	fn timeout_does_not_wait_for_descendants_holding_output_pipes() {
		let start = Instant::now();
		assert!(
			command(&["sh", "-c", "sleep 2 & wait"], Duration::from_millis(20))
				.unwrap_err()
				.contains("timed out")
		);
		assert!(start.elapsed() < Duration::from_secs(1));
	}
}
