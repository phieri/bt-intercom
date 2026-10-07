// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! PipeWire topology discovery and ownership-safe inter-headset audio routing.
//!
//! Routing is limited to allowlisted Bluetooth devices using HFP microphones
//! and HFP speakers, or opt-in A2DP receivers. Each link has a `pw-cli` owner, so
//! dropping that client releases the link without relying on reusable object IDs.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

#[cfg(test)]
use serde_json::Value;

use crate::groups::TalkGroup;
use crate::pipewire::{Link, MONO_CHANNEL, OWNER_PROPERTY, Snapshot};
use crate::process::{COMMAND_TIMEOUT, command};
#[cfg(test)]
use crate::routing_status::RoutingStatus;
use crate::transport::{self, Transport};

static NEXT_ROUTER: AtomicUsize = AtomicUsize::new(0);

/// A live `pw-cli` client that owns a PipeWire link.
trait LinkHandle {
	fn is_running(&mut self) -> Result<bool, String>;
}

/// The production link handle; dropping it terminates its owning `pw-cli`.
struct LinkProcess(Child);

impl LinkHandle for LinkProcess {
	fn is_running(&mut self) -> Result<bool, String> {
		self.0
			.try_wait()
			.map(|status| status.is_none())
			.map_err(|error| error.to_string())
	}
}

impl Drop for LinkProcess {
	fn drop(&mut self) {
		let _ = self.0.kill();
		let _ = self.0.wait();
	}
}

type StartLink = Box<dyn FnMut(&[&str]) -> Result<Box<dyn LinkHandle>, String>>;

#[derive(Debug, PartialEq, Eq)]
/// A PipeWire audio port and the channel it carries.
pub struct Port {
	pub(crate) id: u64,
	pub(crate) node_id: Option<u64>,
	pub(crate) channel: String,
}

#[derive(Debug, Default)]
/// Audio ports belonging to one allowlisted Bluetooth headset.
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

/// Extract allowlisted headset ports and existing links from a `pw-dump` snapshot.
///
/// Nodes are included only when they belong to a listed Bluetooth device and
/// use PipeWire's `headset-head-unit` profile.
#[cfg(test)]
pub fn topology(
	objects: &Value,
	allowed: &BTreeSet<String>,
) -> Result<(BTreeMap<String, Headset>, BTreeSet<Link>), String> {
	Ok(Snapshot::parse(objects)?.topology(allowed, Transport::Hfp))
}

/// Build cross-headset source-to-sink links whose channels are compatible.
///
/// A headset is never linked to itself; mono ports are compatible with every
/// channel on the opposite endpoint.
#[cfg(test)]
pub fn desired_links(headsets: &BTreeMap<String, Headset>) -> BTreeSet<Link> {
	desired_links_with_policy(headsets, None, None)
}

pub(crate) fn channels_compatible(output: &Port, input: &Port) -> bool {
	output.channel == input.channel
		|| output.channel == MONO_CHANNEL
		|| input.channel == MONO_CHANNEL
}

fn desired_links_with_policy(
	headsets: &BTreeMap<String, Headset>,
	allowed_pairs: Option<&BTreeMap<String, BTreeSet<String>>>,
	active_sources: Option<&BTreeSet<String>>,
) -> BTreeSet<Link> {
	let mut desired = BTreeSet::new();
	for (source_address, source) in headsets {
		if active_sources.is_some_and(|sources| !sources.contains(source_address)) {
			continue;
		}
		for (sink_address, sink) in headsets {
			if source_address == sink_address
				|| allowed_pairs.is_some_and(|pairs| {
					!pairs
						.get(source_address)
						.is_some_and(|sinks| sinks.contains(sink_address))
				}) {
				continue;
			}
			for output in &source.sources {
				for input in &sink.sinks {
					if channels_compatible(output, input) {
						desired.insert((output.id, input.id));
					}
				}
			}
		}
	}
	desired
}

fn group_pairs(groups: &[TalkGroup]) -> BTreeMap<String, BTreeSet<String>> {
	let mut pairs = BTreeMap::new();
	for group in groups {
		for source in &group.members {
			pairs
				.entry(source.clone())
				.or_insert_with(BTreeSet::new)
				.extend(group.members.iter().filter(|sink| *sink != source).cloned());
		}
	}
	pairs
}

/// Build routes only between headset pairs that share a talk group.
///
/// An empty group list retains the original all-to-all behavior.
#[cfg(test)]
pub fn desired_links_in_groups(
	headsets: &BTreeMap<String, Headset>,
	groups: &[TalkGroup],
) -> BTreeSet<Link> {
	if groups.is_empty() {
		return desired_links(headsets);
	}
	let allowed_pairs = group_pairs(groups);
	desired_links_with_policy(headsets, Some(&allowed_pairs), None)
}

/// Build routes only for sources granted a floor in the corresponding group.
///
/// With no configured groups, the `None` entry represents the shared floor.
pub fn desired_links_by_group(
	headsets: &BTreeMap<String, Headset>,
	group_sources: &BTreeMap<Option<String>, BTreeSet<String>>,
	groups: &[TalkGroup],
) -> BTreeSet<Link> {
	if groups.is_empty() {
		let sources = group_sources.get(&None).cloned().unwrap_or_default();
		return desired_links_with_policy(headsets, None, Some(&sources));
	}

	groups
		.iter()
		.flat_map(|group| {
			let Some(sources) = group_sources.get(&Some(group.name.clone())) else {
				return BTreeSet::new();
			};
			let allowed_pairs = group_pairs(std::slice::from_ref(group));
			desired_links_with_policy(headsets, Some(&allowed_pairs), Some(sources))
		})
		.collect()
}

/// Returns whether a headset's microphone has a live route to another headset.
#[cfg(test)]
pub fn has_active_source_route(
	address: &str,
	headsets: &BTreeMap<String, Headset>,
	links: &BTreeSet<(u64, u64)>,
) -> bool {
	RoutingStatus::new(headsets, links).has_active_source_route(address)
}

