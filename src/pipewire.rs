//! One typed view of each observed PipeWire dump, shared by routing and transport.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::router::{Headset, Port};
use crate::transport::{Device, Transport, profile_matches};

pub(crate) const OWNER_PROPERTY: &str = "bt-intercom.owner";
pub(crate) const HEADSET_PROFILE: &str = "headset-head-unit";
pub(crate) const MONO_CHANNEL: &str = "MONO";
/// A directed link, represented as `(output_port_id, input_port_id)`.
pub(crate) type Link = (u64, u64);

#[derive(Debug)]
struct DeviceMetadata {
	address: String,
	transport: Option<Device>,
}

#[derive(Debug)]
struct Node {
	device: Option<u64>,
	class: Option<String>,
	profile: Option<String>,
}

#[derive(Debug)]
struct AudioPort {
	id: u64,
	node: u64,
	direction: String,
	channel: String,
}

/// Parsed metadata is never reused across fetches, especially identity rechecks.
#[derive(Debug)]
pub(crate) struct Snapshot {
	devices: BTreeMap<u64, DeviceMetadata>,
	device_observations: Vec<(String, Option<String>)>,
	nodes: BTreeMap<u64, Node>,
	ports: Vec<AudioPort>,
	links: BTreeSet<Link>,
	owned_links: BTreeMap<String, BTreeSet<Link>>,
}

