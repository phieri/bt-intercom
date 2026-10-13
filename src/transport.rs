//! Opt-in SCO microphone / A2DP receiver metadata and profile transitions.
//!
//! Profile changes are not restored on exit: restoring every HFP profile would
//! recreate the controller capacity conflicts this transport avoids.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::bluez::Address;
use crate::pipewire::{HEADSET_PROFILE, Snapshot};
use crate::transmit::{Mode, Transmit};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Transport {
	#[default]
	Hfp,
	ScoA2dp,
}

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

pub(crate) fn controller_warnings(
	devices: &BTreeMap<Address, Device>,
	sco_limit: usize,
) -> Vec<String> {
	addresses_by_controller(devices.iter())
		.into_iter()
		.filter(|(_, addresses)| addresses.len() > sco_limit)
		.map(|(controller, addresses)| {
			format!(
				"{} headsets share {controller} ({}), exceeding --sco-limit {sco_limit}. HFP listeners also need SCO/eSCO; verify controller capacity or use --mode half-duplex --transport sco-a2dp with PTT when sharing a radio.",
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

#[derive(Clone, Debug)]
pub struct Device {
	pub controller: String,
	pub hfp_available: bool,
	pub a2dp_available: bool,
	pub(crate) id: u64,
	pub(crate) serial: String,
	pub(crate) path: String,
	pub(crate) profiles: BTreeMap<u64, String>,
	a2dp_sbc: Option<u64>,
	a2dp_codec_info: bool,
	pub(crate) current: Option<u64>,
}

pub(crate) fn profile_matches(name: &str, family: &str) -> bool {
	name == family
		|| name
			.strip_prefix(family)
			.is_some_and(|suffix| suffix.starts_with('-'))
}

impl Device {
	pub(crate) fn index(&self, family: &str) -> Option<u64> {
		if family == "a2dp-sink" {
			return self.a2dp_sbc.or_else(|| {
				(!self.a2dp_codec_info)
					.then(|| self.generic_index(family))
					.flatten()
			});
		}
		self.generic_index(family)
	}

	fn generic_index(&self, family: &str) -> Option<u64> {
		self.current
			.filter(|index| {
				self.profiles
					.get(index)
					.is_some_and(|name| profile_matches(name, family))
			})
			.or_else(|| {
				self.profiles
					.iter()
					.find(|(_, name)| profile_matches(name, family))
					.map(|(index, _)| *index)
			})
	}

	pub(crate) fn is_profile(&self, family: &str) -> bool {
		if family == "a2dp-sink" && self.a2dp_codec_info {
			return self.a2dp_sbc.is_some() && self.a2dp_sbc == self.current;
		}
		self.current
			.and_then(|index| self.profiles.get(&index))
			.is_some_and(|name| profile_matches(name, family))
	}

	pub(crate) fn same_identity(&self, other: &Self) -> bool {
		self.id == other.id && self.serial == other.serial && self.path == other.path
	}
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

fn identifier(value: &Value) -> Option<String> {
	value
		.as_str()
		.map(str::to_owned)
		.or_else(|| value.as_u64().map(|n| n.to_string()))
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

/// Profile-transition coordinator, independent of the clients owning audio links.
pub(crate) struct Coordinator {
	pub(crate) transport: Transport,
	sco_limit: usize,
	pub(crate) transition: TransitionState,
	disconnects: BTreeSet<Address>,
}

impl Default for Coordinator {
	fn default() -> Self {
		Self {
			transport: Transport::default(),
			sco_limit: 1,
			transition: TransitionState::default(),
			disconnects: BTreeSet::new(),
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
			return Err("SCO/A2DP transport did not become ready within 30 seconds: expected HFP microphone/speaker ports for requested sources and A2DP speaker ports with no old HFP nodes for receivers; inspect pw-dump, Bluetooth connections and WirePlumber profile policy".into());
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
		validate_mixed_devices(&devices)?;
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

		if self.demote_receivers(sources, io, &snapshot, &devices)? {
			return Ok(false);
		}
		self.promote_sources(sources, io, &devices)?;
		Ok(self.sources_ready(sources, &headsets, &devices))
	}

	fn demote_receivers<F, C>(
		&mut self,
		sources: &BTreeSet<Address>,
		io: &mut TransitionIo<'_, F, C>,
		snapshot: &Snapshot,
		devices: &BTreeMap<Address, Device>,
	) -> Result<bool, String>
	where
		F: FnMut(&[&str]) -> Result<String, String>,
		C: FnMut(),
	{
		let mut demoting = false;
		for (address, device) in devices {
			if !sources.contains(address) {
				if !device.is_profile("a2dp-sink") {
					self.change_profile(address, device, "a2dp-sink", sources, io)?;
					demoting = true;
				} else if snapshot.has_hfp_node(device.id) {
					demoting = true;
				}
			}
		}
		Ok(demoting)
	}

	fn promote_sources<F, C>(
		&mut self,
		sources: &BTreeSet<Address>,
		io: &mut TransitionIo<'_, F, C>,
		devices: &BTreeMap<Address, Device>,
	) -> Result<(), String>
	where
		F: FnMut(&[&str]) -> Result<String, String>,
		C: FnMut(),
	{
		for address in sources {
			let device = &devices[address];
			if !device.is_profile(HEADSET_PROFILE) {
				self.change_profile(address, device, HEADSET_PROFILE, sources, io)?;
			}
		}
		Ok(())
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
					device.is_profile(HEADSET_PROFILE) && headset.has_duplex_audio()
				} else {
					device.is_profile("a2dp-sink") && !headset.sinks.is_empty()
				}
			})
		})
	}

	fn change_profile<F, C>(
		&mut self,
		address: &Address,
		expected: &Device,
		family: &str,
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
		validate_mixed_devices(&devices)?;
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
				if family == "a2dp-sink" {
					format!(
						"{address}: no advertised A2DP SBC profile; enable standard SBC in WirePlumber's Bluetooth codec settings"
					)
				} else {
					format!("{address}: {family} profile is no longer available")
				}
			})?;
		if current.is_profile(family) {
			return Ok(());
		}
		if family == HEADSET_PROFILE
			&& devices.iter().any(|(address, device)| {
				!sources.contains(address)
					&& (!device.is_profile("a2dp-sink") || snapshot.has_hfp_node(device.id))
			}) {
			return Ok(());
		}
		if let Some(pending) = self.transition.pending_profile(address)
			&& pending.device.same_identity(current)
			&& pending.index == index
		{
			if pending.requested_at.elapsed() >= Duration::from_secs(15) {
				return Err(format!(
					"{address}: timed out waiting for {family}; inspect pw-dump and Bluetooth connection"
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
		.map_err(|error| format!("{address}: could not select {family}: {error}"))?;
		self.transition
			.record_profile(address.clone(), current.clone(), index);
		Ok(())
	}
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

fn validate_mixed_devices(devices: &BTreeMap<Address, Device>) -> Result<(), String> {
	for (address, device) in devices {
		for family in [HEADSET_PROFILE, "a2dp-sink"] {
			if device.index(family).is_none() {
				return Err(if family == "a2dp-sink" {
					format!(
						"{address}: no available advertised A2DP SBC profile; SCO/A2DP requires standard SBC on every connected headset; enable it in WirePlumber's Bluetooth codec settings"
					)
				} else {
					format!(
						"{address}: no available advertised {family} profile; SCO/A2DP requires both HFP and A2DP on every connected headset; check headset support and PipeWire Bluetooth configuration"
					)
				});
			}
		}
	}
	Ok(())
}

impl Device {
	pub(crate) fn parse(id: u64, address: &Address, info: &Value) -> Option<Self> {
		let props = &info["props"];
		let path = props["api.bluez5.path"].as_str()?;
		let (controller, device_path) = path.rsplit_once('/')?;
		let hci = controller.strip_prefix("/org/bluez/hci")?;
		if hci.is_empty()
			|| !hci.bytes().all(|byte| byte.is_ascii_digit())
			|| device_path != format!("dev_{}", address.as_str().replace(':', "_"))
		{
			return None;
		}
		let serial = identifier(&props["object.serial"])?;
		let params = &info["params"];
		let profile_entries = params["EnumProfile"]
			.as_array()
			.into_iter()
			.flatten()
			.filter(|profile| {
				!matches!(profile["available"].as_str(), Some("no" | "unavailable"))
					&& profile["available"].as_u64() != Some(1)
					&& profile["available"].as_bool() != Some(false)
			})
			.collect::<Vec<_>>();
		let profiles: BTreeMap<_, _> = profile_entries
			.iter()
			.filter_map(|profile| {
				Some((
					profile["index"].as_u64()?,
					profile["name"].as_str()?.to_owned(),
				))
			})
			.collect();
		let a2dp_sbc = profile_entries.iter().find_map(|profile| {
			let name = profile["name"].as_str()?;
			let is_sbc = name == "a2dp-sink-sbc"
				|| (name == "a2dp-sink"
					&& profile["description"].as_str().is_some_and(|description| {
						description.to_ascii_lowercase().contains("codec sbc)")
					}));
			is_sbc.then(|| profile["index"].as_u64()).flatten()
		});
		let a2dp_codec_info = profile_entries.iter().any(|profile| {
			let Some(name) = profile["name"].as_str() else {
				return false;
			};
			profile_matches(name, "a2dp-sink")
				&& (name != "a2dp-sink"
					|| profile["description"].as_str().is_some_and(|description| {
						description.to_ascii_lowercase().contains("codec ")
					}))
		});
		let hfp_available = profiles
			.values()
			.any(|name| profile_matches(name, "headset-head-unit"));
		let a2dp_available = profiles
			.values()
			.any(|name| profile_matches(name, "a2dp-sink"));
		let current = params["Profile"]
			.as_array()
			.and_then(|profiles| profiles.first())
			.and_then(|profile| profile["index"].as_u64());
		Some(Self {
			controller: controller.into(),
			hfp_available,
			a2dp_available,
			id,
			serial,
			path: path.into(),
			profiles,
			a2dp_sbc,
			a2dp_codec_info,
			current,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	const ADDRESS: &str = "AA:BB:CC:DD:EE:01";

	fn address(value: &str) -> Address {
		Address::parse(value).unwrap()
	}

	fn device(profiles: Value, current: u64) -> Device {
		Device::parse(
			10,
			&address(ADDRESS),
			&json!({
				"props": {
					"api.bluez5.path": format!("/org/bluez/hci0/dev_{}", ADDRESS.replace(':', "_")),
					"object.serial": 1000
				},
				"params": {
					"EnumProfile": profiles,
					"Profile": [{"index": current}]
				}
			}),
		)
		.unwrap()
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
	fn a2dp_prefers_standard_sbc_over_other_codecs() {
		let device = device(
			json!([
				{"index": 20, "name": "a2dp-sink-aac", "description": "codec AAC", "available": "yes"},
				{"index": 21, "name": "a2dp-sink-sbc_xq", "description": "codec SBC XQ", "available": "yes"},
				{"index": 22, "name": "a2dp-sink-sbc", "description": "codec SBC", "available": "yes"}
			]),
			20,
		);

		assert_eq!(device.index("a2dp-sink"), Some(22));
		assert!(!device.is_profile("a2dp-sink"));
	}

	#[test]
	fn a2dp_recognizes_sbc_on_pipewire_base_profile() {
		let device = device(
			json!([{
				"index": 41,
				"name": "a2dp-sink",
				"description": "High Fidelity Playback (A2DP Sink, codec SBC)",
				"available": "yes"
			}]),
			41,
		);

		assert_eq!(device.index("a2dp-sink"), Some(41));
		assert!(device.is_profile("a2dp-sink"));
	}

	#[test]
	fn a2dp_does_not_choose_a_known_non_sbc_codec() {
		let device = device(
			json!([{
				"index": 40,
				"name": "a2dp-sink",
				"description": "High Fidelity Playback (A2DP Sink, codec AAC)",
				"available": "yes"
			}]),
			40,
		);

		assert_eq!(device.index("a2dp-sink"), None);
		assert!(!device.is_profile("a2dp-sink"));
	}
}
