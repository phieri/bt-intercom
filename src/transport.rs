//! Opt-in SCO microphone / A2DP receiver metadata and profile transitions.
//!
//! Profile changes are not restored on exit: restoring every HFP profile would
//! recreate the controller capacity conflicts this transport avoids.

use std::collections::{BTreeMap, BTreeSet};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::pipewire::{HEADSET_PROFILE, Snapshot};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Transport {
	#[default]
	Hfp,
	ScoA2dp,
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
		self.current
			.and_then(|index| self.profiles.get(&index))
			.is_some_and(|name| profile_matches(name, family))
	}

	pub(crate) fn same_identity(&self, other: &Self) -> bool {
		self.id == other.id && self.serial == other.serial && self.path == other.path
	}
}

fn identifier(value: &Value) -> Option<String> {
	value
		.as_str()
		.map(str::to_owned)
		.or_else(|| value.as_u64().map(|n| n.to_string()))
}

/// Profile-transition state, independent of the clients owning audio links.
#[derive(Default)]
pub(crate) struct Coordinator {
	pub(crate) transport: Transport,
	sources: Option<BTreeSet<String>>,
	pub(crate) profile_changes: BTreeMap<String, (Device, u64, Instant)>,
	pub(crate) transition_started: Option<Instant>,
	disconnects: BTreeSet<String>,
}

struct TransitionIo<'a, F, C> {
	allowed: &'a BTreeSet<String>,
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

	pub(crate) fn take_disconnects(&mut self) -> BTreeSet<String> {
		std::mem::take(&mut self.disconnects)
	}

	pub(crate) fn snapshot(
		&mut self,
		allowed: &BTreeSet<String>,
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
		sources: &BTreeSet<String>,
		allowed: &BTreeSet<String>,
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
			self.transition_started = None;
		} else {
			(io.close_routes)();
		}
		result
	}

	fn prepare_inner<F, C>(
		&mut self,
		sources: &BTreeSet<String>,
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
		if self.sources.as_ref() != Some(sources) {
			(io.close_routes)();
			self.sources = Some(sources.clone());
			self.transition_started = Some(Instant::now());
		}
		let started = self.transition_started.get_or_insert_with(Instant::now);
		if started.elapsed() >= Duration::from_secs(30) {
			return Err("SCO/A2DP transport did not become ready within 30 seconds: expected HFP microphone/speaker ports for requested sources and A2DP speaker ports with no old HFP nodes for receivers; inspect pw-dump, Bluetooth connections and WirePlumber profile policy".into());
		}
		if !sources.is_disjoint(&self.disconnects) {
			return Ok(false);
		}
		let snapshot = self.snapshot(io.allowed, io.execute)?;
		let devices = snapshot.transport_devices(io.allowed)?;
		self.profile_changes
			.retain(|address, (expected, index, _)| {
				devices.get(address).is_some_and(|current| {
					current.same_identity(expected) && current.current != Some(*index)
				})
			});
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
		let mut controllers = BTreeMap::new();
		for address in sources {
			let device = &devices[address];
			if let Some(previous) = controllers.insert(&device.controller, address) {
				return Err(format!(
					"SCO capacity conflict on {}: {previous} and {address}; select at most one source per controller",
					device.controller
				));
			}
		}
		validate_mixed_devices(&devices)?;
		// An accepted promotion can arrive after a floor change. Observe it
		// before issuing its demotion or another promotion.
		for (address, (_, _, started)) in &self.profile_changes {
			if started.elapsed() >= Duration::from_secs(15) {
				return Err(format!(
					"{address}: timed out waiting for the requested PipeWire profile; inspect pw-dump and Bluetooth connection"
				));
			}
		}
		if !self.profile_changes.is_empty() {
			return Ok(false);
		}
		let mut demoting = false;
		for (address, device) in &devices {
			if !sources.contains(address) {
				if !device.is_profile("a2dp-sink") {
					self.change_profile(address, device, "a2dp-sink", sources, io)?;
					demoting = true;
				} else if snapshot.has_hfp_node(device.id) {
					demoting = true;
				}
			}
		}
		if demoting {
			return Ok(false);
		}
		let mut ready = true;
		for address in sources {
			let device = &devices[address];
			if !device.is_profile(HEADSET_PROFILE) {
				self.change_profile(address, device, HEADSET_PROFILE, sources, io)?;
				ready = false;
			}
		}
		Ok(ready
			&& devices.iter().all(|(address, device)| {
				headsets.get(address).is_some_and(|headset| {
					if sources.contains(address) {
						device.is_profile(HEADSET_PROFILE) && headset.has_duplex_audio()
					} else {
						device.is_profile("a2dp-sink") && !headset.sinks.is_empty()
					}
				})
			}))
	}

	fn change_profile<F, C>(
		&mut self,
		address: &str,
		expected: &Device,
		family: &str,
		sources: &BTreeSet<String>,
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
		let mut controllers = BTreeSet::new();
		for address in sources {
			if !controllers.insert(&devices[address].controller) {
				return Err("SCO controller assignment changed before profile selection; retry discovery with one source per controller".into());
			}
		}
		let current = devices
			.get(address)
			.filter(|device| device.same_identity(expected))
			.ok_or_else(|| {
				format!("{address}: PipeWire device identity changed; retry discovery")
			})?;
		let index = current
			.index(family)
			.ok_or_else(|| format!("{address}: {family} profile is no longer available"))?;
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
		if let Some((device, pending, started)) = self.profile_changes.get(address)
			&& device.same_identity(current)
			&& *pending == index
		{
			if started.elapsed() >= Duration::from_secs(15) {
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
		self.profile_changes
			.insert(address.into(), (current.clone(), index, Instant::now()));
		Ok(())
	}
}

fn validate_mixed_devices(devices: &BTreeMap<String, Device>) -> Result<(), String> {
	for (address, device) in devices {
		for family in [HEADSET_PROFILE, "a2dp-sink"] {
			if device.index(family).is_none() {
				return Err(format!(
					"{address}: no available advertised {family} profile; SCO/A2DP requires both HFP and A2DP on every connected headset; check headset support and PipeWire Bluetooth configuration"
				));
			}
		}
	}
	Ok(())
}

impl Device {
	pub(crate) fn parse(id: u64, address: &str, info: &Value) -> Option<Self> {
		let props = &info["props"];
		let path = props["api.bluez5.path"].as_str()?;
		let (controller, device_path) = path.rsplit_once('/')?;
		let hci = controller.strip_prefix("/org/bluez/hci")?;
		if hci.is_empty()
			|| !hci.bytes().all(|byte| byte.is_ascii_digit())
			|| device_path != format!("dev_{}", address.replace(':', "_"))
		{
			return None;
		}
		let serial = identifier(&props["object.serial"])?;
		let params = &info["params"];
		let profiles: BTreeMap<_, _> = params["EnumProfile"]
			.as_array()
			.into_iter()
			.flatten()
			.filter(|profile| {
				!matches!(profile["available"].as_str(), Some("no" | "unavailable"))
					&& profile["available"].as_u64() != Some(1)
					&& profile["available"].as_bool() != Some(false)
			})
			.filter_map(|profile| {
				Some((
					profile["index"].as_u64()?,
					profile["name"].as_str()?.to_owned(),
				))
			})
			.collect();
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
			current,
		})
	}
}
