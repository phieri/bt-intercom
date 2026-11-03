// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Runtime session ownership, event processing, routing, and presentation.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::queue;
use crossterm::style::Print;
use crossterm::terminal::{self, BeginSynchronizedUpdate, Clear, ClearType, EndSynchronizedUpdate};
use unicode_width::UnicodeWidthChar;

use crate::bluez;
use crate::bluez::Address;
use crate::dashboard::{Dashboard, RunStatus};
use crate::network;
use crate::process::{COMMAND_TIMEOUT, command_cancellable};
use crate::ptt::{PreparedInputs, PttBeep, PttInput, drain_events};
use crate::router::Headset;
use crate::runtime_config::RunConfig;
use crate::transmit::{Mode, Transmit};
use crate::transport;
use crate::worker::Worker;

mod routing;
use routing::RoutingCoordinator;
mod reconnect;
#[cfg(test)]
pub(crate) use reconnect::connect_disconnected;
#[cfg(test)]
pub(crate) use reconnect::ensure_adapter_powered;
pub(crate) use reconnect::reconnect_worker;

const ROUTE_CHANGE_RETRY_INTERVAL: Duration = Duration::from_millis(250);
/// How long a PTT request may stay unconfirmed before the user is warned.
const PTT_STALL_WARNING_DELAY: Duration = Duration::from_secs(10);

#[cfg(test)]
type Execute = Box<dyn FnMut(&[&str]) -> Result<String, String>>;
#[cfg(test)]
use crate::router::Router;
#[cfg(test)]
use crate::transport::Transport;