/// Returns whether a headset has live microphone and speaker routes with a peer.
#[cfg(test)]
pub fn has_active_intercom_connection(
	address: &str,
	headsets: &BTreeMap<String, Headset>,
	links: &BTreeSet<(u64, u64)>,
) -> bool {
	RoutingStatus::new(headsets, links).has_active_intercom_connection(address)
}

/// Reconciles desired headset routes with PipeWire while tracking owned links.
pub struct Router<F = fn(&[&str]) -> Result<String, String>> {
	/// Bluetooth addresses that may be discovered or routed.
	pub allowed: BTreeSet<String>,
	owner: String,
	execute: F,
	start_link: StartLink,
	owned: BTreeMap<Link, (Box<dyn LinkHandle>, Instant)>,
	coordinator: transport::Coordinator,
}

fn default_command(args: &[&str]) -> Result<String, String> {
	command(args, COMMAND_TIMEOUT)
}

fn start_link(args: &[&str]) -> Result<Box<dyn LinkHandle>, String> {
	let Some(program) = args.first().copied().filter(|program| !program.is_empty()) else {
		return Err("cannot start PipeWire link without a command".into());
	};
	Command::new(program)
		.args(&args[1..])
		.stdin(Stdio::null())
		.stdout(Stdio::null())
		.stderr(Stdio::inherit())
		.spawn()
		.map(|child| Box::new(LinkProcess(child)) as Box<dyn LinkHandle>)
		.map_err(|error| error.to_string())
}

impl Router {
	/// Creates a router using the system `pw-dump` and `pw-cli` commands.
	pub fn new(allowed: BTreeSet<String>) -> Self {
		Self::with_executor(allowed, default_command)
	}
}

impl<F: FnMut(&[&str]) -> Result<String, String>> Router<F> {
	/// Creates a router with a custom command executor.
	///
	/// The executor is useful for tests and must return the command's stdout
	/// or an error. Link creation still uses the production `pw-cli` backend.
	pub fn with_executor(allowed: BTreeSet<String>, execute: F) -> Self {
		Self::with_backend(allowed, execute, Box::new(start_link))
	}

	fn with_backend(allowed: BTreeSet<String>, execute: F, start_link: StartLink) -> Self {
		Self {
			allowed,
			owner: format!(
				"{}-{}-{}",
				std::process::id(),
				SystemTime::now()
					.duration_since(UNIX_EPOCH)
					.unwrap_or_default()
					.as_nanos(),
				NEXT_ROUTER.fetch_add(1, Ordering::Relaxed)
			),
			execute,
			start_link,
			owned: BTreeMap::new(),
			coordinator: transport::Coordinator::default(),
		}
	}

	pub fn set_transport(&mut self, transport: Transport) {
		if self.coordinator.transport != transport {
			self.close();
			self.coordinator = transport::Coordinator::new(transport);
		}
	}

	pub fn set_sco_limit(&mut self, limit: usize) {
		self.coordinator.set_sco_limit(limit);
	}

	pub fn transport_devices(&mut self) -> Result<BTreeMap<String, transport::Device>, String> {
		let snapshot = self.snapshot()?;
		snapshot.transport_devices(&self.allowed)
	}

	/// Consume observed allowlisted device disappearances, even after reconnect.
	pub fn take_transport_disconnects(&mut self) -> BTreeSet<String> {
		self.coordinator.take_disconnects()
	}

	/// Close old routes, observe all receiver demotions, then enable SCO sources.
	/// Commands are asynchronous; callers must retry until profiles and ports
	/// are observed ready, with a 30-second deadline for the entire transition.
	/// Last selected profiles are intentionally left on exit.
	pub fn prepare_transport(&mut self, sources: &BTreeSet<String>) -> Result<bool, String> {
		self.coordinator
			.prepare(sources, &self.allowed, &mut self.execute, || {
				self.owned.clear()
			})
	}

	/// Routes every allowlisted microphone when transmitting, or removes all
	/// owned routes when muted.
	pub fn update(&mut self, transmitting: bool) -> Result<BTreeMap<String, Headset>, String> {
		let sources = if transmitting {
			self.allowed.clone()
		} else {
			BTreeSet::new()
		};
		self.update_sources(&sources)
	}

	/// Reconciles owned routes so only the specified headsets' microphones transmit.
	///
	/// Returns the currently discovered headset topology, or combined errors
	/// from link ownership checks and link creation.
	pub fn update_sources(
		&mut self,
		sources: &BTreeSet<String>,
	) -> Result<BTreeMap<String, Headset>, String> {
		self.update_sources_in_groups(sources, &[])
	}

	/// Reconciles routes from the specified microphones within shared groups.
	pub fn update_sources_in_groups(
		&mut self,
		sources: &BTreeSet<String>,
		groups: &[TalkGroup],
	) -> Result<BTreeMap<String, Headset>, String> {
		let group_sources = if groups.is_empty() {
			BTreeMap::from([(None, sources.clone())])
		} else {
			groups
				.iter()
				.map(|group| (Some(group.name.clone()), sources.clone()))
				.collect()
		};
		self.update_group_sources_in_groups(&group_sources, groups)
	}

