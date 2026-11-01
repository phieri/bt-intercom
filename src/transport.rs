// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Opt-in SCO microphone / A2DP receiver metadata and profile transitions.
//!
//! Profile changes are not restored on exit: restoring every HFP profile would
//! recreate the controller capacity conflicts this transport avoids.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use crate::bluez::Address;
use crate::pipewire::Snapshot;
use crate::transmit::{Mode, Transmit};
use crate::transport_model::ProfileFamily;

pub use crate::transport_model::{Device, Transport};
mod profile_plan;
use profile_plan::{ProfileAction, ProfilePlan, plan_profile_changes};

pub(crate) fn validate_controller_count(
	mode: Mode,
	count: usize,
	sco_limit: usize,
) -> Result<(), String> {
	if mode == Mode::FullDuplex && count > 0 && count.saturating_mul(sco_limit) < 2 {
		return Err(format!(
			"full-duplex requires at least two SCO/eSCO slots; detected {count} Bluetooth controller(s) with --sco-limit {sco_limit}. Use a higher verified --sco-limit or --mode half-duplex with --ptt for every headset"
		));
	}
	Ok(())
}

pub(crate) fn validate_transport(mode: Mode, transport: Transport) -> Result<(), String> {
	if transport == Transport::ScoA2dp && mode != Mode::HalfDuplex {
		return Err("--transport sco-a2dp requires --mode half-duplex and --ptt for every headset; use one controller per headset for full-duplex".into());
	}
	Ok(())
}

/// Headsets one controller can keep connected: BlueZ allows one active A2DP
/// stream plus one SCO/eSCO link per controller unless overridden. Only
/// `sco-a2dp` moves listeners off SCO onto A2DP streams. Whenever at least one
/// headset talks, all others stream A2DP, so the worst case (a single talker)
/// bounds the headsets to `a2dp_limit + 1`; more talkers only free A2DP slots.
pub(crate) fn headsets_per_controller(
	mode: Mode,
	transport: Transport,
	sco_limit: usize,
	a2dp_limit: usize,
) -> usize {
	let sco_limit = sco_limit.max(1);
	if mode == Mode::HalfDuplex && transport == Transport::ScoA2dp {
		a2dp_limit.max(1) + 1
	} else {
		sco_limit
	}
}

/// Startup check: the allowlisted network must fit the detected controllers.
pub(crate) fn validate_headset_capacity(
	headsets: usize,
	controllers: usize,
	mode: Mode,
	transport: Transport,
	sco_limit: usize,
	a2dp_limit: usize,
) -> Result<(), String> {
	let capacity = headsets_per_controller(mode, transport, sco_limit, a2dp_limit);
	if controllers > 0 && headsets > controllers.saturating_mul(capacity) {
		return Err(format!(
			"{headsets} headsets exceed the capacity of {controllers} Bluetooth controller(s): each supports {capacity} (one talker plus --a2dp-limit {a2dp_limit} A2DP listener(s) with --transport sco-a2dp, otherwise --sco-limit {sco_limit}). Add a USB Bluetooth adapter or remove headsets"
		));
	}
	Ok(())
}

/// Startup check for headsets whose controller is already known.
pub(crate) fn validate_controller_headsets(
	devices: &BTreeMap<Address, Device>,
	mode: Mode,
	transport: Transport,
	sco_limit: usize,
	a2dp_limit: usize,
) -> Result<(), String> {
	match controller_warnings(devices, mode, transport, sco_limit, a2dp_limit)
		.into_iter()
		.next()
	{
		Some(warning) => Err(warning),
		None => Ok(()),
	}
}

pub(crate) fn controller_warnings(
	devices: &BTreeMap<Address, Device>,
	mode: Mode,
	transport: Transport,
	sco_limit: usize,
	a2dp_limit: usize,
) -> Vec<String> {
	let capacity = headsets_per_controller(mode, transport, sco_limit, a2dp_limit);
	addresses_by_controller(devices.iter())
		.into_iter()
		.filter(|(_, addresses)| addresses.len() > capacity)
		.map(|(controller, addresses)| {
			format!(
				"{} headsets share {controller} ({}), exceeding its capacity of {capacity} (one talker plus --a2dp-limit {a2dp_limit} A2DP listener(s) with --transport sco-a2dp, otherwise --sco-limit {sco_limit}). Add a USB Bluetooth adapter or use --mode half-duplex --transport sco-a2dp with PTT.",
				addresses.len(),
				addresses
					.iter()
					.map(ToString::to_string)
					.collect::<Vec<_>>()
					.join(", ")
			)
		})
		.collect()
}

