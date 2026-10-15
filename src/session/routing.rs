// Copyright (C) 2026 Philip Eriksson. All rights reserved.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicBool;

use crate::bluez::Address;
use crate::groups;
use crate::process::{COMMAND_TIMEOUT, command_cancellable};
use crate::ptt::{PttBeep, PttInput, confirm_connections, confirm_transmissions, drain_events};
use crate::router::{Headset, Router};
use crate::runtime_config::RunConfig;
use crate::transmit::{Mode, Transmit};
use crate::transport::{self, Transport};

type Execute = Box<dyn FnMut(&[&str]) -> Result<String, String>>;

pub(crate) struct InspectionOutcome {
	pub(crate) changed: bool,
	pub(crate) error: Option<String>,
}

pub(crate) struct RouteUpdateOutcome {
	pub(crate) update: Result<BTreeMap<Address, Headset>, String>,
	pub(crate) config_error: Option<String>,
	pub(crate) inspect_error: Option<String>,
	pub(crate) changed: bool,
	pub(crate) invalidated: bool,
}

struct PreparedCycle {
	config_error: Option<String>,
	sources: BTreeSet<Address>,
	transport_ready: bool,
}

pub(crate) struct RoutingCoordinator {
	pub(crate) router: Router<Execute>,
	pub(crate) headsets: BTreeMap<Address, Headset>,
	pub(crate) links: BTreeSet<(u64, u64)>,
	connected_headsets: BTreeSet<Address>,
	reported_controller_warnings: BTreeSet<String>,
}

impl RoutingCoordinator {
	pub(crate) fn new(config: &RunConfig, stopped: &std::sync::Arc<AtomicBool>) -> Self {
		let cancellation = std::sync::Arc::clone(stopped);
		let execute: Execute =
			Box::new(move |args| command_cancellable(args, COMMAND_TIMEOUT, Some(&cancellation)));
		let mut router = Router::with_executor(config.allowed.clone(), execute);
		router.set_transport(config.transport);
		router.set_sco_limit(config.sco_limit);
		Self {
			router,
			headsets: BTreeMap::new(),
			links: BTreeSet::new(),
			connected_headsets: BTreeSet::new(),
			reported_controller_warnings: BTreeSet::new(),
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
			return Ok(self.invalidate_cycle(cycle.config_error));
		}
		let update = self.update_route_set(
			config,
			input,
			&cycle.sources,
			cycle.transport_ready,
			transmit,
		);
		super::guard_half_update(config.mode, &update, || self.router.close())?;
		let inspection = self.inspect_and_confirm(config, transmit, input, beep, stopped)?;
		if inspection.changed {
			self.router.close();
			self.links.clear();
		}
		Ok(RouteUpdateOutcome {
			update,
			config_error: cycle.config_error,
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
		let config_error = self.reload_groups(config, transmit);
		self.refresh_topology(config, transmit, input)?;
		let sources = self.routing_sources(input, transmit);
		let prepared = self.router.prepare_transport(&sources);
		super::guard_half_update(config.mode, &prepared, || self.router.close())?;
		let transport_ready = prepared?;
		Ok(PreparedCycle {
			config_error,
			sources,
			transport_ready,
		})
	}

	fn invalidate_cycle(&mut self, config_error: Option<String>) -> RouteUpdateOutcome {
		self.router.close();
		self.links.clear();
		RouteUpdateOutcome {
			update: Ok(BTreeMap::new()),
			config_error,
			inspect_error: None,
			changed: true,
			invalidated: true,
		}
	}

	fn reload_groups(&mut self, config: &mut RunConfig, transmit: &mut Transmit) -> Option<String> {
		match groups::load(&config.groups_path) {
			Ok(groups) => {
				transmit.set_groups(&groups);
				config.groups = groups;
				None
			}
			Err(error) => Some(error),
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
		} else {
			if let Ok(devices) = self.router.transport_devices() {
				for warning in transport::controller_warnings(&devices, config.sco_limit) {
					if self.reported_controller_warnings.insert(warning.clone()) {
						log::warn!("{warning}");
					}
				}
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
		Ok(())
	}

	fn routing_sources(&self, input: Option<&PttInput>, transmit: &Transmit) -> BTreeSet<Address> {
		if input.is_some() {
			transmit.sources().clone()
		} else {
			self.router.allowed.clone()
		}
	}

	fn update_route_set(
		&mut self,
		config: &RunConfig,
		input: Option<&PttInput>,
		sources: &BTreeSet<Address>,
		transport_ready: bool,
		transmit: &Transmit,
	) -> Result<BTreeMap<Address, Headset>, String> {
		if !transport_ready {
			self.router.inspect().map(|(headsets, _)| headsets)
		} else if config.groups.is_empty() && input.is_none() {
			self.router.update(true)
		} else if config.mode == Mode::HalfDuplex && !config.groups.is_empty() {
			self.router
				.update_group_sources_in_groups(transmit.group_sources(), &config.groups)
		} else {
			self.router
				.update_sources_in_groups(sources, &config.groups)
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
			if !changed {
				changed |= confirm_transmissions(
					config.mode,
					transmit,
					&input.events,
					&self.headsets,
					&self.links,
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
