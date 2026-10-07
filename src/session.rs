// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Runtime session ownership, event processing, routing, and presentation.

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use crossterm::cursor::MoveTo;
use crossterm::execute;
use crossterm::terminal::{Clear, ClearType};

use crate::dashboard::{Dashboard, RunStatus};
use crate::groups;
use crate::network;
use crate::pair_button::PairButton;
use crate::process::{COMMAND_TIMEOUT, command_cancellable};
use crate::ptt::{PreparedInputs, PttInput};
use crate::router::{Headset, Router};
use crate::runtime_config::RunConfig;
use crate::transmit::{Mode, Transmit};
use crate::transport::Transport;
use crate::worker::Worker;
use crate::{
	PttBeep, bluez, confirm_connections, confirm_transmissions, controller_warnings, drain_ptt,
	guard_half_update, reconnect_worker, refresh_transport_requests,
};

type Execute = Box<dyn FnMut(&[&str]) -> Result<String, String>>;

pub(crate) struct Session {
	config: RunConfig,
	stopped: Arc<AtomicBool>,
	router: Router<Execute>,
	transmit: Transmit,
	input: Option<PttInput>,
	pairing: Option<PairButton>,
	dashboard: Option<Dashboard>,
	connector: Option<Worker>,
	reconnect_devices: Arc<Mutex<BTreeSet<String>>>,
	beep: Option<PttBeep>,
	headsets: BTreeMap<String, Headset>,
	links: BTreeSet<(u64, u64)>,
	connected_headsets: BTreeSet<String>,
	reported_controller_warnings: BTreeSet<String>,
	last_error: Option<String>,
	last_update: Option<Instant>,
	redraw: bool,
}

impl Session {
	pub fn start(config: RunConfig) -> Result<Self, String> {
		let mut session = Self::new(config, Arc::new(AtomicBool::new(false)));
		session.setup()?;
		Ok(session)
	}