pub(crate) fn refresh_transmit_requests(
	transmit: &mut Transmit,
	devices: &BTreeMap<Address, Device>,
	disconnected: &BTreeSet<Address>,
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

fn addresses_by_controller<'a>(
	devices: impl IntoIterator<Item = (&'a Address, &'a Device)>,
) -> BTreeMap<&'a str, Vec<&'a Address>> {
	let mut controllers = BTreeMap::new();
	for (address, device) in devices {
		controllers
			.entry(device.controller.as_str())
			.or_insert_with(Vec::new)
			.push(address);
	}
	controllers
}

struct PendingProfile {
	device: Device,
	index: u64,
	requested_at: Instant,
}

enum TransitionPhase {
	Idle,
	Preparing {
		sources: BTreeSet<Address>,
		started_at: Instant,
	},
	Ready {
		sources: BTreeSet<Address>,
	},
}

impl TransitionPhase {
	fn begin(&mut self, sources: &BTreeSet<Address>) -> Instant {
		if let Self::Preparing {
			sources: current,
			started_at,
		} = self && current == sources
		{
			return *started_at;
		}
		let started_at = Instant::now();
		*self = Self::Preparing {
			sources: sources.clone(),
			started_at,
		};
		started_at
	}

	fn mark_ready(&mut self) {
		let Self::Preparing { sources, .. } = self else {
			return;
		};
		*self = Self::Ready {
			sources: sources.clone(),
		};
	}
}

/// Explicit transition state, independent of command execution and audio links.
pub(crate) struct TransitionState {
	phase: TransitionPhase,
	pending_profiles: BTreeMap<Address, PendingProfile>,
}

impl Default for TransitionState {
	fn default() -> Self {
		Self {
			phase: TransitionPhase::Idle,
			pending_profiles: BTreeMap::new(),
		}
	}
}

impl TransitionState {
	fn begin(&mut self, sources: &BTreeSet<Address>) -> Instant {
		self.phase.begin(sources)
	}

	fn mark_ready(&mut self) {
		self.phase.mark_ready();
	}

	fn retain_unobserved_profiles(&mut self, devices: &BTreeMap<Address, Device>) {
		self.pending_profiles.retain(|address, pending| {
			devices.get(address).is_some_and(|current| {
				current.same_identity(&pending.device) && current.current != Some(pending.index)
			})
		});
	}

	fn has_pending_profiles(&self) -> bool {
		!self.pending_profiles.is_empty()
	}

	fn record_profile(&mut self, address: Address, device: Device, index: u64) {
		self.pending_profiles.insert(
			address,
			PendingProfile {
				device,
				index,
				requested_at: Instant::now(),
			},
		);
	}

	fn pending_profile(&self, address: &Address) -> Option<&PendingProfile> {
		self.pending_profiles.get(address)
	}

	#[cfg(test)]
	pub(crate) fn preparing_since(&self) -> Option<Instant> {
		match self.phase {
			TransitionPhase::Preparing { started_at, .. } => Some(started_at),
			TransitionPhase::Idle | TransitionPhase::Ready { .. } => None,
		}
	}

	#[cfg(test)]
	pub(crate) fn profile_count(&self) -> usize {
		self.pending_profiles.len()
	}

	#[cfg(test)]
	pub(crate) fn expire_preparation(&mut self, age: Duration) {
		if let TransitionPhase::Preparing { started_at, .. } = &mut self.phase {
			*started_at = Instant::now() - age;
		}
	}
}

const A2DP_SINK_UUID: &str = "0000110b-0000-1000-8000-00805f9b34fb";
const HFP_UUID: &str = "0000111e-0000-1000-8000-00805f9b34fb";

/// Profile-transition coordinator, independent of the clients owning audio links.
pub(crate) struct Coordinator {
	pub(crate) transport: Transport,
	sco_limit: usize,
	a2dp_limit: usize,
	pub(crate) transition: TransitionState,
	disconnects: BTreeSet<Address>,
	profile_wait: Option<String>,
	profile_nudged: Option<Instant>,
}

impl Default for Coordinator {
	fn default() -> Self {
		Self {
			transport: Transport::default(),
			sco_limit: 1,
			a2dp_limit: 1,
			transition: TransitionState::default(),
			disconnects: BTreeSet::new(),
			profile_wait: None,
			profile_nudged: None,
		}
	}
}

