// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Runtime session ownership, event processing, routing, and presentation.

use std::collections::BTreeMap;
use std::io::{IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::execute;

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
pub(crate) use reconnect::reconnect_worker;

const ROUTE_CHANGE_RETRY_INTERVAL: Duration = Duration::from_millis(250);

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
		transmit.set_sco_limit(config.sco_limit);
		transmit.set_groups(&config.groups);
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
			last_update: None,
			update_interval,
			redraw: true,
		}
	}

	fn setup(&mut self) -> Result<(), String> {
		let controllers = if self.config.mode == Mode::FullDuplex || self.config.connect {
			command_cancellable(
				&["bluetoothctl", "list"],
				COMMAND_TIMEOUT,
				Some(&self.stopped),
			)
			.ok()
			.map(|output| bluez::controller_addresses(&output))
		} else {
			None
		};
		if self.config.mode == Mode::FullDuplex
			&& let Some(controllers) = &controllers
		{
			transport::validate_controller_count(
				self.config.mode,
				controllers.len(),
				self.config.sco_limit,
			)?;
		}
		if self.config.sco_limit > 1 {
			log::warn!(
				"Assuming up to {} simultaneous SCO/eSCO links per Bluetooth controller; verify the capacity of your hardware",
				self.config.sco_limit
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
		// Validate all device opens before starting any worker or saving the network.
		let inputs = PreparedInputs::open(&self.config.buttons)?;
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
					"Half-duplex: Hold play/pause to request the FIFO floor; release to cancel or relinquish. Double beep confirms a ready route."
				),
				Mode::FullDuplex => log::info!(
					"Full-duplex: Hold play/pause to transmit; release to mute. Three presses within 1 second (first to third, inclusive) toggle this headset's PTT/always-open mode."
				),
			}
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
		if outcome.invalidated {
			self.last_update = Some(Instant::now());
			self.update_interval = ROUTE_CHANGE_RETRY_INTERVAL;
			return Ok(());
		}
		self.report_update(outcome.update, outcome.config_error, outcome.inspect_error);
		self.last_update = Some(Instant::now());
		self.update_interval = if outcome.changed {
			ROUTE_CHANGE_RETRY_INTERVAL
		} else {
			self.config.interval
		};
		Ok(())
	}

	fn report_update(
		&mut self,
		update: Result<BTreeMap<Address, Headset>, String>,
		config_error: Option<String>,
		inspect_error: Option<String>,
	) {
		if self.dashboard.is_some() {
			self.last_error = config_error.or_else(|| update.err()).or(inspect_error);
			self.redraw = true;
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
						self.routing.router.allowed.len()
					);
				}
				Err(error) => log::warn!("Routing update failed: {error}"),
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
					},
					self.last_error.as_deref(),
					&mut frame,
				)
				.map_err(|error| error.to_string())?;
			let mut output = stderr.lock();
			if interactive {
				// Overwrite in place and erase leftovers to avoid a blank frame.
				execute!(output, MoveTo(0, 0)).map_err(|error| error.to_string())?;
				let mut text = String::from_utf8_lossy(&frame).replace('\n', "\x1b[K\n");
				text.push_str("\x1b[J");
				output
					.write_all(text.as_bytes())
					.map_err(|error| error.to_string())?;
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
	use crate::ptt::PttButton;
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
			dashboard: false,
			explicit_network: true,
			groups_path: network_path.with_extension("groups"),
			network_path,
			groups: vec![],
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
					.send(Ok((address.into(), false, Instant::now())))
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
