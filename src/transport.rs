//! Opt-in SCO microphone / A2DP receiver transport metadata.
//!
//! Profile changes are not restored on exit: restoring every HFP profile would
//! recreate the controller capacity conflicts this transport avoids.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

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

pub(crate) fn devices(
	snapshot: &Value,
	allowed: &BTreeSet<String>,
) -> Result<BTreeMap<String, Device>, String> {
	let objects = snapshot
		.as_array()
		.ok_or("pw-dump output must be an array")?;
	let mut devices = BTreeMap::new();
	let mut observed = BTreeMap::new();
	for object in objects {
		if object["type"] != "PipeWire:Interface:Device" {
			continue;
		}
		let props = &object["info"]["props"];
		let Some(address) = props["api.bluez5.address"].as_str() else {
			continue;
		};
		let address = address.to_ascii_uppercase();
		if !allowed.contains(&address) {
			continue;
		}
		let observed_path = props["api.bluez5.path"]
			.as_str()
			.unwrap_or("<missing api.bluez5.path>");
		if let Some(previous) = observed.insert(address.clone(), observed_path.to_owned()) {
			return Err(format!(
				"{address}: multiple live PipeWire Device objects ({previous} and {observed_path}); disconnect the duplicate headset device or remove duplicate Bluetooth bonds; controller migration is not automatic"
			));
		}
		let Some(path) = props["api.bluez5.path"].as_str() else {
			continue;
		};
		let Some((controller, device_path)) = path.rsplit_once('/') else {
			continue;
		};
		let Some(hci) = controller.strip_prefix("/org/bluez/hci") else {
			continue;
		};
		if hci.is_empty()
			|| !hci.bytes().all(|byte| byte.is_ascii_digit())
			|| device_path != format!("dev_{}", address.replace(':', "_"))
		{
			continue;
		}
		let (Some(id), Some(serial)) = (object["id"].as_u64(), identifier(&props["object.serial"]))
		else {
			continue;
		};
		let params = &object["info"]["params"];
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
		devices.insert(
			address,
			Device {
				controller: controller.into(),
				hfp_available,
				a2dp_available,
				id,
				serial,
				path: path.into(),
				profiles,
				current,
			},
		);
	}
	Ok(devices)
}