struct TransitionIo<'a, F, C> {
	allowed: &'a BTreeSet<Address>,
	execute: &'a mut F,
	close_routes: C,
}

impl Coordinator {
	pub(crate) fn new(transport: Transport) -> Self {
		Self {
			transport,
			..Self::default()
		}
	}

	pub(crate) fn set_sco_limit(&mut self, limit: usize) {
		self.sco_limit = limit;
	}

	pub(crate) fn set_a2dp_limit(&mut self, limit: usize) {
		self.a2dp_limit = limit;
	}

	pub(crate) fn take_disconnects(&mut self) -> BTreeSet<Address> {
		std::mem::take(&mut self.disconnects)
	}

	pub(crate) fn snapshot(
		&mut self,
		allowed: &BTreeSet<Address>,
		execute: &mut impl FnMut(&[&str]) -> Result<String, String>,
	) -> Result<Snapshot, String> {
		let snapshot = Snapshot::fetch(execute)?;
		if self.transport == Transport::ScoA2dp {
			// Presence, not temporary profile ports, determines absence. Keep
			// disappearances latched until input policy consumes them.
			let observed = snapshot.addresses();
			self.disconnects
				.extend(allowed.difference(&observed).cloned());
		}
		Ok(snapshot)
	}

	pub(crate) fn prepare(
		&mut self,
		sources: &BTreeSet<Address>,
		allowed: &BTreeSet<Address>,
		execute: &mut impl FnMut(&[&str]) -> Result<String, String>,
		close_routes: impl FnMut(),
	) -> Result<bool, String> {
		let mut io = TransitionIo {
			allowed,
			execute,
			close_routes,
		};
		let result = self.prepare_inner(sources, &mut io);
		if matches!(result, Ok(true)) {
			self.transition.mark_ready();
		} else {
			(io.close_routes)();
		}
		result
	}

	fn prepare_inner<F, C>(
		&mut self,
		sources: &BTreeSet<Address>,
		io: &mut TransitionIo<'_, F, C>,
	) -> Result<bool, String>
	where
		F: FnMut(&[&str]) -> Result<String, String>,
		C: FnMut(),
	{
		if self.transport == Transport::Hfp {
			return Ok(true);
		}
		if let Some(address) = sources
			.iter()
			.find(|address| !io.allowed.contains(*address))
		{
			return Err(format!("{address}: requested source is not allowlisted"));
		}
		if !matches!(
			&self.transition.phase,
			TransitionPhase::Preparing {
				sources: current, ..
			} | TransitionPhase::Ready { sources: current }
				if current == sources
		) {
			(io.close_routes)();
		}
		let started = self.transition.begin(sources);
		if started.elapsed() >= Duration::from_secs(30) {
			let detail = self
				.profile_wait
				.take()
				.map(|reason| format!(" (last profile check: {reason})"))
				.unwrap_or_default();
			return Err(format!(
				"SCO/A2DP transport did not become ready within 30 seconds: expected HFP microphone/speaker ports for requested sources and A2DP speaker ports with no old HFP nodes for receivers; inspect pw-dump, Bluetooth connections and WirePlumber profile policy{detail}"
			));
		}
		if !sources.is_disjoint(&self.disconnects) {
			return Ok(false);
		}
		let snapshot = self.snapshot(io.allowed, io.execute)?;
		self.prepare_snapshot(sources, io, snapshot)
	}

