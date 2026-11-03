// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! One typed view of each observed PipeWire dump, shared by routing and transport.

use std::collections::{BTreeMap, BTreeSet};

use serde_json::Value;

use crate::bluez::Address;
use crate::transport_model::{Device, ProfileFamily, Transport};

pub(crate) const OWNER_PROPERTY: &str = "bt-intercom.owner";
pub(crate) const MONO_CHANNEL: &str = "MONO";
/// A directed link, represented as `(output_port_id, input_port_id)`.
pub(crate) type Link = (u64, u64);

#[derive(Clone, Debug, PartialEq, Eq)]
/// A PipeWire audio port and the channel it carries.
pub struct Port {
	pub(crate) id: u64,
	pub(crate) node_id: Option<u64>,
	pub(crate) channel: String,
}

#[derive(Clone, Debug, Default)]
/// Audio ports belonging to one allowlisted Bluetooth headset, or to the
/// routing computer's own wired headset.
pub struct Headset {
	/// Microphone output ports exposed by the headset.
	pub sources: Vec<Port>,
	/// Speaker input ports exposed by the headset.
	pub sinks: Vec<Port>,
}

impl Headset {
	/// Returns whether PipeWire exposes both microphone and speaker ports.
	pub fn has_duplex_audio(&self) -> bool {
		!self.sources.is_empty() && !self.sinks.is_empty()
	}

	/// Returns the PipeWire node ID for playback to this headset.
	pub fn speaker_node(&self) -> Option<u64> {
		self.sinks.iter().find_map(|port| port.node_id)
	}
}

/// Overrides for the routing computer's wired headset, by PipeWire `node.name`.
/// An unset side follows PipeWire's default device when it is not Bluetooth.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HostSelection {
	pub(crate) source: Option<String>,
	pub(crate) sink: Option<String>,
}

#[derive(Debug)]
struct DeviceMetadata {
	address: Address,
	transport: Option<Device>,
}

#[derive(Debug)]
struct Node {
	device: Option<u64>,
	name: Option<String>,
	bluetooth: bool,
	class: Option<String>,
	profile: Option<String>,
	label: Option<String>,
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
	device_observations: Vec<(Address, Option<String>)>,
	nodes: BTreeMap<u64, Node>,
	ports: Vec<AudioPort>,
	defaults: BTreeMap<String, String>,
	links: BTreeSet<Link>,
	unowned_links: BTreeSet<Link>,
	owned_links: BTreeMap<String, BTreeSet<Link>>,
}

fn id(value: &Value) -> Option<u64> {
	value.as_u64().or_else(|| value.as_str()?.parse().ok())
}

fn text(props: &Value, key: &str) -> Option<String> {
	props[key].as_str().map(str::to_owned)
}

