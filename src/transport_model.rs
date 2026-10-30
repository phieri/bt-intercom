// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Typed transport and PipeWire profile/device models.

use std::collections::BTreeMap;

use serde_json::Value;

use crate::bluez::Address;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ProfileFamily {
	Hfp,
	A2dp,
}

impl ProfileFamily {
	pub(crate) fn name(self) -> &'static str {
		match self {
			Self::Hfp => "headset-head-unit",
			Self::A2dp => "a2dp-sink",
		}
	}

	pub(crate) fn matches(self, profile: &str) -> bool {
		let family = self.name();
		profile == family
			|| profile
				.strip_prefix(family)
				.is_some_and(|suffix| suffix.starts_with('-'))
	}
}

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
	profiles: BTreeMap<u64, String>,
	a2dp_best: Option<u64>,
	pub(crate) current: Option<u64>,
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
		let a2dp_best = profile_entries
			.iter()
			.filter_map(|profile| {
				let name = profile["name"].as_str()?;
				ProfileFamily::A2dp.matches(name).then_some(())?;
				Some((a2dp_rank(name), profile["index"].as_u64()?))
			})
			.min()
			.map(|(_, index)| index);
		let hfp_available = profiles
			.values()
			.any(|name| ProfileFamily::Hfp.matches(name));
		let a2dp_available = profiles
			.values()
			.any(|name| ProfileFamily::A2dp.matches(name));
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
			a2dp_best,
			current,
		})
	}

	pub(crate) fn index(&self, family: ProfileFamily) -> Option<u64> {
		if family == ProfileFamily::A2dp {
			return self.a2dp_best;
		}
		self.generic_index(family)
	}

	fn generic_index(&self, family: ProfileFamily) -> Option<u64> {
		self.current
			.filter(|index| {
				self.profiles
					.get(index)
					.is_some_and(|name| family.matches(name))
			})
			.or_else(|| {
				self.profiles
					.iter()
					.find(|(_, name)| family.matches(name))
					.map(|(index, _)| *index)
			})
	}

	pub(crate) fn is_profile(&self, family: ProfileFamily) -> bool {
		if family == ProfileFamily::A2dp && self.a2dp_best.is_some() {
			return self.a2dp_best == self.current;
		}
		self.current
			.and_then(|index| self.profiles.get(&index))
			.is_some_and(|name| family.matches(name))
	}

	pub(crate) fn same_identity(&self, other: &Self) -> bool {
		self.id == other.id && self.serial == other.serial && self.path == other.path
	}
}

/// Lower is preferred: SBC suits low-latency mono voice, then other codecs
/// in order of quality, so headsets without SBC still work.
fn a2dp_rank(name: &str) -> u8 {
	match name.strip_prefix("a2dp-sink-").unwrap_or("sbc") {
		"sbc" => 0,
		"sbc_xq" => 1,
		"aac" => 2,
		_ => 3,
	}
}

fn identifier(value: &Value) -> Option<String> {
	value
		.as_str()
		.map(str::to_owned)
		.or_else(|| value.as_u64().map(|n| n.to_string()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	const ADDRESS: &str = "AA:BB:CC:DD:EE:01";

	fn address() -> Address {
		Address::parse(ADDRESS).unwrap()
	}

	fn device(profiles: Value, current: u64) -> Device {
		Device::parse(
			10,
			&address(),
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

	#[test]
	fn profile_families_match_variants_without_matching_neighbor_names() {
		assert!(ProfileFamily::Hfp.matches("headset-head-unit-msbc"));
		assert!(ProfileFamily::A2dp.matches("a2dp-sink-sbc_xq"));
		assert!(!ProfileFamily::Hfp.matches("headset-head-unitary"));
		assert!(!ProfileFamily::A2dp.matches("a2dp-sinkish"));
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

		assert_eq!(device.index(ProfileFamily::A2dp), Some(22));
		assert!(!device.is_profile(ProfileFamily::A2dp));
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

		assert_eq!(device.index(ProfileFamily::A2dp), Some(41));
		assert!(device.is_profile(ProfileFamily::A2dp));
	}

	#[test]
	fn a2dp_falls_back_to_other_codecs_without_sbc() {
		let device = device(
			json!([
				{"index": 40, "name": "a2dp-sink-ldac", "available": "yes"},
				{"index": 41, "name": "a2dp-sink-aac", "available": "yes"}
			]),
			40,
		);

		assert_eq!(device.index(ProfileFamily::A2dp), Some(41));
		assert!(!device.is_profile(ProfileFamily::A2dp));
	}
}