	fn prepare_snapshot<F, C>(
		&mut self,
		sources: &BTreeSet<Address>,
		io: &mut TransitionIo<'_, F, C>,
		snapshot: Snapshot,
	) -> Result<bool, String>
	where
		F: FnMut(&[&str]) -> Result<String, String>,
		C: FnMut(),
	{
		let devices = snapshot.transport_devices(io.allowed)?;
		self.transition.retain_unobserved_profiles(&devices);
		let (headsets, _) = snapshot.topology(io.allowed, self.transport);
		for address in headsets.keys() {
			if !devices.contains_key(address) {
				return Err(format!(
					"{address}: SCO/A2DP requires an allowlisted PipeWire Device with matching api.bluez5.path and object.serial"
				));
			}
		}
		if sources.iter().any(|address| !devices.contains_key(address)) {
			return Ok(false);
		}
		validate_source_capacity(sources, &devices, self.sco_limit)?;
		validate_listener_capacity(sources, &devices, self.a2dp_limit)?;
		// Headsets such as CMF Buds 2 connect HFP and A2DP separately, so a
		// profile can be unavailable until the second connection completes.
		// Wait within the preparation deadline instead of failing immediately.
		if let Err(reason) = validate_role_profiles(sources, &devices) {
			let refused = self.request_missing_profiles(sources, &devices, &snapshot, io.execute);
			self.profile_wait = Some(match refused {
				Some(refused) => format!("{reason}; {refused}"),
				None => reason,
			});
			return Ok(false);
		}
		self.profile_wait = None;
		// An accepted promotion can arrive after a floor change. Observe it
		// before issuing its demotion or another promotion.
		for (address, pending) in &self.transition.pending_profiles {
			if pending.requested_at.elapsed() >= Duration::from_secs(15) {
				return Err(format!(
					"{address}: timed out waiting for the requested PipeWire profile; inspect pw-dump and Bluetooth connection"
				));
			}
		}
		if self.transition.has_pending_profiles() {
			return Ok(false);
		}

		match plan_profile_changes(sources, &snapshot, &devices) {
			ProfilePlan::Wait => Ok(false),
			ProfilePlan::Ready => Ok(self.sources_ready(sources, &headsets, &devices)),
			ProfilePlan::Apply(actions) => {
				for action in actions {
					let ProfileAction::SelectProfile { address, family } = action;
					self.change_profile(&address, &devices[&address], family, sources, io)?;
				}
				Ok(false)
			}
		}
	}

	// BlueZ may leave A2DP/HFP disconnected after the other profile connects,
	// so PipeWire never marks it available. Ask BlueZ to connect it, at most
	// every 5 seconds, only once no profile change or other listener's HFP
	// teardown is outstanding; failures are non-fatal because the wait deadline applies.
	fn request_missing_profiles(
		&mut self,
		sources: &BTreeSet<Address>,
		devices: &BTreeMap<Address, Device>,
		snapshot: &Snapshot,
		execute: &mut impl FnMut(&[&str]) -> Result<String, String>,
	) -> Option<String> {
		if self.transition.has_pending_profiles()
			|| self
				.profile_nudged
				.is_some_and(|at| at.elapsed() < Duration::from_secs(5))
		{
			return None;
		}
		self.profile_nudged = Some(Instant::now());
		let mut failures = Vec::new();
		for (address, device) in devices {
			let (family, uuid) = role_profile(sources, address);
			// Close before open: another listener must have released HFP first.
			let closing = devices.iter().any(|(other, device)| {
				other != address
					&& !sources.contains(other)
					&& (!device.is_profile(ProfileFamily::A2dp) || snapshot.has_hfp_node(device.id))
			});
			if device.index(family).is_none()
				&& !closing && let Err(error) = crate::bluez::checked(execute(&[
				"bluetoothctl",
				"connect-profile",
				address.as_str(),
				uuid,
			])) {
				log::debug!("{address}: could not request {}: {error}", family.name());
				failures.push(format!(
					"{address}: BlueZ refused {}: {error}",
					family.name()
				));
			}
		}
		(!failures.is_empty()).then(|| failures.join("; "))
	}

	fn sources_ready(
		&self,
		sources: &BTreeSet<Address>,
		headsets: &BTreeMap<Address, crate::pipewire::Headset>,
		devices: &BTreeMap<Address, Device>,
	) -> bool {
		devices.iter().all(|(address, device)| {
			headsets.get(address).is_some_and(|headset| {
				if sources.contains(address) {
					device.is_profile(ProfileFamily::Hfp) && headset.has_duplex_audio()
				} else {
					device.is_profile(ProfileFamily::A2dp) && !headset.sinks.is_empty()
				}
			})
		})
	}