	/// Reconciles routes for sources granted a floor in each talk group.
	pub fn update_group_sources_in_groups(
		&mut self,
		group_sources: &BTreeMap<Option<String>, BTreeSet<String>>,
		groups: &[TalkGroup],
	) -> Result<BTreeMap<String, Headset>, String> {
		let sources = group_sources.values().flatten().cloned().collect();
		if !self.prepare_transport(&sources)? {
			log::debug!("Transport not ready for sources {sources:?}; releasing owned links");
			self.close();
			return self.inspect().map(|(headsets, _)| headsets);
		}
		let snapshot = self.snapshot()?;
		let (headsets, existing) = snapshot.topology(&self.allowed, self.coordinator.transport);
		let desired = desired_links_by_group(&headsets, group_sources, groups);
		log::debug!(
			"Routing: sources {group_sources:?}, {} headsets, {} desired links, {} observed links, {} owned links",
			headsets.len(),
			desired.len(),
			existing.len(),
			self.owned.len()
		);
		let mut failures = Vec::new();
		let live = snapshot.owned_links(&self.owner);
		self.owned.retain(|link, (handle, started)| {
			if !desired.contains(link) {
				log::debug!(
					"Releasing link {} -> {} (no longer desired)",
					link.0,
					link.1
				);
				return false;
			}
			match handle.is_running() {
				Ok(true) if live.contains(link) || started.elapsed() < Duration::from_secs(15) => {
					true
				}
				Ok(true) => {
					failures.push(format!("Link did not appear for {} -> {}", link.0, link.1));
					false
				}
				Ok(false) => {
					failures.push(format!("Link process exited for {} -> {}", link.0, link.1));
					false
				}
				Err(error) => {
					failures.push(error);
					false
				}
			}
		});
		let properties = serde_json::json!({
			OWNER_PROPERTY: self.owner,
			"object.linger": false,
		})
		.to_string();
		for &(output, input) in desired.difference(&existing) {
			if self.owned.contains_key(&(output, input)) {
				continue;
			}
			match (self.start_link)(&[
				"pw-cli",
				"-m",
				"create-link",
				"-",
				&output.to_string(),
				"-",
				&input.to_string(),
				&properties,
			]) {
				Ok(handle) => {
					log::debug!("Creating link {output} -> {input}");
					self.owned.insert((output, input), (handle, Instant::now()));
				}
				Err(error) => {
					failures.push(format!("Could not link ports {output} -> {input}: {error}"))
				}
			}
		}
		if failures.is_empty() {
			Ok(headsets)
		} else {
			Err(failures.join("; "))
		}
	}

	/// Reads the current allowlisted headset topology and all observed links.
	pub fn inspect(&mut self) -> Result<(BTreeMap<String, Headset>, BTreeSet<Link>), String> {
		let snapshot = self.snapshot()?;
		Ok(snapshot.topology(&self.allowed, self.coordinator.transport))
	}

	/// Reads headset topology and only links owned by this router instance.
	pub fn inspect_owned(&mut self) -> Result<(BTreeMap<String, Headset>, BTreeSet<Link>), String> {
		let snapshot = self.snapshot()?;
		let (headsets, _) = snapshot.topology(&self.allowed, self.coordinator.transport);
		Ok((headsets, snapshot.owned_links(&self.owner)))
	}

	fn snapshot(&mut self) -> Result<Snapshot, String> {
		self.coordinator.snapshot(&self.allowed, &mut self.execute)
	}