fn id(value: &Value) -> Option<u64> {
	value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn text(props: &Value, key: &str) -> Option<String> {
	props[key].as_str().map(str::to_owned)
}

impl Snapshot {
	pub(crate) fn fetch(
		execute: &mut impl FnMut(&[&str]) -> Result<String, String>,
	) -> Result<Self, String> {
		let value = serde_json::from_str(&execute(&["pw-dump"])?)
			.map_err(|error| format!("invalid pw-dump JSON: {error}"))?;
		Self::parse(&value)
	}
}

impl Snapshot {
	pub(crate) fn parse(value: &Value) -> Result<Self, String> {
		let objects = value.as_array().ok_or("pw-dump output must be an array")?;
		let mut snapshot = Self {
			devices: BTreeMap::new(),
			device_observations: Vec::new(),
			nodes: BTreeMap::new(),
			ports: Vec::new(),
			links: BTreeSet::new(),
			owned_links: BTreeMap::new(),
		};
		let mut ids = BTreeSet::new();
		for object in objects {
			let object_id = id(&object["id"]);
			if let Some(object_id) = object_id
				&& !ids.insert(object_id)
			{
				return Err(format!("duplicate PipeWire object ID {object_id}"));
			}
			let info = &object["info"];
			let props = &info["props"];
			match object["type"].as_str() {
				Some("PipeWire:Interface:Device") => {
					if let Some(address) = text(props, "api.bluez5.address") {
						let address = address.to_ascii_uppercase();
						snapshot
							.device_observations
							.push((address.clone(), text(props, "api.bluez5.path")));
						let Some(object_id) = object_id else { continue };
						snapshot.devices.insert(
							object_id,
							DeviceMetadata {
								transport: Device::parse(object_id, &address, info),
								address,
							},
						);
					}
				}
				Some("PipeWire:Interface:Node") => {
					let Some(object_id) = object_id else { continue };
					snapshot.nodes.insert(
						object_id,
						Node {
							device: id(&props["device.id"]),
							class: text(props, "media.class"),
							profile: text(props, "api.bluez5.profile"),
						},
					);
				}
				Some("PipeWire:Interface:Port") => {
					let Some(object_id) = object_id else { continue };
					if let Some(node) = id(&props["node.id"]) {
						snapshot.ports.push(AudioPort {
							id: object_id,
							node,
							direction: text(props, "port.direction").unwrap_or_default(),
							channel: text(props, "audio.channel")
								.unwrap_or_else(|| MONO_CHANNEL.into()),
						});
					}
				}
				Some("PipeWire:Interface:Link") => {
					if let (Some(output), Some(input)) =
						(id(&info["output-port-id"]), id(&info["input-port-id"]))
					{
						let link = (output, input);
						if object_id.is_some() {
							snapshot.links.insert(link);
						}
						if let Some(owner) = text(props, OWNER_PROPERTY) {
							snapshot.owned_links.entry(owner).or_default().insert(link);
						}
					}
				}
				_ => {}
			}
		}
		Ok(snapshot)
	}

	pub(crate) fn addresses(&self) -> BTreeSet<String> {
		self.devices
			.values()
			.map(|device| device.address.clone())
			.collect()
	}

	pub(crate) fn transport_devices(
		&self,
		allowed: &BTreeSet<String>,
	) -> Result<BTreeMap<String, Device>, String> {
		let mut devices = BTreeMap::new();
		let mut observed = BTreeMap::new();
		for (address, path) in &self.device_observations {
			if !allowed.contains(address) {
				continue;
			}
			let path = path.as_deref().unwrap_or("<missing api.bluez5.path>");
			if let Some(previous) = observed.insert(address, path) {
				return Err(format!(
					"{address}: multiple live PipeWire Device objects ({previous} and {path}); disconnect the duplicate headset device or remove duplicate Bluetooth bonds; controller migration is not automatic"
				));
			}
		}
		for device in self
			.devices
			.values()
			.filter(|d| allowed.contains(&d.address))
		{
			if let Some(transport) = &device.transport {
				devices.insert(device.address.clone(), transport.clone());
			}
		}
		Ok(devices)
	}

	pub(crate) fn has_hfp_node(&self, device: u64) -> bool {
		self.nodes.values().any(|node| {
			node.device == Some(device)
				&& node
					.profile
					.as_deref()
					.is_some_and(|p| profile_matches(p, HEADSET_PROFILE))
		})
	}

	pub(crate) fn topology(
		&self,
		allowed: &BTreeSet<String>,
		transport: Transport,
	) -> (BTreeMap<String, Headset>, BTreeSet<Link>) {
		let mut headsets: BTreeMap<_, _> = self
			.devices
			.values()
			.filter(|device| allowed.contains(&device.address))
			.map(|device| (device.address.clone(), Headset::default()))
			.collect();
		for port in &self.ports {
			let Some(node) = self.nodes.get(&port.node) else {
				continue;
			};
			let Some(device) = node.device.and_then(|id| self.devices.get(&id)) else {
				continue;
			};
			let Some(headset) = headsets.get_mut(&device.address) else {
				continue;
			};
			let Some(profile) = node.profile.as_deref() else {
				continue;
			};
			if !profile_matches(profile, HEADSET_PROFILE)
				&& !(transport == Transport::ScoA2dp
					&& node.class.as_deref() == Some("Audio/Sink")
					&& profile_matches(profile, "a2dp-sink"))
			{
				continue;
			}
			let ports = match (node.class.as_deref(), port.direction.as_str()) {
				(Some("Audio/Source"), "out") => &mut headset.sources,
				(Some("Audio/Sink"), "in") => &mut headset.sinks,
				_ => continue,
			};
			ports.push(Port {
				id: port.id,
				node_id: Some(port.node),
				channel: port.channel.clone(),
			});
		}
		(headsets, self.links.clone())
	}

	pub(crate) fn owned_links(&self, owner: &str) -> BTreeSet<Link> {
		self.owned_links.get(owner).cloned().unwrap_or_default()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	const ADDRESS: &str = "AA:BB:CC:DD:EE:01";

	fn fixture() -> Value {
		json!([
			{"id":"10","type":"PipeWire:Interface:Device","info":{
				"props":{"api.bluez5.address":"aa:bb:cc:dd:ee:01",
					"api.bluez5.path":"/org/bluez/hci0/dev_AA_BB_CC_DD_EE_01","object.serial":100},
				"params":{"EnumProfile":[{"index":17,"name":"headset-head-unit-msbc"}],
					"Profile":[{"index":17}]}}},
			{"id":11,"type":"PipeWire:Interface:Node","info":{"props":{
				"device.id":10,"media.class":"Audio/Source","api.bluez5.profile":"headset-head-unit-msbc"}}},
			{"id":"12","type":"PipeWire:Interface:Port","info":{"props":{
				"node.id":"11","port.direction":"out"}}},
			{"id":20,"type":"PipeWire:Interface:Link","info":{
				"output-port-id":"12","input-port-id":99,"props":{"bt-intercom.owner":"owner"}}}
		])
	}

	#[test]
	fn metadata_is_consistent_across_topology_transport_and_ownership() {
		let snapshot = Snapshot::parse(&fixture()).unwrap();
		let allowed = [ADDRESS.to_string()].into();
		let devices = snapshot.transport_devices(&allowed).unwrap();
		let (headsets, links) = snapshot.topology(&allowed, Transport::Hfp);
		assert_eq!(snapshot.addresses(), allowed);
		assert_eq!(devices[ADDRESS].id, 10);
		assert_eq!(devices[ADDRESS].serial, "100");
		assert_eq!(devices[ADDRESS].controller, "/org/bluez/hci0");
		assert!(devices[ADDRESS].hfp_available);
		assert!(snapshot.has_hfp_node(devices[ADDRESS].id));
		assert_eq!(headsets[ADDRESS].sources[0].id, 12);
		assert_eq!(headsets[ADDRESS].sources[0].node_id, Some(11));
		assert_eq!(headsets[ADDRESS].sources[0].channel, MONO_CHANNEL);
		assert_eq!(links, [(12, 99)].into());
		assert_eq!(snapshot.owned_links("owner"), links);
		assert!(snapshot.owned_links("other").is_empty());
	}

	#[test]
	fn unknown_references_do_not_create_ports_or_transport_metadata() {
		let mut value = fixture();
		value[1]["info"]["props"]["device.id"] = json!(999);
		let snapshot = Snapshot::parse(&value).unwrap();
		let allowed = [ADDRESS.to_string()].into();
		assert!(
			snapshot.topology(&allowed, Transport::Hfp).0[ADDRESS]
				.sources
				.is_empty()
		);
		assert!(!snapshot.has_hfp_node(10));
		value[1]["info"]["props"]["device.id"] = json!(10);
		value[2]["info"]["props"]["node.id"] = json!(999);
		value[0]["info"]["props"]["api.bluez5.path"] = json!("/org/bluez/hci0/dev_WRONG");
		let snapshot = Snapshot::parse(&value).unwrap();
		assert!(
			snapshot.topology(&allowed, Transport::Hfp).0[ADDRESS]
				.sources
				.is_empty()
		);
		assert!(snapshot.transport_devices(&allowed).unwrap().is_empty());
	}

	#[test]
	fn repeated_links_are_deduplicated_without_losing_owner_tags() {
		let mut value = fixture();
		let mut duplicate = value[3].clone();
		duplicate["id"] = json!(21);
		duplicate["info"]["props"]["bt-intercom.owner"] = json!("other");
		value.as_array_mut().unwrap().push(duplicate);
		let snapshot = Snapshot::parse(&value).unwrap();
		assert_eq!(snapshot.links.len(), 1);
		assert_eq!(snapshot.owned_links("owner"), snapshot.owned_links("other"));
	}

	#[test]
	fn owner_tags_remain_observable_without_a_link_object_id() {
		let mut value = fixture();
		value[3].as_object_mut().unwrap().remove("id");
		let snapshot = Snapshot::parse(&value).unwrap();
		assert_eq!(snapshot.owned_links("owner"), [(12, 99)].into());
		assert!(snapshot.links.is_empty());
	}

	#[test]
	fn duplicate_object_ids_fail_closed_even_with_different_representations() {
		let mut value = fixture();
		value[1]["id"] = json!(10);
		assert!(
			Snapshot::parse(&value)
				.unwrap_err()
				.contains("duplicate PipeWire object ID 10")
		);
		assert!(Snapshot::parse(&json!({})).is_err());
	}

	#[test]
	fn duplicate_addresses_still_fail_transport_validation_with_missing_metadata() {
		let mut value = fixture();
		value.as_array_mut().unwrap().push(json!({
			"id":30,"type":"PipeWire:Interface:Device",
			"info":{"props":{"api.bluez5.address":ADDRESS}}
		}));
		let snapshot = Snapshot::parse(&value).unwrap();
		assert!(
			snapshot
				.transport_devices(&[ADDRESS.into()].into())
				.unwrap_err()
				.contains("multiple live PipeWire Device objects")
		);
		assert_eq!(
			snapshot
				.topology(&[ADDRESS.into()].into(), Transport::Hfp)
				.0
				.len(),
			1
		);
		value
			.as_array_mut()
			.unwrap()
			.last_mut()
			.unwrap()
			.as_object_mut()
			.unwrap()
			.remove("id");
		assert!(
			Snapshot::parse(&value)
				.unwrap()
				.transport_devices(&[ADDRESS.into()].into())
				.unwrap_err()
				.contains("multiple live PipeWire Device objects")
		);
	}
}
