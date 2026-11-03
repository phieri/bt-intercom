// Copyright (C) 2026 Philip Eriksson. All rights reserved.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

use crate::bluez::Address;
use crate::process::{COMMAND_TIMEOUT, command_cancellable};
use crate::ptt::{PttBeep, PttInput, confirm_connections, confirm_transmissions, drain_events};
use crate::router::{Headset, Router};
use crate::routing_status::Observed;
use crate::runtime_config::RunConfig;
use crate::transmit::Transmit;
use crate::transport::{self, Transport};

type Execute = Box<dyn FnMut(&[&str]) -> Result<String, String>>;

pub(crate) struct InspectionOutcome {
	pub(crate) changed: bool,
	pub(crate) error: Option<String>,
}

pub(crate) struct RouteUpdateOutcome {
	pub(crate) update: Result<BTreeMap<Address, Headset>, String>,
	pub(crate) inspect_error: Option<String>,
	pub(crate) changed: bool,
	pub(crate) invalidated: bool,
}

struct PreparedCycle {
	sources: BTreeSet<Address>,
	host_talking: bool,
	transport_ready: bool,
}

pub(crate) struct RoutingCoordinator {
	pub(crate) router: Router<Execute>,
	pub(crate) headsets: BTreeMap<Address, Headset>,
	/// The host's wired headset ports, while it is available.
	pub(crate) host: Option<Headset>,
	pub(crate) links: BTreeSet<(u64, u64)>,
	connected_headsets: BTreeSet<Address>,
	reported_controller_warnings: BTreeSet<String>,
	reported_host_error: Option<String>,
}

impl RoutingCoordinator {
	pub(crate) fn new(config: &RunConfig, stopped: &std::sync::Arc<AtomicBool>) -> Self {
		let cancellation = std::sync::Arc::clone(stopped);
		let execute: Execute =
			Box::new(move |args| command_cancellable(args, COMMAND_TIMEOUT, Some(&cancellation)));
		let mut router = Router::with_executor(config.allowed.clone(), execute);
		router.set_transport(config.transport);
		router.set_sco_limit(config.sco_limit);
		router.set_a2dp_limit(config.a2dp_limit);
		if let Some(host) = &config.host {
			router.set_host(host.audio.clone());
		}
		Self {
			router,
			headsets: BTreeMap::new(),
			host: None,
			links: BTreeSet::new(),
			connected_headsets: BTreeSet::new(),
			reported_controller_warnings: BTreeSet::new(),
			reported_host_error: None,
		}
	}

	pub(crate) fn update(
		&mut self,
		config: &mut RunConfig,
		transmit: &mut Transmit,
		input: Option<&PttInput>,
		beep: Option<&PttBeep>,
		stopped: &AtomicBool,
	) -> Result<RouteUpdateOutcome, String> {
		let cycle = self.prepare_cycle(config, transmit, input)?;
		if let Some(input) = input
			&& drain_events(&input.events, transmit)?
		{
			return Ok(self.invalidate_cycle());
		}
		let update = self.update_route_set(
			input,
			&cycle.sources,
			cycle.host_talking,
			cycle.transport_ready,
		);
		super::guard_half_update(config.mode, &update, || self.router.close())?;
		let inspection = self.inspect_and_confirm(config, transmit, input, beep, stopped)?;
		if inspection.changed {
			self.router.close();
			self.links.clear();
		}
		Ok(RouteUpdateOutcome {
			update,
			inspect_error: inspection.error,
			changed: inspection.changed,
			invalidated: false,
		})
	}

	fn prepare_cycle(
		&mut self,
		config: &mut RunConfig,
		transmit: &mut Transmit,
		input: Option<&PttInput>,
	) -> Result<PreparedCycle, String> {
		self.refresh_topology(config, transmit, input)?;
		let (sources, host_talking) = self.routing_sources(input, transmit);
		let prepared = self.router.prepare_transport(&sources);
		super::guard_half_update(config.mode, &prepared, || self.router.close())?;
		let transport_ready = prepared?;
		Ok(PreparedCycle {
			sources,
			host_talking,
			transport_ready,
		})
	}

	fn invalidate_cycle(&mut self) -> RouteUpdateOutcome {
		self.router.close();
		self.links.clear();
		RouteUpdateOutcome {
			update: Ok(BTreeMap::new()),
			inspect_error: None,
			changed: true,
			invalidated: true,
		}
	}