	/// Releases every link created by this router.
	pub fn close(&mut self) {
		self.owned.clear();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;
	use std::cell::RefCell;
	use std::rc::Rc;

	const A: &str = "AA:BB:CC:DD:EE:01";
	const B: &str = "AA:BB:CC:DD:EE:02";
	const C: &str = "AA:BB:CC:DD:EE:03";

	fn headset(base: u64, address: &str) -> Vec<Value> {
		vec![
			json!({"type":"PipeWire:Interface:Device","id":base,"info":{"props":{"api.bluez5.address":address}}}),
			json!({"type":"PipeWire:Interface:Node","id":base+1,"info":{"props":{"device.id":base.to_string(),"media.class":"Audio/Source","api.bluez5.profile":"headset-head-unit"}}}),
			json!({"type":"PipeWire:Interface:Node","id":base+2,"info":{"props":{"device.id":base.to_string(),"media.class":"Audio/Sink","api.bluez5.profile":"headset-head-unit"}}}),
			json!({"type":"PipeWire:Interface:Port","id":base+3,"info":{"props":{"node.id":(base+1).to_string(),"port.direction":"out","audio.channel":"MONO"}}}),
			json!({"type":"PipeWire:Interface:Port","id":base+4,"info":{"props":{"node.id":(base+2).to_string(),"port.direction":"in","audio.channel":"FL"}}}),
			json!({"type":"PipeWire:Interface:Port","id":base+5,"info":{"props":{"node.id":(base+2).to_string(),"port.direction":"in","audio.channel":"FR"}}}),
		]
	}

	fn fixture() -> Vec<Value> {
		[headset(10, A), headset(20, B)].concat()
	}

	fn allowed() -> BTreeSet<String> {
		[A.to_string(), B.to_string()].into()
	}

	fn transport_fixture() -> Vec<Value> {
		let mut objects = fixture();
		for object in &mut objects {
			if object["type"] == "PipeWire:Interface:Device" {
				let address = object["info"]["props"]["api.bluez5.address"]
					.as_str()
					.unwrap()
					.to_owned();
				object["info"]["props"]["api.bluez5.path"] =
					json!(format!("/org/bluez/hci0/dev_{}", address.replace(':', "_")));
				object["info"]["props"]["object.serial"] =
					json!(object["id"].as_u64().unwrap() + 1000);
				object["info"]["params"] = json!({
					"EnumProfile": [
						{"index":17,"name":"headset-head-unit-msbc","available":"yes"},
						{"index":41,"name":"a2dp-sink-sbc","available":"yes"}
					],
					"Profile":[{"index":17}]
				});
			}
		}
		objects
	}

	fn observe_profile(server: &PipeWire, base: u64, hfp: bool) {
		let mut objects = server.objects.borrow_mut();
		for object in objects.iter_mut() {
			if object["id"] == base {
				object["info"]["params"]["Profile"] = json!([{"index":if hfp {17} else {41}}]);
			}
			if object["id"] == base + 1 {
				object["info"]["props"]["api.bluez5.profile"] = json!(if hfp {
					"headset-head-unit-msbc"
				} else {
					"a2dp-sink-sbc"
				});
			}
			if object["id"] == base + 2 {
				object["info"]["props"]["api.bluez5.profile"] = json!(if hfp {
					"headset-head-unit-msbc"
				} else {
					"a2dp-sink-sbc"
				});
			}
		}
	}

	#[test]
	fn sco_a2dp_waits_for_demotion_before_promoting_a_new_floor() {
		let server = PipeWire::new();
		*server.objects.borrow_mut() = transport_fixture();
		let mut router = server.router();
		router.set_transport(Transport::ScoA2dp);
		let a = [A.to_string()].into();
		let b = [B.to_string()].into();
		assert!(!router.prepare_transport(&a).unwrap());
		assert!(!router.prepare_transport(&a).unwrap());
		let changes = || {
			server
				.calls
				.borrow()
				.iter()
				.filter(|call| call.get(1).is_some_and(|arg| arg == "set-param"))
				.cloned()
				.collect::<Vec<_>>()
		};
		assert_eq!(changes().len(), 1);
		assert_eq!(changes()[0][2], "20");
		assert_eq!(changes()[0][4], "{ index: 41, save: false }");
		// A Profile acknowledgement alone is insufficient while old HFP nodes remain.
		server.objects.borrow_mut()[6]["info"]["params"]["Profile"] = json!([{"index":41}]);
		assert!(!router.prepare_transport(&a).unwrap());
		observe_profile(&server, 20, false);
		assert!(router.prepare_transport(&a).unwrap());
		let (headsets, _) = router.inspect().unwrap();
		assert!(headsets[B].sources.is_empty());
		assert!(!headsets[B].sinks.is_empty());
		router.update_sources(&a).unwrap();
		assert_eq!(
			router.inspect_owned().unwrap().1,
			[(13, 24), (13, 25)].into()
		);
		assert!(!router.prepare_transport(&b).unwrap());
		assert_eq!(server.links(), 0);
		assert_eq!(changes().len(), 2);
		assert_eq!(changes()[1][2], "10");
		assert!(!router.prepare_transport(&b).unwrap());
		assert_eq!(changes().len(), 2);
		observe_profile(&server, 10, false);
		assert!(!router.prepare_transport(&b).unwrap());
		assert_eq!(changes()[2][2], "20");
		assert_eq!(changes()[2][4], "{ index: 17, save: false }");
		assert!(!router.prepare_transport(&b).unwrap());
		observe_profile(&server, 20, true);
		assert!(router.prepare_transport(&b).unwrap());
	}

	#[test]
	fn changed_floor_waits_for_a_previous_in_flight_promotion() {
		let server = PipeWire::new();
		*server.objects.borrow_mut() = transport_fixture();
		observe_profile(&server, 10, false);
		observe_profile(&server, 20, false);
		let mut router = server.router();
		router.set_transport(Transport::ScoA2dp);
		assert!(!router.prepare_transport(&[A.to_string()].into()).unwrap());
		assert!(!router.prepare_transport(&[B.to_string()].into()).unwrap());
		let changes = || {
			server
				.calls
				.borrow()
				.iter()
				.filter(|call| call.get(1).is_some_and(|arg| arg == "set-param"))
				.cloned()
				.collect::<Vec<_>>()
		};
		assert_eq!(changes().len(), 1);
		observe_profile(&server, 10, true);
		assert!(!router.prepare_transport(&[B.to_string()].into()).unwrap());
		assert_eq!(changes().len(), 2);
		assert_eq!(changes()[1][2], "10");
		assert_eq!(changes()[1][4], "{ index: 41, save: false }");
	}

	#[test]
	fn mixed_transport_rejects_capacity_conflicts_and_unsupported_profiles() {
		let server = PipeWire::new();
		*server.objects.borrow_mut() = transport_fixture();
		let mut router = server.router();
		let devices = router.transport_devices().unwrap();
		assert_eq!(devices[A].controller, devices[B].controller);
		router.set_transport(Transport::ScoA2dp);
		assert!(
			router
				.prepare_transport(&allowed())
				.unwrap_err()
				.contains("capacity conflict")
		);
		router.set_sco_limit(2);
		assert!(router.prepare_transport(&allowed()).unwrap());
		router.set_sco_limit(1);
		server.objects.borrow_mut()[6]["info"]["params"]["EnumProfile"][1]["available"] =
			json!("no");
		assert!(
			router
				.prepare_transport(&[A.to_string()].into())
				.unwrap_err()
				.contains("no available advertised A2DP SBC")
		);
		server.objects.borrow_mut()[6]["info"]["params"]["EnumProfile"] = json!([]);
		assert!(!router.transport_devices().unwrap()[B].hfp_available);
		assert!(!router.transport_devices().unwrap()[B].a2dp_available);
		*server.objects.borrow_mut() = transport_fixture();
		server.objects.borrow_mut()[6]["info"]["params"]["EnumProfile"][0]["available"] =
			json!("no");
		assert!(
			router
				.prepare_transport(&[A.to_string()].into())
				.unwrap_err()
				.contains("no available advertised headset-head-unit")
		);
		*server.objects.borrow_mut() = transport_fixture();
		server.objects.borrow_mut()[0]["info"]["params"]["EnumProfile"][1]["available"] =
			json!("no");
		assert!(
			router
				.prepare_transport(&[A.to_string()].into())
				.unwrap_err()
				.contains("no available advertised A2DP SBC")
		);
	}

	#[test]
	fn profile_mutations_revalidate_serial_and_bluez_address() {
		let mut objects = transport_fixture();
		objects[6]["info"]["props"]["api.bluez5.path"] =
			json!("/org/bluez/hci0/dev_AA_BB_CC_DD_EE_99");
		let mut router = Router::with_executor(allowed(), |args: &[&str]| {
			assert_eq!(args, ["pw-dump"]);
			Ok(json!(objects).to_string())
		});
		router.set_transport(Transport::ScoA2dp);
		assert!(!router.transport_devices().unwrap().contains_key(B));
		assert!(router.prepare_transport(&[A.to_string()].into()).is_err());

		let mut calls = 0;
		let mut router = Router::with_executor(allowed(), |args: &[&str]| {
			assert_eq!(args, ["pw-dump"]);
			let mut objects = transport_fixture();
			calls += 1;
			if calls > 1 {
				objects[6]["info"]["props"]["object.serial"] = json!(9999);
			}
			Ok(json!(objects).to_string())
		});
		router.set_transport(Transport::ScoA2dp);
		assert!(
			router
				.prepare_transport(&[A.to_string()].into())
				.unwrap_err()
				.contains("identity changed")
		);
	}

	#[test]
	fn disconnected_allowed_devices_are_not_transport_errors() {
		let server = PipeWire::new();
		let mut objects = transport_fixture();
		objects.retain(|object| {
			object["id"]
				.as_u64()
				.is_none_or(|id| !(20..26).contains(&id))
		});
		*server.objects.borrow_mut() = objects;
		let mut router = server.router();
		router.set_transport(Transport::ScoA2dp);
		assert!(router.prepare_transport(&[A.to_string()].into()).unwrap());
		assert!(!router.prepare_transport(&[B.to_string()].into()).unwrap());
		assert!(
			router
				.prepare_transport(&[C.to_string()].into())
				.unwrap_err()
				.contains("not allowlisted")
		);

		let mut calls = 0;
		let mut router = Router::with_executor(allowed(), |args: &[&str]| {
			assert_eq!(args, ["pw-dump"]);
			calls += 1;
			let mut objects = transport_fixture();
			if calls > 1 {
				objects.retain(|object| {
					object["id"]
						.as_u64()
						.is_none_or(|id| !(20..26).contains(&id))
				});
			}
			Ok(json!(objects).to_string())
		});
		router.set_transport(Transport::ScoA2dp);
		assert!(!router.prepare_transport(&[A.to_string()].into()).unwrap());
	}

	#[test]
	fn promotion_retries_if_fresh_snapshot_shows_an_unfinished_demotion() {
		let server = PipeWire::new();
		*server.objects.borrow_mut() = transport_fixture();
		observe_profile(&server, 10, false);
		observe_profile(&server, 20, false);
		let mut calls = 0;
		let mut router = Router::with_executor(allowed(), |args: &[&str]| {
			assert_eq!(args, ["pw-dump"]);
			calls += 1;
			if calls > 1 {
				observe_profile(&server, 20, true);
			}
			Ok(json!(*server.objects.borrow()).to_string())
		});
		router.set_transport(Transport::ScoA2dp);
		assert!(!router.prepare_transport(&[A.to_string()].into()).unwrap());
	}

	#[test]
	fn readiness_deadline_covers_missing_ports_and_external_profile_changes() {
		let server = PipeWire::new();
		*server.objects.borrow_mut() = transport_fixture();
		observe_profile(&server, 20, false);
		server
			.objects
			.borrow_mut()
			.retain(|object| object["id"] != 13);
		let mut router = server.router();
		router.set_transport(Transport::ScoA2dp);
		let sources = [A.to_string()].into();
		assert!(!router.prepare_transport(&sources).unwrap());
		assert!(router.coordinator.profile_changes.is_empty());
		let started = router.coordinator.transition_started;
		assert!(!router.prepare_transport(&sources).unwrap());
		assert_eq!(router.coordinator.transition_started, started);
		server
			.objects
			.borrow_mut()
			.push(transport_fixture()[3].clone());
		assert!(router.prepare_transport(&sources).unwrap());
		assert!(router.coordinator.transition_started.is_none());
		router.update_sources(&sources).unwrap();
		assert_eq!(server.links(), 2);

		// A later external profile change starts a fresh bounded transition,
		// even when the requested floor has not changed.
		server
			.objects
			.borrow_mut()
			.iter_mut()
			.find(|object| object["id"] == 20)
			.unwrap()["info"]["params"]["Profile"] = json!([{"index":17}]);
		assert!(!router.prepare_transport(&sources).unwrap());
		assert_eq!(server.links(), 0);
		let started = router.coordinator.transition_started;
		assert!(!router.prepare_transport(&sources).unwrap());
		assert_eq!(router.coordinator.transition_started, started);
		observe_profile(&server, 20, false);
		assert!(router.prepare_transport(&sources).unwrap());
		assert!(router.coordinator.transition_started.is_none());

		server
			.objects
			.borrow_mut()
			.retain(|object| object["id"] != 13);
		assert!(!router.prepare_transport(&sources).unwrap());
		router.coordinator.transition_started = Some(Instant::now() - Duration::from_secs(31));
		assert!(
			router
				.prepare_transport(&sources)
				.unwrap_err()
				.contains("within 30 seconds")
		);
		assert_eq!(server.links(), 0);
	}

	#[test]
	fn readiness_deadline_covers_stale_hfp_nodes_after_profile_acknowledgement() {
		let server = PipeWire::new();
		*server.objects.borrow_mut() = transport_fixture();
		let mut router = server.router();
		router.set_transport(Transport::ScoA2dp);
		let sources = [A.to_string()].into();
		assert!(!router.prepare_transport(&sources).unwrap());
		server.objects.borrow_mut()[6]["info"]["params"]["Profile"] = json!([{"index":41}]);
		assert!(!router.prepare_transport(&sources).unwrap());
		assert!(router.coordinator.profile_changes.is_empty());
		router.coordinator.transition_started = Some(Instant::now() - Duration::from_secs(31));
		assert!(
			router
				.prepare_transport(&sources)
				.unwrap_err()
				.contains("within 30 seconds")
		);
	}

	#[test]
	fn duplicate_live_devices_reject_ambiguous_controllers_only_in_mixed_mode() {
		let server = PipeWire::new();
		let mut objects = transport_fixture();
		let mut duplicate = objects[0].clone();
		duplicate["id"] = json!(30);
		duplicate["info"]["props"]["object.serial"] = json!(1030);
		duplicate["info"]["props"]["api.bluez5.path"] =
			json!("/org/bluez/hci1/dev_AA_BB_CC_DD_EE_01");
		objects.push(duplicate);
		*server.objects.borrow_mut() = objects;
		let mut router = server.router();
		let error = router.transport_devices().unwrap_err();
		assert!(error.contains("multiple live PipeWire Device"));
		assert!(error.contains("hci0") && error.contains("hci1"));
		// Advisory metadata errors must not change legacy HFP discovery.
		router.update(true).unwrap();
		assert_eq!(server.links(), 4);
		router.set_transport(Transport::ScoA2dp);
		assert!(
			router
				.prepare_transport(&[A.to_string()].into())
				.unwrap_err()
				.contains("multiple live PipeWire Device")
		);
		assert_eq!(server.links(), 0);
	}

	#[test]
	fn a2dp_is_opt_in_and_never_supplies_a_microphone() {
		let server = PipeWire::new();
		*server.objects.borrow_mut() = transport_fixture();
		observe_profile(&server, 20, false);
		let mut router = server.router();
		assert!(router.inspect().unwrap().0[B].sinks.is_empty());
		router.set_transport(Transport::ScoA2dp);
		let devices = router.transport_devices().unwrap();
		assert_eq!(devices[B].controller, "/org/bluez/hci0");
		assert!(devices[B].hfp_available && devices[B].a2dp_available);
		let headsets = router.inspect().unwrap().0;
		assert!(headsets[B].sources.is_empty());
		assert_eq!(desired_links(&headsets), [(13, 24), (13, 25)].into());
	}

	#[test]
	fn transport_switching_preserves_external_links_and_last_profiles() {
		let server = PipeWire::new();
		*server.objects.borrow_mut() = transport_fixture();
		server.external_link();
		let mut router = server.router();
		router.update(true).unwrap();
		router.set_transport(Transport::ScoA2dp);
		assert_eq!(server.links(), 1);
		assert!(!router.prepare_transport(&[A.to_string()].into()).unwrap());
		observe_profile(&server, 20, false);
		assert!(router.prepare_transport(&[A.to_string()].into()).unwrap());
		router.close();
		assert_eq!(server.links(), 1);
		assert_eq!(
			server.objects.borrow()[6]["info"]["params"]["Profile"][0]["index"],
			41
		);
	}

	#[test]
	fn routes_only_other_allowlisted_headsets() {
		let (headsets, links) = topology(&json!(fixture()), &allowed()).unwrap();
		assert!(links.is_empty());
		assert_eq!(headsets[A].speaker_node(), Some(12));
		assert_eq!(headsets[B].speaker_node(), Some(22));
		assert_eq!(
			desired_links(&headsets),
			[(13, 24), (13, 25), (23, 14), (23, 15)].into()
		);
		let (headsets, _) = topology(&json!(fixture()), &[A.to_string()].into()).unwrap();
		assert!(desired_links(&headsets).is_empty());
	}

	#[test]
	fn rejects_link_commands_without_a_program() {
		assert!(start_link(&[]).is_err());
		assert!(start_link(&[""]).is_err());
	}

	#[test]
	fn routes_only_headsets_sharing_a_talk_group() {
		let objects = [fixture(), headset(30, C)].concat();
		let allowed = [A.to_string(), B.to_string(), C.to_string()].into();
		let (headsets, _) = topology(&json!(objects), &allowed).unwrap();
		let groups = [TalkGroup {
			name: "Team".into(),
			members: [A.to_string(), C.to_string()].into(),
		}];
		assert_eq!(
			desired_links_in_groups(&headsets, &groups),
			[(13, 34), (13, 35), (33, 14), (33, 15)].into()
		);
		assert_eq!(
			desired_links_in_groups(&headsets, &[]),
			desired_links(&headsets)
		);
	}

	#[test]
	fn half_duplex_routes_each_source_only_in_groups_where_it_holds_the_floor() {
		let objects = [fixture(), headset(30, C)].concat();
		let allowed = [A.to_string(), B.to_string(), C.to_string()].into();
		let (headsets, _) = topology(&json!(objects), &allowed).unwrap();
		let groups = [
			TalkGroup {
				name: "North".into(),
				members: [A.to_string(), B.to_string()].into(),
			},
			TalkGroup {
				name: "South".into(),
				members: [A.to_string(), B.to_string(), C.to_string()].into(),
			},
		];
		let sources = BTreeMap::from([
			(Some("North".into()), [A.to_string()].into()),
			(Some("South".into()), [C.to_string()].into()),
		]);
		assert_eq!(
			desired_links_by_group(&headsets, &sources, &groups),
			[(13, 24), (13, 25), (33, 14), (33, 15), (33, 24), (33, 25),].into()
		);
	}

	#[test]
	fn detects_live_routes_from_each_headset_microphone() {
		let (headsets, _) = topology(&json!(fixture()), &allowed()).unwrap();
		let links = desired_links(&headsets);
		assert!(has_active_source_route(A, &headsets, &links));
		assert!(has_active_source_route(B, &headsets, &links));
		let links = links
			.into_iter()
			.filter(|(output, _)| *output != 13)
			.collect();
		assert!(!has_active_source_route(A, &headsets, &links));
		assert!(has_active_source_route(B, &headsets, &links));
	}

	#[test]
	fn detects_bidirectional_intercom_connections() {
		let (headsets, _) = topology(&json!(fixture()), &allowed()).unwrap();
		let links = desired_links(&headsets);
		assert!(has_active_intercom_connection(A, &headsets, &links));
		assert!(has_active_intercom_connection(B, &headsets, &links));
		let one_way = [(13, 24), (13, 25)].into();
		assert!(!has_active_intercom_connection(A, &headsets, &one_way));
		assert!(!has_active_intercom_connection(B, &headsets, &one_way));
		let (single, _) = topology(&json!(headset(10, A)), &[A.to_string()].into()).unwrap();
		assert!(!has_active_intercom_connection(A, &single, &links));
	}

	#[test]
	fn headset_button_controls_only_its_own_microphone() {
		let server = PipeWire::new();
		let mut router = server.router();
		let mut sources = BTreeSet::new();
		router.update_sources(&sources).unwrap();
		assert_eq!(server.links(), 0);
		sources.insert(A.to_string());
		router.update_sources(&sources).unwrap();
		let (_, links) = router.inspect_owned().unwrap();
		assert_eq!(links, [(13, 24), (13, 25)].into());
		sources.clear();
		router.update_sources(&sources).unwrap();
		assert_eq!(server.links(), 0);
	}

	#[test]
	fn filters_profiles_and_matches_channels() {
		let mut objects = fixture();
		objects[1]["info"]["props"]["api.bluez5.profile"] = json!("bap-duplex");
		objects[2]["info"]["props"]["api.bluez5.profile"] = json!("a2dp-sink");
		let (headsets, _) = topology(&json!(objects), &allowed()).unwrap();
		assert!(desired_links(&headsets).is_empty());
		let mut objects = fixture();
		objects[3]["info"]["props"]["audio.channel"] = json!("FL");
		let (headsets, _) = topology(&json!(objects), &allowed()).unwrap();
		assert!(!desired_links(&headsets).contains(&(13, 25)));
		objects[1]["info"]["props"]
			.as_object_mut()
			.unwrap()
			.remove("api.bluez5.profile");
		let (headsets, _) = topology(&json!(objects), &allowed()).unwrap();
		assert_eq!(desired_links(&headsets), [(23, 14), (23, 15)].into());
	}

	#[test]
	fn ignores_unrelated_ports_and_detects_existing_links() {
		let mut objects = fixture();
		objects.push(json!({"type":"PipeWire:Interface:Node","id":31,"info":{"props":{"device.id":"10","media.class":"Stream/Output/Audio"}}}));
		objects.push(json!({"type":"PipeWire:Interface:Port","id":32,"info":{"props":{"node.id":"31","port.direction":"out"}}}));
		objects.push(json!({"type":"PipeWire:Interface:Link","id":100,"info":{"output-port-id":13,"input-port-id":24}}));
		let (headsets, links) = topology(&json!(objects), &allowed()).unwrap();
		assert_eq!(headsets[A].sources.len(), 1);
		assert_eq!(links, [(13, 24)].into());
	}

	#[test]
	fn inspection_does_not_create_or_remove_links() {
		let mut router = Router::with_executor(allowed(), |args: &[&str]| {
			assert_eq!(args, ["pw-dump"]);
			Ok(json!(fixture()).to_string())
		});
		let (headsets, links) = router.inspect().unwrap();
		assert!(links.is_empty());
		assert!(headsets[A].has_duplex_audio());
		assert!(headsets[B].has_duplex_audio());
		assert!(!Headset::default().has_duplex_audio());
	}

	#[test]
	fn dashboard_inspection_excludes_external_links() {
		let server = PipeWire::new();
		server.external_link();
		let mut router = server.router();
		router.update(true).unwrap();
		let (headsets, owned) = router.inspect_owned().unwrap();
		assert!(headsets[A].has_duplex_audio());
		assert!(!owned.contains(&(13, 24)));
		assert_eq!(owned.len(), 3);
		router.close();
	}

	#[derive(Clone)]
	struct PipeWire {
		objects: Rc<RefCell<Vec<Value>>>,
		calls: Rc<RefCell<Vec<Vec<String>>>>,
		fail_next: Rc<RefCell<bool>>,
		fail_snapshot: Rc<RefCell<bool>>,
		replace_after_snapshot: Rc<RefCell<bool>>,
	}

	impl PipeWire {
		fn new() -> Self {
			Self {
				objects: Rc::new(RefCell::new(fixture())),
				calls: Rc::default(),
				fail_next: Rc::default(),
				fail_snapshot: Rc::default(),
				replace_after_snapshot: Rc::default(),
			}
		}

		fn execute(&self, args: &[&str]) -> Result<String, String> {
			self.calls
				.borrow_mut()
				.push(args.iter().map(|s| s.to_string()).collect());
			if args[0] == "pw-dump" {
				if *self.fail_snapshot.borrow() {
					return Err("PipeWire unavailable".into());
				}
				let snapshot = json!(*self.objects.borrow()).to_string();
				if self.replace_after_snapshot.replace(false) {
					for object in self.objects.borrow_mut().iter_mut() {
						if object["type"] == "PipeWire:Interface:Link" {
							object["info"]["props"] = json!({});
						}
					}
				}
				return Ok(snapshot);
			}
			if self.fail_next.replace(false) {
				return Err("simulated pw-cli failure".into());
			}
			if args.get(1) == Some(&"set-param") {
				assert_eq!(args[0], "pw-cli");
				assert_eq!(args[3], "Profile");
				return Ok(String::new());
			}
			let mut objects = self.objects.borrow_mut();
			assert_eq!(&args[..4], ["pw-cli", "-m", "create-link", "-"]);
			assert_eq!(args[5], "-");
			let props: Value = serde_json::from_str(args[7]).unwrap();
			assert!(props[OWNER_PROPERTY].as_str().is_some());
			assert_eq!(props["object.linger"], false);
			let output = args[4].parse::<u64>().unwrap();
			let input = args[6].parse::<u64>().unwrap();
			let id = objects
				.iter()
				.filter_map(|object| object["id"].as_u64())
				.max()
				.unwrap_or(0)
				+ 1;
			objects.push(json!({"type":"PipeWire:Interface:Link","id":id,"info":{"output-port-id":output,"input-port-id":input,"props":props}}));
			Ok(id.to_string())
		}

		fn router(&self) -> Router<impl FnMut(&[&str]) -> Result<String, String> + use<>> {
			let snapshot = self.clone();
			let links = self.clone();
			Router::with_backend(
				allowed(),
				move |args| snapshot.execute(args),
				Box::new(move |args| {
					let id = links.execute(args)?.parse::<u64>().unwrap();
					let object = links
						.objects
						.borrow()
						.iter()
						.find(|object| object["id"] == id)
						.unwrap()
						.clone();
					Ok(Box::new(TestLink {
						server: links.clone(),
						object,
					}))
				}),
			)
		}

		fn links(&self) -> usize {
			self.objects
				.borrow()
				.iter()
				.filter(|object| object["type"] == "PipeWire:Interface:Link")
				.count()
		}

		fn external_link(&self) {
			self.objects.borrow_mut().push(json!({"type":"PipeWire:Interface:Link","id":100,"info":{"output-port-id":13,"input-port-id":24}}));
		}
	}

	struct TestLink {
		server: PipeWire,
		object: Value,
	}

	impl LinkHandle for TestLink {
		fn is_running(&mut self) -> Result<bool, String> {
			Ok(self.server.objects.borrow().contains(&self.object))
		}
	}

	impl Drop for TestLink {
		fn drop(&mut self) {
			self.server
				.objects
				.borrow_mut()
				.retain(|object| object != &self.object);
		}
	}

	#[test]
	fn owns_only_created_links_and_cleans_up_on_disappearance() {
		let server = PipeWire::new();
		server.external_link();
		let mut router = server.router();
		router.update(true).unwrap();
		assert_eq!(server.links(), 4);
		*server.objects.borrow_mut() = headset(10, A);
		router.update(true).unwrap();
		router.close();
		assert!(
			!server
				.calls
				.borrow()
				.iter()
				.any(|args| args.get(1).is_some_and(|arg| arg == "-d"))
		);
	}

	#[test]
	fn failed_link_retries_and_close_only_unlinks_owned() {
		let server = PipeWire::new();
		let mut router = server.router();
		server.fail_next.replace(true);
		assert!(router.update(true).is_err());
		assert_eq!(server.links(), 3);
		router.update(true).unwrap();
		assert_eq!(server.links(), 4);
		server.external_link();
		router.close();
		assert_eq!(server.links(), 1);
	}

	#[test]
	fn ptt_mutes_and_restores_only_owned_links() {
		let server = PipeWire::new();
		server.external_link();
		let mut router = server.router();
		router.update(false).unwrap();
		assert_eq!(server.links(), 1);
		router.update(true).unwrap();
		assert_eq!(server.links(), 4);
		router.update(false).unwrap();
		assert_eq!(server.links(), 1);
		router.update(true).unwrap();
		assert_eq!(server.links(), 4);
	}

	#[test]
	fn reused_ids_and_replacement_links_are_not_owned() {
		for close in [false, true] {
			let server = PipeWire::new();
			let mut router = server.router();
			router.update(true).unwrap();
			for object in server.objects.borrow_mut().iter_mut() {
				if object["type"] == "PipeWire:Interface:Link" {
					object["info"]["props"] = json!({});
				}
			}
			if close {
				router.close();
			} else {
				router.update(false).unwrap();
			}
			assert_eq!(server.links(), 4);
		}
	}

	#[test]
	fn replacement_after_snapshot_is_not_deleted_when_muting() {
		let server = PipeWire::new();
		let mut router = server.router();
		router.update(true).unwrap();
		server.replace_after_snapshot.replace(true);
		router.update(false).unwrap();
		assert_eq!(server.links(), 4);
		assert!(router.owned.is_empty());
	}

	#[test]
	fn dropping_router_releases_links_without_delete_commands() {
		let server = PipeWire::new();
		let mut router = server.router();
		router.update(true).unwrap();
		drop(router);
		assert_eq!(server.links(), 0);
		assert!(
			!server
				.calls
				.borrow()
				.iter()
				.any(|args| args.get(1) == Some(&"-d".into()))
		);
	}

	#[test]
	fn separate_routers_do_not_own_each_others_links() {
		let server = PipeWire::new();
		let mut first = server.router();
		let mut second = server.router();
		first.update(true).unwrap();
		second.update(true).unwrap();
		second.close();
		assert_eq!(server.links(), 4);
		first.close();
		assert_eq!(server.links(), 0);
	}

	#[test]
	fn profile_changes_remove_owned_links_and_reconnect_restores_them() {
		let server = PipeWire::new();
		let mut router = server.router();
		router.update(true).unwrap();
		for object in server.objects.borrow_mut().iter_mut() {
			if object["type"] == "PipeWire:Interface:Node" {
				object["info"]["props"]["api.bluez5.profile"] = json!("a2dp-sink");
			}
		}
		router.update(true).unwrap();
		assert_eq!(server.links(), 0);
		*server.objects.borrow_mut() = fixture();
		router.update(true).unwrap();
		assert_eq!(server.links(), 4);
	}

	#[test]
	fn cleanup_does_not_need_a_working_snapshot() {
		let server = PipeWire::new();
		let mut router = server.router();
		router.update(true).unwrap();
		server.fail_snapshot.replace(true);
		assert!(router.update(false).is_err());
		assert_eq!(server.links(), 4);
		router.close();
		assert_eq!(server.links(), 0);
		server.fail_snapshot.replace(false);
		router.update(true).unwrap();
		assert_eq!(server.links(), 4);
	}

	#[test]
	fn exited_link_processes_are_retried() {
		let server = PipeWire::new();
		let mut router = server.router();
		router.update(true).unwrap();
		*server.objects.borrow_mut() = fixture();
		assert!(router.update(true).is_err());
		assert_eq!(server.links(), 4);
		router.close();
		assert_eq!(server.links(), 0);
	}
}