	fn new(config: RunConfig, stopped: Arc<AtomicBool>) -> Self {
		let cancellation = Arc::clone(&stopped);
		let execute: Execute =
			Box::new(move |args| command_cancellable(args, COMMAND_TIMEOUT, Some(&cancellation)));
		let mut router = Router::with_executor(config.allowed.clone(), execute);
		router.set_transport(config.transport);
		router.set_sco_limit(config.sco_limit);
		let mut transmit = Transmit::new(
			config.mode,
			config.buttons.iter().map(|button| button.address.clone()),
		);
		transmit.set_sco_limit(config.sco_limit);
		transmit.set_groups(&config.groups);
		let reconnect_devices = Arc::new(Mutex::new(config.allowed.clone()));
		Self {
			config,
			stopped,
			router,
			transmit,
			input: None,
			pairing: None,
			dashboard: None,
			connector: None,
			reconnect_devices,
			beep: None,
			headsets: BTreeMap::new(),
			links: BTreeSet::new(),
			connected_headsets: BTreeSet::new(),
			reported_controller_warnings: BTreeSet::new(),
			last_error: None,
			last_update: None,
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
			crate::validate_controller_count(
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
				"Multiple Bluetooth controllers detected: --connect addresses only BlueZ's default controller. Keep other controllers' headsets connected using an adapter-aware Bluetooth manager, or connect them in a bluetoothctl session after select CONTROLLER_MAC."
			);
		}
		// Validate all device opens before starting any worker or saving the network.
		let inputs = PreparedInputs::open(&self.config.buttons)?;
		let pairing_pin = if self.config.pair_button {
			Some(PairButton::open()?)
		} else {
			None
		};
		let signal = Arc::clone(&self.stopped);
		ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
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
			self.input = Some(inputs.start(Arc::clone(&self.stopped)));
			match self.config.mode {
				Mode::HalfDuplex => log::info!(
					"Half-duplex: Hold play/pause to request the FIFO floor; release to cancel or relinquish. Double beep confirms a ready route."
				),
				Mode::FullDuplex => log::info!(
					"Full-duplex: Hold play/pause to transmit; release to mute. Three presses within 1 second (first to third, inclusive) toggle this headset's PTT/always-open mode."
				),
			}
		}
		self.pairing = pairing_pin.map(|pin| {
			PairButton::start(
				pin,
				self.config.allowed.clone(),
				Arc::clone(&self.stopped),
				self.config.network_path.clone(),
			)
		});
		if self.config.dashboard {
			self.dashboard = Some(Dashboard::start(
				self.config.allowed.clone(),
				Arc::clone(&self.stopped),
			));
		}
		if self.config.connect {
			let devices = Arc::clone(&self.reconnect_devices);
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
			thread::sleep(Duration::from_millis(100).min(self.config.interval));
		}
		Ok(())
	}

	fn tick(&mut self) -> Result<(), String> {
		self.poll_events()?;
		if self
			.last_update
			.is_none_or(|updated| updated.elapsed() >= self.config.interval)
		{
			self.update_routes()?;
		}
		self.render()
	}

	fn poll_events(&mut self) -> Result<(), String> {
		if let Some(pairing) = &self.pairing {
			while let Ok(event) = pairing.events.try_recv() {
				match event {
					Ok(device) => {
						self.router.allowed.insert(device.clone());
						*self.reconnect_devices.lock().unwrap() = self.router.allowed.clone();
						if let Some(current) = &mut self.dashboard {
							current.stop();
							self.dashboard = Some(Dashboard::start(
								self.router.allowed.clone(),
								Arc::clone(&self.stopped),
							));
						}
						log::info!("Paired and saved {device}; added to the intercom");
						self.last_update = None;
						self.redraw = true;
					}
					Err(error) => log::warn!("Button pairing failed: {error}"),
				}
			}
		}
		if let Some(input) = &self.input
			&& drain_ptt(&input.events, &mut self.transmit)?
		{
			self.last_update = None;
			self.redraw = true;
		}
		Ok(())
	}

	fn refresh_topology(&mut self) -> Result<(), String> {
		if self.config.transport == Transport::ScoA2dp {
			let devices = self.router.transport_devices();
			guard_half_update(self.config.mode, &devices, || self.router.close())?;
			let devices = devices?;
			self.transmit.set_controllers(
				&devices
					.iter()
					.map(|(address, device)| (address.clone(), device.controller.clone()))
					.collect(),
			);
			self.transmit.topology(&devices.keys().cloned().collect());
		} else {
			if let Ok(devices) = self.router.transport_devices() {
				for warning in controller_warnings(&devices, self.config.sco_limit) {
					if self.reported_controller_warnings.insert(warning.clone()) {
						log::warn!("{warning}");
					}
				}
			}
			// Profile transitions remove microphones; only HFP treats this as a disconnect.
			if let Some(input) = &self.input {
				let snapshot = self.router.inspect_owned();
				drain_ptt(&input.events, &mut self.transmit)?;
				if let Ok((current, _)) = snapshot {
					let available = current
						.iter()
						.filter(|(_, headset)| headset.has_duplex_audio())
						.map(|(address, _)| address.clone())
						.collect();
					self.transmit.topology(&available);
				}
			}
		}
		Ok(())
	}

	fn update_routes(&mut self) -> Result<(), String> {
		let config_error = match groups::load(&self.config.groups_path) {
			Ok(groups) => {
				self.transmit.set_groups(&groups);
				self.config.groups = groups;
				None
			}
			Err(error) => Some(error),
		};
		self.refresh_topology()?;
		let sources = if self.input.is_some() {
			self.transmit.sources().clone()
		} else {
			self.router.allowed.clone()
		};
		let prepared = self.router.prepare_transport(&sources);
		guard_half_update(self.config.mode, &prepared, || self.router.close())?;
		let transport_ready = prepared?;
		if let Some(input) = &self.input
			&& drain_ptt(&input.events, &mut self.transmit)?
		{
			self.router.close();
			self.links.clear();
			self.last_update = None;
			return Ok(());
		}
		let update = if !transport_ready {
			self.router.inspect().map(|(headsets, _)| headsets)
		} else if self.config.groups.is_empty() && self.input.is_none() {
			self.router.update(true)
		} else if self.config.mode == Mode::HalfDuplex && !self.config.groups.is_empty() {
			self.router
				.update_group_sources_in_groups(self.transmit.group_sources(), &self.config.groups)
		} else {
			self.router
				.update_sources_in_groups(&sources, &self.config.groups)
		};
		guard_half_update(self.config.mode, &update, || self.router.close())?;
		let (changed, inspect_error) = self.inspect_and_confirm()?;
		if changed {
			self.router.close();
			self.links.clear();
		}
		self.report_update(update, config_error, inspect_error);
		self.last_update = (!changed).then(Instant::now);
		Ok(())
	}

	fn inspect_and_confirm(&mut self) -> Result<(bool, Option<String>), String> {
		let (current, active) = match self.router.inspect_owned() {
			Ok(snapshot) => snapshot,
			Err(error) => {
				self.headsets.clear();
				self.links.clear();
				return Ok((false, Some(error)));
			}
		};
		self.headsets = current;
		self.links = active;
		let mut changed = false;
		if let Some(input) = &self.input {
			changed |= drain_ptt(&input.events, &mut self.transmit)?;
			if self.config.transport == Transport::ScoA2dp {
				let devices = self.router.transport_devices();
				guard_half_update(self.config.mode, &devices, || self.router.close())?;
				changed |= refresh_transport_requests(
					&mut self.transmit,
					&devices?,
					&self.router.take_transport_disconnects(),
				);
			} else {
				let available = self
					.headsets
					.iter()
					.filter(|(_, headset)| headset.has_duplex_audio())
					.map(|(address, _)| address.clone())
					.collect();
				changed |= self.transmit.topology(&available);
			}
			if !changed {
				changed |= confirm_transmissions(
					self.config.mode,
					&mut self.transmit,
					&input.events,
					&self.headsets,
					&self.links,
					self.beep.as_ref(),
					&self.stopped,
				)?;
			}
		} else {
			confirm_connections(
				&mut self.connected_headsets,
				&self.headsets,
				&self.links,
				self.beep.as_ref(),
				&self.stopped,
			);
		}
		Ok((changed, None))
	}

	fn report_update(
		&mut self,
		update: Result<BTreeMap<String, Headset>, String>,
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
						self.router.allowed.len()
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
			let mut output = stderr.lock();
			if interactive {
				execute!(output, Clear(ClearType::All), MoveTo(0, 0))
					.map_err(|error| error.to_string())?;
			}
			dashboard
				.draw(
					&self.router.allowed,
					&self.headsets,
					&self.links,
					RunStatus {
						mode: self.config.mode,
						transmitting: self.input.is_none() || !self.transmit.sources().is_empty(),
					},
					self.last_error.as_deref(),
					&mut output,
				)
				.map_err(|error| error.to_string())?;
			self.redraw = false;
		}
		Ok(())
	}
}