pub(crate) fn guard_half_update<T>(
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

pub(crate) struct Session {
	config: RunConfig,
	stopped: Arc<AtomicBool>,
	routing: RoutingCoordinator,
	transmit: Transmit,
	input: Option<PttInput>,
	dashboard: Option<Dashboard>,
	connector: Option<Worker>,
	beep: Option<PttBeep>,
	wake: Receiver<()>,
	wake_sender: SyncSender<()>,
	last_error: Option<String>,
	last_duplex_count: Option<usize>,
	last_warning: Option<String>,
	pending_since: Option<Instant>,
	stall_warned: bool,
	last_update: Option<Instant>,
	update_interval: Duration,
	redraw: bool,
}

impl Session {
	pub fn start(config: RunConfig) -> Result<Self, String> {
		let mut session = Self::new(config, Arc::new(AtomicBool::new(false)));
		session.setup()?;
		Ok(session)
	}

	fn new(config: RunConfig, stopped: Arc<AtomicBool>) -> Self {
		let (wake_sender, wake) = mpsc::sync_channel(1);
		let update_interval = config.interval;
		let routing = RoutingCoordinator::new(&config, &stopped);
		let mut transmit = Transmit::new(
			config.mode,
			config
				.buttons
				.iter()
				.filter_map(|button| button.address.clone()),
		);
		if config.host.is_some() {
			transmit = transmit.with_host();
		}
		Self {
			config,
			stopped,
			routing,
			transmit,
			input: None,
			dashboard: None,
			connector: None,
			beep: None,
			wake,
			wake_sender,
			last_error: None,
			last_duplex_count: None,
			last_warning: None,
			pending_since: None,
			stall_warned: false,
			last_update: None,
			update_interval,
			redraw: true,
		}
	}

	fn setup(&mut self) -> Result<(), String> {
		let controllers = command_cancellable(
			&["bluetoothctl", "list"],
			COMMAND_TIMEOUT,
			Some(&self.stopped),
		)
		.ok()
		.map(|output| bluez::controller_addresses(&output));
		if let Some(controllers) = &controllers {
			transport::validate_headset_capacity(
				self.config.allowed.len(),
				controllers.len(),
				self.config.mode,
				self.config.transport,
				self.config.sco_limit,
				self.config.a2dp_limit,
			)?;
		}
		if let Ok(devices) = self.routing.router.transport_devices() {
			transport::validate_controller_headsets(
				&devices,
				self.config.mode,
				self.config.transport,
				self.config.sco_limit,
				self.config.a2dp_limit,
			)?;
		}
		if self.config.mode == Mode::FullDuplex
			&& let Some(controllers) = &controllers
		{
			transport::validate_controller_count(
				self.config.mode,
				controllers.len(),
				self.config.sco_limit,
			)?;
		}
		if self.config.sco_limit > 1 || self.config.a2dp_limit > 1 {
			log::warn!(
				"Assuming up to {} SCO/eSCO link(s) and {} A2DP stream(s) per Bluetooth controller; verify the capacity of your hardware",
				self.config.sco_limit,
				self.config.a2dp_limit
			);
		}
		if self.config.connect
			&& let Some(controllers) = &controllers
			&& controllers.len() > 1
		{
			log::warn!(
				"Multiple Bluetooth controllers detected: automatic reconnection addresses only BlueZ's default controller (use --no-connect to disable it). Keep other controllers' headsets connected using an adapter-aware Bluetooth manager, or connect them in a bluetoothctl session after select CONTROLLER_MAC."
			);
		}
		if self.config.allowed.len() < 2 && self.config.host.is_none() {
			log::warn!(
				"Only one headset is configured, so there is nothing to route; add another headset address (or --host) and restart"
			);
		}
		// Advisory only; never blocks startup.
		let findings = crate::diagnostics::diagnose_startup(
			&self.config.allowed,
			self.config.mode,
			self.config.transport,
			self.config.sco_limit,
			self.config.a2dp_limit,
			|args| command_cancellable(args, COMMAND_TIMEOUT, Some(&self.stopped)),
		);
		crate::diagnostics::log_findings(&findings);
		// Validate all device opens before starting any worker or saving the network.
		let inputs = PreparedInputs::open(
			&self.config.buttons,
			self.config.host.as_ref().map(|host| &host.keyboards),
		)?;
		let signal = Arc::clone(&self.stopped);
		let wake = self.wake_sender.clone();
		ctrlc::set_handler(move || {
			signal.store(true, Ordering::SeqCst);
			let _ = wake.try_send(());
		})
		.map_err(|error| error.to_string())?;
		if self.config.explicit_network {
			network::save(&self.config.network_path, &self.config.allowed)?;
		}
		self.beep = match PttBeep::new() {
			Ok(beep) => Some(beep),
			Err(error) => {
				log::warn!("Confirmation beeps unavailable: {error}");
				None
			}
		};
		if !self.config.buttons.is_empty() {
			self.input = Some(inputs.start(Arc::clone(&self.stopped), self.wake_sender.clone()));
			match self.config.mode {
				Mode::HalfDuplex => log::info!(
					"Half-duplex: Tap play to request the FIFO floor; tap pause to cancel or relinquish. Double beep confirms a ready route."
				),
				Mode::FullDuplex => log::info!(
					"Full-duplex: Tap play to transmit; tap pause to mute. A single toggling play/pause key alternates on each tap."
				),
			}
		}
		if self.config.host.is_some() {
			log::info!(
				"Host: hold the spacebar to talk through this computer's wired headset; it shares the {} with the Bluetooth headsets.",
				match self.config.mode {
					Mode::HalfDuplex => "FIFO floor",
					Mode::FullDuplex => "intercom",
				}
			);
		}
		if self.config.dashboard {
			self.dashboard = Some(Dashboard::start(
				self.config.allowed.clone(),
				Arc::clone(&self.stopped),
			));
		}
		if self.config.connect {
			let devices = self.config.allowed.clone();
			self.connector = Some(Worker::spawn(
				Arc::clone(&self.stopped),
				move |stopped, wake| {
					reconnect_worker(
						devices,
						Arc::clone(&stopped),
						wake,
						Duration::from_secs(30),
						|args, timeout| command_cancellable(args, timeout, Some(&stopped)),
					);
				},
			));
		}
		Ok(())
	}

	pub fn run(mut self) -> Result<(), String> {
		while !self.stopped.load(Ordering::SeqCst) {
			if let Err(error) = self.tick() {
				if self.stopped.load(Ordering::SeqCst) {
					return Ok(());
				}
				return Err(error);
			}
			let _ = self
				.wake
				.recv_timeout(Duration::from_millis(100).min(self.config.interval));
		}
		Ok(())
	}

	fn tick(&mut self) -> Result<(), String> {
		self.poll_events()?;
		if self
			.last_update
			.is_none_or(|updated| updated.elapsed() >= self.update_interval)
		{
			self.update_routes()?;
		}
		self.render()
	}

	fn poll_events(&mut self) -> Result<(), String> {
		if let Some(input) = &self.input
			&& drain_events(&input.events, &mut self.transmit)?
		{
			self.last_update = None;
			self.redraw = true;
		}
		Ok(())
	}

	fn update_routes(&mut self) -> Result<(), String> {
		let outcome = self.routing.update(
			&mut self.config,
			&mut self.transmit,
			self.input.as_ref(),
			self.beep.as_ref(),
			&self.stopped,
		)?;
		self.warn_dropped_requests();
		self.warn_stalled_requests();
		if outcome.invalidated {
			self.last_update = Some(Instant::now());
			self.update_interval = ROUTE_CHANGE_RETRY_INTERVAL;
			return Ok(());
		}
		self.report_update(outcome.update, outcome.inspect_error);
		self.last_update = Some(Instant::now());
		// Links and profile changes appear asynchronously, so poll quickly until
		// every requested transmission is confirmed.
		self.update_interval = if outcome.changed || self.transmit.has_pending() {
			ROUTE_CHANGE_RETRY_INTERVAL
		} else {
			self.config.interval
		};
		Ok(())
	}

	fn warn_dropped_requests(&mut self) {
		for address in self.transmit.take_dropped() {
			log::warn!(
				"PTT press from {address} ignored: the headset is not connected or lacks a headset-head-unit (HFP/HSP) microphone and speaker in PipeWire; connect it, then press again"
			);
		}
		if self.transmit.take_host_dropped() {
			log::warn!(
				"Host push-to-talk ignored: the wired headset's microphone and speakers are unavailable in PipeWire; connect it (or pass --host-source/--host-sink), then press space again"
			);
		}
	}

	/// A held button that never reaches a confirmed route usually means a
	/// configuration problem rather than a transient transition.
	fn warn_stalled_requests(&mut self) {
		if !self.transmit.has_pending() {
			self.pending_since = None;
			self.stall_warned = false;
			return;
		}
		let since = *self.pending_since.get_or_insert_with(Instant::now);
		if !self.stall_warned && since.elapsed() >= PTT_STALL_WARNING_DELAY {
			self.stall_warned = true;
			let pending: Vec<_> = self
				.transmit
				.host_pending
				.then(|| "the host".to_string())
				.into_iter()
				.chain(self.transmit.pending.iter().map(|a| a.to_string()))
				.collect();
			log::warn!(
				"PTT from {} is not transmitting after {} s: it needs at least one other connected headset with a speaker, working PipeWire (`pw-dump`, `pw-cli`) and, with --transport sco-a2dp, an HFP profile that WirePlumber exposes; run `bt-intercom diagnose` for details",
				pending.join(", "),
				PTT_STALL_WARNING_DELAY.as_secs()
			);
		}
	}

	fn warn_once(&mut self, message: String) {
		if self.last_warning.as_deref() != Some(&message) {
			log::warn!("{message}");
			self.last_warning = Some(message);
		}
	}

	fn report_update(
		&mut self,
		update: Result<BTreeMap<Address, Headset>, String>,
		inspect_error: Option<String>,
	) {
		if self.dashboard.is_some() {
			self.last_error = update.err().or(inspect_error);
			self.redraw = true;
		} else {
			if let Some(error) = inspect_error {
				self.warn_once(format!("Routing inspection failed: {error}"));
			}
			match update {
				Ok(headsets) => {
					let active = headsets
						.values()
						.filter(|headset| headset.has_duplex_audio())
						.count();
					// Half-duplex keeps only the talker on a duplex profile, so the count is not a readiness signal there.
					if self.config.mode == Mode::FullDuplex
						&& self.last_duplex_count.replace(active) != Some(active)
					{
						log::info!(
							"{active}/{} headsets with duplex audio",
							self.routing.router.allowed.len()
						);
					}
				}
				Err(error) => self.warn_once(format!("Routing update failed: {error}")),
			}
		}
	}

	fn render(&mut self) -> Result<(), String> {
		let Some(dashboard) = &mut self.dashboard else {
			return Ok(());
		};
		self.redraw |= dashboard.refresh();
		if self.redraw {
			let stderr = std::io::stderr();
			let interactive = stderr.is_terminal();
			let mut frame = Vec::new();
			dashboard
				.draw(
					&self.routing.router.allowed,
					&self.routing.headsets,
					&self.routing.links,
					RunStatus {
						mode: self.config.mode,
						transmit: &self.transmit,
						host: self.routing.host.as_ref(),
					},
					self.last_error.as_deref(),
					&mut frame,
				)
				.map_err(|error| error.to_string())?;
			let mut output = stderr.lock();
			if interactive {
				let width = terminal::size()
					.ok()
					.map(|(columns, _)| usize::from(columns));
				let text = interactive_frame(&String::from_utf8_lossy(&frame), width)
					.map_err(|error| error.to_string())?;
				output.write_all(&text).map_err(|error| error.to_string())?;
			} else {
				output
					.write_all(&frame)
					.map_err(|error| error.to_string())?;
			}
			output.flush().map_err(|error| error.to_string())?;
			self.redraw = false;
		}
		Ok(())
	}
}

/// Truncates a line to at most `width` terminal columns.
fn fit_line(line: &str, width: usize) -> &str {
	let mut used = 0;
	for (index, ch) in line.char_indices() {
		used += ch.width().unwrap_or(0);
		if used > width {
			return &line[..index];
		}
	}
	line
}

/// Builds one atomic in-place frame: lines are clipped to the terminal width so
/// wrapping cannot desynchronize the cursor, and leftovers are erased.
fn interactive_frame(frame: &str, width: Option<usize>) -> io::Result<Vec<u8>> {
	let mut out = Vec::new();
	queue!(out, BeginSynchronizedUpdate, MoveTo(0, 0))?;
	for line in frame.lines() {
		let line = width.map_or(line, |width| fit_line(line, width));
		queue!(
			out,
			Print(line),
			Clear(ClearType::UntilNewLine),
			Print("\r\n")
		)?;
	}
	queue!(out, Clear(ClearType::FromCursorDown), EndSynchronizedUpdate)?;
	Ok(out)
}

impl Drop for Session {
	fn drop(&mut self) {
		self.stopped.store(true, Ordering::SeqCst);
		self.routing.router.close();
		if let Some(input) = &mut self.input {
			input.stop();
		}
		if let Some(dashboard) = &mut self.dashboard {
			dashboard.stop();
		}
		if let Some(connector) = &mut self.connector {
			connector.stop();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn fit_line_clips_by_display_width() {
		assert_eq!(fit_line("abcdef", 3), "abc");
		assert_eq!(fit_line("abc", 10), "abc");
		assert_eq!(fit_line("日本語", 5), "日本");
	}

	#[test]
	fn interactive_frame_clips_and_clears() {
		let out = String::from_utf8(interactive_frame("abcdef\nxy\n", Some(3)).unwrap()).unwrap();
		assert!(out.contains("abc\x1b[K\r\n"));
		assert!(!out.contains("abcd"));
		assert!(out.contains("\x1b[J"));
	}

	use crate::ptt::PttButton;
	use crate::transmit::Tap;
	use std::sync::mpsc;

	fn config() -> RunConfig {
		let network_path =
			std::env::temp_dir().join(format!("session-unused-network-{}", std::process::id()));
		RunConfig {
			allowed: ["AA:BB:CC:DD:EE:01".into()].into(),
			interval: Duration::from_millis(1),
			connect: false,
			buttons: vec![PttButton {
				address: Some(Address::parse("AA:BB:CC:DD:EE:01").unwrap()),
				path: "injected-input".into(),
			}],
			mode: Mode::HalfDuplex,
			transport: Transport::Hfp,
			sco_limit: 1,
			a2dp_limit: 1,
			dashboard: false,
			host: None,
			explicit_network: true,
			network_path,
		}
	}

	#[test]
	fn routing_failure_cancels_and_joins_retained_workers() {
		let stopped = Arc::new(AtomicBool::new(false));
		let mut session = Session::new(config(), Arc::clone(&stopped));
		let execute: Execute = Box::new(|_| Err("injected snapshot failure".into()));
		session.routing.router = Router::with_executor(session.config.allowed.clone(), execute);
		let (done, completed) = mpsc::channel();
		session.connector = Some(Worker::spawn(Arc::clone(&stopped), move |_, wake| {
			let _ = wake.recv();
			done.send(()).unwrap();
		}));
		assert!(session.run().unwrap_err().contains("snapshot failure"));
		assert!(stopped.load(Ordering::SeqCst));
		completed.try_recv().unwrap();
	}

	#[test]
	fn cancellation_during_a_tick_is_a_clean_shutdown() {
		let stopped = Arc::new(AtomicBool::new(false));
		let mut session = Session::new(config(), Arc::clone(&stopped));
		let cancellation = Arc::clone(&stopped);
		let execute: Execute = Box::new(move |_| {
			cancellation.store(true, Ordering::SeqCst);
			Err("cancelled".into())
		});
		session.routing.router = Router::with_executor(session.config.allowed.clone(), execute);
		assert!(session.run().is_ok());
		assert!(stopped.load(Ordering::SeqCst));
	}

	#[test]
	fn released_input_during_routing_invalidates_routes_and_schedules_retry() {
		let address = "AA:BB:CC:DD:EE:01";
		let snapshot = serde_json::json!([
			{"type":"PipeWire:Interface:Device", "id":10,
				"info":{"props":{"api.bluez5.address":address}}},
			{"type":"PipeWire:Interface:Node", "id":11,
				"info":{"props":{"device.id":10, "media.class":"Audio/Source",
					"api.bluez5.profile":"headset-head-unit"}}},
			{"type":"PipeWire:Interface:Node", "id":12,
				"info":{"props":{"device.id":10, "media.class":"Audio/Sink",
					"api.bluez5.profile":"headset-head-unit"}}},
			{"type":"PipeWire:Interface:Port", "id":13,
				"info":{"props":{"node.id":11, "port.direction":"out", "audio.channel":"MONO"}}},
			{"type":"PipeWire:Interface:Port", "id":14,
				"info":{"props":{"node.id":12, "port.direction":"in", "audio.channel":"MONO"}}}
		])
		.to_string();
		let (sender, receiver) = mpsc::channel();
		let mut session = Session::new(config(), Arc::new(AtomicBool::new(false)));
		session.input = Some(PttInput::from_events(receiver));
		session.transmit.topology(&[address.into()].into());
		assert!(
			session
				.transmit
				.event(&Address::parse(address).unwrap(), true, Instant::now())
		);
		session.last_update = Some(Instant::now());
		let mut snapshots = 0;
		let execute: Execute = Box::new(move |args| {
			assert_eq!(args, ["pw-dump"]);
			snapshots += 1;
			if snapshots == 3 {
				sender
					.send(Ok((
						crate::transmit::Talker::Headset(address.parse().unwrap()),
						Tap::Pause,
						Instant::now(),
					)))
					.unwrap();
			}
			Ok(snapshot.clone())
		});
		session.routing.router = Router::with_executor(session.config.allowed.clone(), execute);
		session.update_routes().unwrap();
		assert!(session.transmit.sources().is_empty());
		assert!(session.transmit.pending.is_empty());
		assert!(session.routing.links.is_empty());
		assert!(session.last_update.is_some());
		assert_eq!(session.update_interval, ROUTE_CHANGE_RETRY_INTERVAL);
	}

	#[test]
	fn changed_routes_wait_for_the_retry_interval_before_another_snapshot() {
		let mut session = Session::new(config(), Arc::new(AtomicBool::new(false)));
		session.last_update = Some(Instant::now());
		session.update_interval = ROUTE_CHANGE_RETRY_INTERVAL;
		let execute: Execute = Box::new(|_| panic!("route retry should be throttled"));
		session.routing.router = Router::with_executor(session.config.allowed.clone(), execute);

		session.tick().unwrap();
	}

	#[test]
	fn input_open_failure_does_not_save_network_or_start_workers() {
		let mut config = config();
		config.buttons = vec![PttButton {
			address: Some(Address::parse("AA:BB:CC:DD:EE:01").unwrap()),
			path: config
				.network_path
				.join("missing-input")
				.display()
				.to_string(),
		}];
		let path = config.network_path.clone();
		let mut session = Session::new(config, Arc::new(AtomicBool::new(false)));
		assert!(session.setup().unwrap_err().starts_with("PTT input"));
		assert!(session.input.is_none());
		assert!(session.connector.is_none());
		assert!(session.dashboard.is_none());
		assert!(!path.exists());
	}
}