/// PipeWire reports a default device as `{"name": "node.name"}`, either parsed
/// or as the JSON text.
fn default_node_name(value: &Value) -> Option<String> {
	match value {
		Value::String(text) => default_node_name(&serde_json::from_str(text).ok()?),
		_ => value["name"].as_str().map(str::to_owned),
	}
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
			defaults: BTreeMap::new(),
			links: BTreeSet::new(),
			unowned_links: BTreeSet::new(),
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
						let Some(address) = Address::parse(&address).ok() else {
							continue;
						};
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
					let name = text(props, "node.name");
					let bluetooth = text(props, "device.api").as_deref() == Some("bluez5")
						|| name
							.as_deref()
							.is_some_and(|name| name.starts_with("bluez_"));
					snapshot.nodes.insert(
						object_id,
						Node {
							device: id(&props["device.id"]),
							name,
							bluetooth,
							class: text(props, "media.class"),
							profile: text(props, "api.bluez5.profile"),
							label: text(props, "application.name")
								.or_else(|| text(props, "node.description"))
								.or_else(|| text(props, "node.name")),
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
				Some("PipeWire:Interface:Metadata") => {
					if text(props, "metadata.name").as_deref() != Some("default") {
						continue;
					}
					for entry in object["metadata"].as_array().into_iter().flatten() {
						if let Some(key) = entry["key"].as_str()
							&& let Some(name) = default_node_name(&entry["value"])
						{
							snapshot.defaults.insert(key.to_owned(), name);
						}
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
						} else if object_id.is_some() {
							snapshot.unowned_links.insert(link);
						}
					}
				}
				_ => {}
			}
		}
		Ok(snapshot)
	}

	pub(crate) fn addresses(&self) -> BTreeSet<Address> {
		self.devices
			.values()
			.map(|device| device.address.clone())
			.collect()
	}

	pub(crate) fn transport_devices(
		&self,
		allowed: &BTreeSet<Address>,
	) -> Result<BTreeMap<Address, Device>, String> {
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
					.is_some_and(|p| ProfileFamily::Hfp.matches(p))
		})
	}

	pub(crate) fn topology(
		&self,
		allowed: &BTreeSet<Address>,
		transport: Transport,
	) -> (BTreeMap<Address, Headset>, BTreeSet<Link>) {
		(self.headsets(allowed, transport), self.links.clone())
	}

	pub(crate) fn headsets(
		&self,
		allowed: &BTreeSet<Address>,
		transport: Transport,
	) -> BTreeMap<Address, Headset> {
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
			if !ProfileFamily::Hfp.matches(profile)
				&& !(transport == Transport::ScoA2dp
					&& node.class.as_deref() == Some("Audio/Sink")
					&& ProfileFamily::A2dp.matches(profile))
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
		headsets
	}

	fn is_bluetooth(&self, node: &Node) -> bool {
		node.bluetooth
			|| node
				.device
				.is_some_and(|device| self.devices.contains_key(&device))
	}

	/// Finds the node carrying one side of the host's wired headset: the
	/// explicit `node.name`, else PipeWire's default if it is not Bluetooth,
	/// else the only physical non-Bluetooth device of that class.
	fn host_node(
		&self,
		explicit: Option<&str>,
		class: &str,
		default_key: &str,
		flag: &str,
	) -> Result<u64, String> {
		let of_class = || {
			self.nodes
				.iter()
				.filter(move |(_, node)| node.class.as_deref() == Some(class))
		};
		if let Some(wanted) = explicit {
			let (id, node) = of_class()
				.find(|(_, node)| node.name.as_deref() == Some(wanted))
				.ok_or_else(|| {
					format!("no PipeWire {class} node named {wanted} (see `pw-cli ls Node`)")
				})?;
			if self.is_bluetooth(node) {
				return Err(format!(
					"{flag} {wanted} is a Bluetooth device; the host needs a wired headset"
				));
			}
			return Ok(*id);
		}
		let usable = || of_class().filter(|(_, node)| !self.is_bluetooth(node));
		if let Some(default) = self.defaults.get(default_key)
			&& let Some((id, _)) = usable().find(|(_, node)| node.name.as_ref() == Some(default))
		{
			return Ok(*id);
		}
		let mut physical = usable().filter(|(_, node)| node.device.is_some());
		match (physical.next(), physical.next()) {
			(Some((id, _)), None) => Ok(*id),
			(None, _) => Err(format!(
				"no wired {class} node found; connect the headset or pass {flag} NODE_NAME"
			)),
			_ => Err(format!(
				"several wired {class} nodes and no usable PipeWire default; pass {flag} NODE_NAME"
			)),
		}
	}

	/// The routing computer's wired headset ports, or why they are unavailable.
	pub(crate) fn host_audio(&self, selection: &HostSelection) -> Result<Headset, String> {
		let source = self.host_node(
			selection.source.as_deref(),
			"Audio/Source",
			"default.audio.source",
			"--host-source",
		)?;
		let sink = self.host_node(
			selection.sink.as_deref(),
			"Audio/Sink",
			"default.audio.sink",
			"--host-sink",
		)?;
		let ports = |node: u64, direction: &str| -> Vec<Port> {
			self.ports
				.iter()
				.filter(|port| port.node == node && port.direction == direction)
				.map(|port| Port {
					id: port.id,
					node_id: Some(port.node),
					channel: port.channel.clone(),
				})
				.collect()
		};
		let headset = Headset {
			sources: ports(source, "out"),
			sinks: ports(sink, "in"),
		};
		if !headset.has_duplex_audio() {
			return Err("the wired headset's PipeWire nodes expose no audio ports yet".into());
		}
		Ok(headset)
	}

	/// Other programs' audio nodes linked to an allowlisted headset's nodes.
	/// Links created by any bt-intercom instance carry an owner and are skipped.
	pub(crate) fn foreign_users(
		&self,
		allowed: &BTreeSet<Address>,
	) -> BTreeMap<Address, BTreeSet<String>> {
		let node_of = |port: u64| self.ports.iter().find(|p| p.id == port).map(|p| p.node);
		let headset_of = |node: u64| {
			let device = self.nodes.get(&node)?.device?;
			let address = &self.devices.get(&device)?.address;
			allowed.contains(address).then(|| address.clone())
		};
		let mut users: BTreeMap<Address, BTreeSet<String>> = BTreeMap::new();
		for &(output, input) in &self.unowned_links {
			let (Some(from), Some(to)) = (node_of(output), node_of(input)) else {
				continue;
			};
			for (headset, peer) in [(from, to), (to, from)] {
				if let Some(address) = headset_of(headset)
					&& headset_of(peer).is_none()
				{
					let label = self
						.nodes
						.get(&peer)
						.and_then(|node| node.label.clone())
						.unwrap_or_else(|| format!("PipeWire node {peer}"));
					users.entry(address).or_default().insert(label);
				}
			}
		}
		users
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
		let allowed = [Address::parse(ADDRESS).unwrap()].into();
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
	fn foreign_users_exclude_owned_links() {
		let mut value = fixture();
		let allowed = [Address::parse(ADDRESS).unwrap()].into();
		let items = value.as_array_mut().unwrap();
		items.push(
			json!({"id":30,"type":"PipeWire:Interface:Node","info":{"props":{
			"application.name":"Music Player","media.class":"Stream/Output/Audio"}}}),
		);
		items.push(
			json!({"id":31,"type":"PipeWire:Interface:Port","info":{"props":{
			"node.id":30,"port.direction":"in"}}}),
		);
		items.push(json!({"id":32,"type":"PipeWire:Interface:Link","info":{
			"output-port-id":12,"input-port-id":31,"props":{}}}));
		let snapshot = Snapshot::parse(&Value::Array(items.clone())).unwrap();
		let users = snapshot.foreign_users(&allowed);
		assert_eq!(users[ADDRESS], ["Music Player".to_string()].into());
		assert!(
			Snapshot::parse(&fixture())
				.unwrap()
				.foreign_users(&allowed)
				.is_empty()
		);
	}

	#[test]
	fn unknown_references_do_not_create_ports_or_transport_metadata() {
		let mut value = fixture();
		value[1]["info"]["props"]["device.id"] = json!(999);
		let snapshot = Snapshot::parse(&value).unwrap();
		let allowed = [Address::parse(ADDRESS).unwrap()].into();
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