impl Drop for Session {
	fn drop(&mut self) {
		self.stopped.store(true, Ordering::SeqCst);
		self.router.close();
		self.pairing.take();
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
	use std::sync::mpsc;

	fn config() -> RunConfig {
		let network_path =
			std::env::temp_dir().join(format!("session-unused-network-{}", std::process::id()));
		RunConfig {
			allowed: ["AA:BB:CC:DD:EE:01".into()].into(),
			interval: Duration::from_millis(1),
			connect: false,
			pair_button: false,
			buttons: vec![crate::PttButton {
				address: "AA:BB:CC:DD:EE:01".into(),
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
		session.router = Router::with_executor(session.config.allowed.clone(), execute);
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
		session.router = Router::with_executor(session.config.allowed.clone(), execute);
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
		assert!(session.transmit.event(address, true, Instant::now()));
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
		session.router = Router::with_executor(session.config.allowed.clone(), execute);
		session.update_routes().unwrap();
		assert!(session.transmit.sources().is_empty());
		assert!(session.transmit.pending.is_empty());
		assert!(session.links.is_empty());
		assert!(session.last_update.is_none());
	}

	#[test]
	fn input_open_failure_does_not_save_network_or_start_workers() {
		let mut config = config();
		config.buttons = vec![crate::PttButton {
			address: "AA:BB:CC:DD:EE:01".into(),
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
		assert!(session.pairing.is_none());
		assert!(!path.exists());
	}
}