	fn warn_controller_capacity(
		&mut self,
		config: &RunConfig,
		devices: &BTreeMap<Address, transport::Device>,
	) {
		for warning in transport::controller_warnings(
			devices,
			config.mode,
			config.transport,
			config.sco_limit,
			config.a2dp_limit,
		) {
			if self.reported_controller_warnings.insert(warning.clone()) {
				log::warn!("{warning}");
			}
		}
	}

	fn refresh_topology(
		&mut self,
		config: &RunConfig,
		transmit: &mut Transmit,
		input: Option<&PttInput>,
	) -> Result<(), String> {
		if config.transport == Transport::ScoA2dp {
			let devices = self.router.transport_devices();
			super::guard_half_update(config.mode, &devices, || self.router.close())?;
			let devices = devices?;
			transmit.set_controllers(
				&devices
					.iter()
					.map(|(address, device)| (address.clone(), device.controller.clone()))
					.collect(),
			);
			transmit.topology(&devices.keys().cloned().collect());
			self.warn_controller_capacity(config, &devices);
		} else {
			if let Ok(devices) = self.router.transport_devices() {
				self.warn_controller_capacity(config, &devices);
			}
			if let Some(input) = input {
				let snapshot = self.router.inspect_owned();
				drain_events(&input.events, transmit)?;
				if let Ok((current, _)) = snapshot {
					let available = current
						.iter()
						.filter(|(_, headset)| headset.has_duplex_audio())
						.map(|(address, _)| address.clone())
						.collect();
					transmit.topology(&available);
				}
			}
		}
		self.sync_host(transmit);
		Ok(())
	}

	/// Follows the host's wired headset as of the latest snapshot, telling the
	/// user once when it is unavailable. Returns whether the floor changed.
	fn sync_host(&mut self, transmit: &mut Transmit) -> bool {
		if !transmit.has_host() {
			return false;
		}
		self.host = self.router.host_ports().cloned();
		let error = self.router.host_error().map(str::to_owned);
		if error != self.reported_host_error {
			match &error {
				Some(error) => log::warn!("Host wired headset unavailable: {error}"),
				None if self.reported_host_error.is_some() => {
					log::info!("Host wired headset is available");
				}
				None => {}
			}
			self.reported_host_error = error;
		}
		transmit.host_topology(self.host.is_some())
	}

	fn routing_sources(
		&self,
		input: Option<&PttInput>,
		transmit: &Transmit,
	) -> (BTreeSet<Address>, bool) {
		if input.is_some() {
			(transmit.sources().clone(), transmit.host_active())
		} else {
			(self.router.allowed.clone(), false)
		}
	}

	fn update_route_set(
		&mut self,
		input: Option<&PttInput>,
		sources: &BTreeSet<Address>,
		host_talking: bool,
		transport_ready: bool,
	) -> Result<BTreeMap<Address, Headset>, String> {
		if !transport_ready {
			self.router.inspect().map(|(headsets, _)| headsets)
		} else if input.is_none() {
			self.router.update(true)
		} else {
			self.router.update_talkers(sources, host_talking)
		}
	}

	fn inspect_and_confirm(
		&mut self,
		config: &RunConfig,
		transmit: &mut Transmit,
		input: Option<&PttInput>,
		beep: Option<&PttBeep>,
		stopped: &AtomicBool,
	) -> Result<InspectionOutcome, String> {
		let (current, active) = match self.router.inspect_owned() {
			Ok(snapshot) => snapshot,
			Err(error) => {
				self.headsets.clear();
				self.links.clear();
				return Ok(InspectionOutcome {
					changed: false,
					error: Some(error),
				});
			}
		};
		self.headsets = current;
		self.links = active;
		self.host = self.router.host_ports().cloned();
		let mut changed = false;
		if let Some(input) = input {
			changed |= drain_events(&input.events, transmit)?;
			if config.transport == Transport::ScoA2dp {
				let devices = self.router.transport_devices();
				super::guard_half_update(config.mode, &devices, || self.router.close())?;
				changed |= transport::refresh_transmit_requests(
					transmit,
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
				changed |= transmit.topology(&available);
			}
			changed |= self.sync_host(transmit);
			if !changed {
				changed |= confirm_transmissions(
					config.mode,
					transmit,
					&input.events,
					Observed {
						headsets: &self.headsets,
						host: self.host.as_ref(),
						links: &self.links,
					},
					beep,
					stopped,
				)?;
			}
		} else {
			confirm_connections(
				&mut self.connected_headsets,
				&self.headsets,
				&self.links,
				beep,
				stopped,
			);
		}
		Ok(InspectionOutcome {
			changed,
			error: None,
		})
	}
}