	fn change_profile<F, C>(
		&mut self,
		address: &Address,
		expected: &Device,
		family: ProfileFamily,
		sources: &BTreeSet<Address>,
		io: &mut TransitionIo<'_, F, C>,
	) -> Result<(), String>
	where
		F: FnMut(&[&str]) -> Result<String, String>,
		C: FnMut(),
	{
		// IDs are reusable. Fetch fresh serial, BlueZ path and advertised indices
		// immediately before issuing a mutation; never cache this observation.
		let snapshot = self.snapshot(io.allowed, io.execute)?;
		let devices = snapshot.transport_devices(io.allowed)?;
		let (headsets, _) = snapshot.topology(io.allowed, self.transport);
		for address in headsets.keys() {
			if !devices.contains_key(address) {
				return Err(format!(
					"{address}: device metadata changed before profile selection; retry discovery"
				));
			}
		}
		if sources.iter().any(|address| !devices.contains_key(address))
			|| !headsets.contains_key(address)
		{
			return Ok(());
		}
		validate_role_profiles(sources, &devices)?;
		validate_source_capacity(sources, &devices, self.sco_limit)?;
		let current = devices
			.get(address)
			.filter(|device| device.same_identity(expected))
			.ok_or_else(|| {
				format!("{address}: PipeWire device identity changed; retry discovery")
			})?;
		let index = current
			.index(family)
			.ok_or_else(|| {
				if family == ProfileFamily::A2dp {
					format!(
						"{address}: no advertised A2DP profile; enable an A2DP codec in WirePlumber's Bluetooth settings"
					)
				} else {
					format!("{address}: {} profile is no longer available", family.name())
				}
			})?;
		if current.is_profile(family) {
			return Ok(());
		}
		if family == ProfileFamily::Hfp
			&& devices.iter().any(|(address, device)| {
				!sources.contains(address)
					&& (!device.is_profile(ProfileFamily::A2dp) || snapshot.has_hfp_node(device.id))
			}) {
			return Ok(());
		}
		if let Some(pending) = self.transition.pending_profile(address)
			&& pending.device.same_identity(current)
			&& pending.index == index
		{
			if pending.requested_at.elapsed() >= Duration::from_secs(15) {
				return Err(format!(
					"{address}: timed out waiting for {}; inspect pw-dump and Bluetooth connection",
					family.name()
				));
			}
			return Ok(());
		}
		(io.close_routes)();
		(io.execute)(&[
			"pw-cli",
			"set-param",
			&current.id.to_string(),
			"Profile",
			&format!("{{ index: {index}, save: false }}"),
		])
		.map_err(|error| format!("{address}: could not select {}: {error}", family.name()))?;
		self.transition
			.record_profile(address.clone(), current.clone(), index);
		Ok(())
	}
}

/// Every connected headset on a controller with an active talker streams
/// A2DP, so their count must fit the controller's A2DP budget.
fn validate_listener_capacity(
	sources: &BTreeSet<Address>,
	devices: &BTreeMap<Address, Device>,
	a2dp_limit: usize,
) -> Result<(), String> {
	if a2dp_limit == 0 {
		return Ok(());
	}
	let talking: BTreeSet<_> = sources
		.iter()
		.filter_map(|address| devices.get(address))
		.map(|device| device.controller.as_str())
		.collect();
	for (controller, addresses) in addresses_by_controller(devices.iter()) {
		let listeners = addresses
			.iter()
			.filter(|address| !sources.contains(**address))
			.count();
		if talking.contains(controller) && listeners > a2dp_limit {
			return Err(format!(
				"A2DP capacity conflict on {controller}: {listeners} listeners exceed configured --a2dp-limit {a2dp_limit} while a headset talks. Add a USB Bluetooth adapter, remove headsets from this controller, or raise --a2dp-limit only after verifying it"
			));
		}
	}
	Ok(())
}

fn validate_source_capacity(
	sources: &BTreeSet<Address>,
	devices: &BTreeMap<Address, Device>,
	sco_limit: usize,
) -> Result<(), String> {
	for (controller, addresses) in addresses_by_controller(
		sources
			.iter()
			.filter_map(|address| devices.get(address).map(|device| (address, device))),
	) {
		if addresses.len() > sco_limit {
			return Err(format!(
				"SCO capacity conflict on {controller}: {} sources ({}) exceed configured --sco-limit {sco_limit}",
				addresses.len(),
				addresses
					.iter()
					.map(ToString::to_string)
					.collect::<Vec<_>>()
					.join(", ")
			));
		}
	}
	Ok(())
}

/// Half-duplex uses one profile per headset at a time: HFP for talkers and
/// A2DP for listeners, so only that profile must be available.
fn role_profile(sources: &BTreeSet<Address>, address: &Address) -> (ProfileFamily, &'static str) {
	if sources.contains(address) {
		(ProfileFamily::Hfp, HFP_UUID)
	} else {
		(ProfileFamily::A2dp, A2DP_SINK_UUID)
	}
}

fn validate_role_profiles(
	sources: &BTreeSet<Address>,
	devices: &BTreeMap<Address, Device>,
) -> Result<(), String> {
	for (address, device) in devices {
		let (family, _) = role_profile(sources, address);
		if device.index(family).is_none() {
			return Err(if family == ProfileFamily::A2dp {
				format!(
					"{address}: no available advertised A2DP profile; listeners need A2DP; enable an A2DP codec in WirePlumber's Bluetooth settings"
				)
			} else {
				format!(
					"{address}: no available advertised {} profile; talkers need HFP; check headset support and PipeWire Bluetooth configuration",
					family.name()
				)
			});
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn address(value: &str) -> Address {
		Address::parse(value).unwrap()
	}

	fn device_on_controller(id: u64, address: &Address, controller: &str) -> Device {
		Device::parse(
			id,
			address,
			&json!({
				"props": {
					"api.bluez5.path": format!(
						"/org/bluez/{controller}/dev_{}",
						address.as_str().replace(':', "_")
					),
					"object.serial": id + 1000
				},
				"params": { "EnumProfile": [], "Profile": [] }
			}),
		)
		.unwrap()
	}

	fn snapshot_with_profiles(
		profiles: &[(&str, u64, bool)],
	) -> (Snapshot, BTreeMap<Address, Device>) {
		let mut objects = Vec::new();
		let mut allowed = BTreeSet::new();
		for (index, (raw_address, current, hfp_node)) in profiles.iter().enumerate() {
			let address = address(raw_address);
			let id = (index as u64 + 1) * 10;
			allowed.insert(address.clone());
			objects.push(json!({
				"id": id,
				"type": "PipeWire:Interface:Device",
				"info": {
					"props": {
						"api.bluez5.address": address.as_str(),
						"api.bluez5.path": format!(
							"/org/bluez/hci0/dev_{}",
							address.as_str().replace(':', "_")
						),
						"object.serial": id + 1000
					},
					"params": {
						"EnumProfile": [
							{"index": 17, "name": "headset-head-unit", "available": "yes"},
							{"index": 41, "name": "a2dp-sink-sbc", "available": "yes"}
						],
						"Profile": [{"index": current}]
					}
				}
			}));
			if *hfp_node {
				objects.push(json!({
					"id": id + 1,
					"type": "PipeWire:Interface:Node",
					"info": {"props": {
						"device.id": id,
						"media.class": "Audio/Sink",
						"api.bluez5.profile": "headset-head-unit"
					}}
				}));
			}
		}
		let snapshot = Snapshot::parse(&json!(objects)).unwrap();
		let devices = snapshot.transport_devices(&allowed).unwrap();
		(snapshot, devices)
	}

	#[test]
	fn controller_capacity_adds_one_a2dp_stream_only_for_sco_a2dp() {
		let addresses = [
			address("AA:BB:CC:DD:EE:01"),
			address("AA:BB:CC:DD:EE:02"),
			address("AA:BB:CC:DD:EE:03"),
		];
		let devices = addresses
			.iter()
			.enumerate()
			.map(|(index, address)| {
				(
					address.clone(),
					device_on_controller(index as u64, address, "hci0"),
				)
			})
			.collect::<BTreeMap<_, _>>();
		let two = BTreeMap::from_iter(devices.iter().take(2).map(|(a, d)| (a.clone(), d.clone())));
		let warn = |devices, mode, transport| controller_warnings(devices, mode, transport, 1, 1);

		assert_eq!(
			headsets_per_controller(Mode::HalfDuplex, Transport::ScoA2dp, 1, 1),
			2
		);
		assert_eq!(
			headsets_per_controller(Mode::HalfDuplex, Transport::Hfp, 1, 1),
			1
		);
		assert_eq!(
			headsets_per_controller(Mode::FullDuplex, Transport::Hfp, 1, 1),
			1
		);
		assert!(warn(&two, Mode::HalfDuplex, Transport::ScoA2dp).is_empty());
		assert_eq!(warn(&two, Mode::HalfDuplex, Transport::Hfp).len(), 1);
		assert_eq!(
			warn(&devices, Mode::HalfDuplex, Transport::ScoA2dp).len(),
			1
		);
	}

	#[test]
	fn listeners_must_fit_a2dp_limit_while_a_headset_talks() {
		let addresses = [
			"AA:BB:CC:DD:EE:01",
			"AA:BB:CC:DD:EE:02",
			"AA:BB:CC:DD:EE:03",
		]
		.map(address);
		let devices = addresses
			.iter()
			.enumerate()
			.map(|(index, address)| {
				(
					address.clone(),
					device_on_controller(index as u64, address, "hci0"),
				)
			})
			.collect::<BTreeMap<_, _>>();
		let one = BTreeSet::from([addresses[0].clone()]);
		let two = BTreeSet::from([addresses[0].clone(), addresses[1].clone()]);
		assert!(
			validate_listener_capacity(&one, &devices, 1)
				.unwrap_err()
				.contains("A2DP capacity conflict")
		);
		assert!(validate_listener_capacity(&one, &devices, 2).is_ok());
		assert!(validate_listener_capacity(&two, &devices, 1).is_ok());
		assert!(validate_listener_capacity(&BTreeSet::new(), &devices, 1).is_ok());
	}

	#[test]
	fn startup_capacity_rejects_more_headsets_than_controllers_support() {
		let check = |headsets, controllers, mode, transport| {
			validate_headset_capacity(headsets, controllers, mode, transport, 1, 1)
		};
		assert!(check(2, 1, Mode::HalfDuplex, Transport::ScoA2dp).is_ok());
		assert!(check(3, 1, Mode::HalfDuplex, Transport::ScoA2dp).is_err());
		assert!(check(4, 2, Mode::HalfDuplex, Transport::ScoA2dp).is_ok());
		assert!(check(2, 1, Mode::HalfDuplex, Transport::Hfp).is_err());
		assert!(check(2, 2, Mode::FullDuplex, Transport::Hfp).is_ok());
		assert!(check(9, 0, Mode::HalfDuplex, Transport::ScoA2dp).is_ok());
		assert_eq!(
			headsets_per_controller(Mode::HalfDuplex, Transport::ScoA2dp, 1, 3),
			4
		);
	}

	#[test]
	fn source_capacity_obeys_per_controller_limit() {
		let addresses = [
			address("AA:BB:CC:DD:EE:01"),
			address("AA:BB:CC:DD:EE:02"),
			address("AA:BB:CC:DD:EE:03"),
		];
		let devices = addresses
			.iter()
			.enumerate()
			.map(|(index, address)| {
				(
					address.clone(),
					device_on_controller(index as u64, address, "hci0"),
				)
			})
			.collect::<BTreeMap<_, _>>();
		let two = addresses[..2].iter().cloned().collect();
		let three = addresses.iter().cloned().collect();

		assert!(
			validate_source_capacity(&two, &devices, 1)
				.unwrap_err()
				.contains("--sco-limit 1")
		);
		assert!(validate_source_capacity(&two, &devices, 2).is_ok());
		assert!(validate_source_capacity(&three, &devices, 2).is_err());
	}

	#[test]
	fn profile_planning_demotes_receivers_before_promoting_sources() {
		let source = address("AA:BB:CC:DD:EE:01");
		let receiver = address("AA:BB:CC:DD:EE:02");
		let (snapshot, devices) =
			snapshot_with_profiles(&[(source.as_str(), 41, false), (receiver.as_str(), 17, true)]);
		assert_eq!(
			plan_profile_changes(&[source.clone()].into(), &snapshot, &devices),
			ProfilePlan::Apply(vec![ProfileAction::SelectProfile {
				address: receiver,
				family: ProfileFamily::A2dp,
			}])
		);
	}

	#[test]
	fn profile_planning_waits_for_stale_receiver_nodes_before_promotion() {
		let source = address("AA:BB:CC:DD:EE:01");
		let receiver = address("AA:BB:CC:DD:EE:02");
		let sources = [source.clone()].into();
		let (snapshot, devices) =
			snapshot_with_profiles(&[(source.as_str(), 41, false), (receiver.as_str(), 41, true)]);
		assert_eq!(
			plan_profile_changes(&sources, &snapshot, &devices),
			ProfilePlan::Wait
		);

		let (snapshot, devices) =
			snapshot_with_profiles(&[(source.as_str(), 41, false), (receiver.as_str(), 41, false)]);
		assert_eq!(
			plan_profile_changes(&sources, &snapshot, &devices),
			ProfilePlan::Apply(vec![ProfileAction::SelectProfile {
				address: source,
				family: ProfileFamily::Hfp,
			}])
		);
	}
}
