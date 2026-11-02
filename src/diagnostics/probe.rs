// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Active hardware probe: connects the allowlisted headsets, exercises their
//! A2DP and HFP profiles and measures how many can use SCO at once per
//! controller. Only allowlisted devices are touched, profile changes are not
//! saved (`save: false`), and every headset's profiles are released (PipeWire `off`) afterwards so
//! none holds the controller's single A2DP stream.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use super::Finding;
use crate::bluez::{self, Address};
use crate::pipewire::{Headset, Snapshot};
use crate::transport::{Device, Transport};
use crate::transport_model::ProfileFamily;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Timing {
	pub(crate) poll: Duration,
	pub(crate) connect: Duration,
	pub(crate) profile: Duration,
}

impl Default for Timing {
	fn default() -> Self {
		Self {
			poll: Duration::from_millis(500),
			connect: Duration::from_secs(20),
			profile: Duration::from_secs(15),
		}
	}
}

type Observed = (BTreeMap<Address, Device>, BTreeMap<Address, Headset>);

struct Prober<'a, F> {
	allowed: &'a BTreeSet<Address>,
	stopped: &'a AtomicBool,
	timing: Timing,
	execute: F,
	findings: Vec<Finding>,
	labels: BTreeMap<Address, String>,
}

impl<F: FnMut(&[&str]) -> Result<String, String>> Prober<'_, F> {
	fn cancelled(&self) -> bool {
		self.stopped.load(Ordering::SeqCst)
	}

	fn label(&self, address: &Address) -> String {
		self.labels
			.get(address)
			.cloned()
			.unwrap_or_else(|| address.to_string())
	}

	fn observe(&mut self) -> Result<Observed, String> {
		let snapshot = Snapshot::fetch(&mut self.execute)?;
		let devices = snapshot.transport_devices(self.allowed)?;
		let (headsets, _) = snapshot.topology(self.allowed, Transport::ScoA2dp);
		Ok((devices, headsets))
	}

	fn is_ready(observed: &Observed, address: &Address, family: ProfileFamily) -> bool {
		let (devices, headsets) = observed;
		devices
			.get(address)
			.is_some_and(|device| device.is_profile(family))
			&& headsets.get(address).is_some_and(|headset| match family {
				ProfileFamily::Hfp => headset.has_duplex_audio(),
				ProfileFamily::A2dp => !headset.sinks.is_empty(),
			})
	}

	fn wait_ready(&mut self, targets: &[(Address, ProfileFamily)]) -> Result<Duration, String> {
		let started = Instant::now();
		let mut last_error = String::new();
		loop {
			if self.cancelled() {
				return Err("cancelled".into());
			}
			match self.observe() {
				Ok(observed) => {
					if targets
						.iter()
						.all(|(address, family)| Self::is_ready(&observed, address, *family))
					{
						return Ok(started.elapsed());
					}
				}
				Err(error) => last_error = error,
			}
			if started.elapsed() >= self.timing.profile {
				return Err(format!(
					"profile and audio ports not ready within {}s{}",
					self.timing.profile.as_secs(),
					if last_error.is_empty() {
						String::new()
					} else {
						format!(" ({last_error})")
					}
				));
			}
			thread::sleep(self.timing.poll);
		}
	}

	/// Selects a profile using a fresh observation; IDs are never cached.
	fn select(&mut self, address: &Address, family: ProfileFamily) -> Result<(), String> {
		let (devices, _) = self.observe()?;
		let device = devices
			.get(address)
			.ok_or("not present in PipeWire (disconnected?)")?;
		let index = device
			.index(family)
			.ok_or_else(|| format!("{} profile is not available", family.name()))?;
		if device.is_profile(family) {
			return Ok(());
		}
		(self.execute)(&[
			"pw-cli",
			"set-param",
			&device.id.to_string(),
			"Profile",
			&format!("{{ index: {index}, save: false }}"),
		])
		.map(drop)
	}

	/// Releases every profile so a headset holds no A2DP stream or SCO link.
	/// Falls back to A2DP when the device has no `off` profile.
	fn release(&mut self, address: &Address) {
		let result = self.observe().and_then(|(devices, _)| {
			let device = devices.get(address).ok_or("not present in PipeWire")?;
			let Some(index) = device.off_index() else {
				return self.select(address, ProfileFamily::A2dp);
			};
			if device.is_off() {
				return Ok(());
			}
			(self.execute)(&[
				"pw-cli",
				"set-param",
				&device.id.to_string(),
				"Profile",
				&format!("{{ index: {index}, save: false }}"),
			])
			.map(drop)
		});
		if let Err(error) = result {
			self.warn(format!(
				"{}: could not release profiles: {error}",
				self.label(address)
			));
		}
	}

	fn try_profile(
		&mut self,
		address: &Address,
		family: ProfileFamily,
	) -> Result<Duration, String> {
		self.select(address, family)?;
		self.wait_ready(&[(address.clone(), family)])
	}

	fn warn(&mut self, text: String) {
		self.findings.push(Finding::warn(text));
	}

	fn info(&mut self, text: String) {
		self.findings.push(Finding::info(text));
	}

	fn power_and_connect(&mut self) -> BTreeSet<Address> {
		if let Ok(show) = (self.execute)(&["bluetoothctl", "show"])
			&& bluez::property(&show, "Powered") == Some("no")
			&& let Err(error) = bluez::checked((self.execute)(&["bluetoothctl", "power", "on"]))
		{
			self.warn(format!("Could not power on the Bluetooth adapter: {error}"));
		}
		let mut candidates = BTreeSet::new();
		for address in self.allowed.clone() {
			if self.cancelled() {
				break;
			}
			let info = (self.execute)(&["bluetoothctl", "info", address.as_str()]);
			if let Ok(info) = &info {
				if let Some(name) = bluez::bluetooth_name(info) {
					self.labels
						.insert(address.clone(), format!("{name} ({address})"));
				}
				if bluez::property(info, "Paired") == Some("no") {
					self.warn(format!(
						"{}: not paired; skipped (run `bt-intercom pair {address}`)",
						self.label(&address)
					));
					continue;
				}
			}
			let connected = info
				.as_ref()
				.is_ok_and(|info| bluez::device_flag(info, "Connected"));
			if !connected
				&& let Err(error) = bluez::checked((self.execute)(&[
					"bluetoothctl",
					"--timeout",
					"30",
					"connect",
					address.as_str(),
				])) {
				self.warn(format!("{}: connect failed: {error}", self.label(&address)));
			}
			candidates.insert(address);
		}
		candidates
	}

	fn await_presence(&mut self, candidates: &BTreeSet<Address>) -> BTreeMap<Address, Device> {
		let started = Instant::now();
		loop {
			let devices = self
				.observe()
				.map(|(devices, _)| devices)
				.unwrap_or_default();
			if candidates
				.iter()
				.all(|address| devices.contains_key(address))
				|| started.elapsed() >= self.timing.connect
				|| self.cancelled()
			{
				return devices;
			}
			thread::sleep(self.timing.poll);
		}
	}

	fn run(&mut self) {
		let candidates = self.power_and_connect();
		let devices = self.await_presence(&candidates);
		for address in &candidates {
			if !devices.contains_key(address) {
				self.warn(format!(
					"{}: did not appear in PipeWire within {}s; it is not connected or WirePlumber's Bluetooth monitor is not running",
					self.label(address),
					self.timing.connect.as_secs()
				));
			}
		}
		let mut hfp_capable: BTreeMap<String, Vec<Address>> = BTreeMap::new();
		let mut connected_per_controller: BTreeMap<String, usize> = BTreeMap::new();
		for (address, device) in &devices {
			if self.cancelled() {
				break;
			}
			let controller = device
				.controller
				.rsplit('/')
				.next()
				.unwrap_or(&device.controller)
				.to_string();
			*connected_per_controller
				.entry(controller.clone())
				.or_default() += 1;
			let label = self.label(address);
			let a2dp = self.try_profile(address, ProfileFamily::A2dp);
			let hfp = self.try_profile(address, ProfileFamily::Hfp);
			for (name, result) in [("A2DP", &a2dp), ("HFP", &hfp)] {
				match result {
					Ok(time) => self.info(format!(
						"{label} on {controller}: {name} playback ports ready in {:.1}s",
						time.as_secs_f32()
					)),
					Err(error) => {
						self.warn(format!("{label} on {controller}: {name} failed: {error}"))
					}
				}
			}
			if hfp.is_ok() {
				hfp_capable
					.entry(controller)
					.or_default()
					.push(address.clone());
			}
			// Free the controller's A2DP stream before testing the next headset.
			self.release(address);
		}
		let mut total = 0;
		for (controller, headsets) in &hfp_capable {
			total += self.measure_sco(controller, headsets);
		}
		for address in devices.keys() {
			self.release(address);
		}
		let connected: usize = connected_per_controller.values().sum();
		let per_controller = connected_per_controller
			.iter()
			.map(|(controller, count)| format!("{controller}: {count}"))
			.collect::<Vec<_>>()
			.join(", ");
		let summary = format!(
			"Measured: {connected} of {} headsets connected at once ({per_controller}); {total} simultaneous HFP/SCO talker(s) in total across controllers",
			self.allowed.len()
		);
		if connected < self.allowed.len() {
			self.warn(summary);
		} else {
			self.info(summary);
		}
		if self.cancelled() {
			self.warn("Probe cancelled; results are incomplete".into());
		}
	}

	/// Switches this controller's headsets to HFP one by one until one fails to
	/// come up alongside the others. Returns the number that worked together.
	fn measure_sco(&mut self, controller: &str, headsets: &[Address]) -> usize {
		let mut active: Vec<Address> = Vec::new();
		let mut limited_by = None;
		for address in headsets {
			if self.cancelled() {
				break;
			}
			let result = self.select(address, ProfileFamily::Hfp).and_then(|()| {
				let mut targets: Vec<_> = active
					.iter()
					.map(|address| (address.clone(), ProfileFamily::Hfp))
					.collect();
				targets.push((address.clone(), ProfileFamily::Hfp));
				self.wait_ready(&targets)
			});
			match result {
				Ok(_) => active.push(address.clone()),
				Err(error) => {
					limited_by = Some((address.clone(), error));
					self.release(address);
					break;
				}
			}
		}
		for address in &active {
			self.release(address);
		}
		let count = active.len();
		match limited_by {
			Some((address, error)) => self.warn(format!(
				"{controller}: {count} headset(s) worked with simultaneous HFP/SCO; adding {} failed: {error}. Use --sco-limit {}",
				self.label(&address),
				count.max(1)
			)),
			None if count > 0 => self.info(format!(
				"{controller}: all {count} tested headset(s) used HFP/SCO simultaneously; the real limit may be higher (test with more headsets). Verified --sco-limit: {count}"
			)),
			None => {}
		}
		count
	}
}

pub(crate) fn probe(
	allowed: &BTreeSet<Address>,
	stopped: &AtomicBool,
	timing: Timing,
	execute: impl FnMut(&[&str]) -> Result<String, String>,
) -> Vec<Finding> {
	let mut prober = Prober {
		allowed,
		stopped,
		timing,
		execute,
		findings: Vec::new(),
		labels: BTreeMap::new(),
	};
	prober.run();
	prober.findings
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::diagnostics::Level;
	use serde_json::{Value, json};
	use std::cell::RefCell;
	use std::rc::Rc;

	/// Simulated PipeWire: at most `limit` headsets per controller can hold HFP.
	struct Sim {
		profiles: Vec<u64>,
		limit: usize,
	}

	fn dump(sim: &Sim) -> Value {
		let mut objects = Vec::new();
		let hfp_count = sim.profiles.iter().filter(|p| **p == 2).count();
		for (i, current) in sim.profiles.iter().enumerate() {
			let n = i as u64;
			let address = format!("AA:BB:CC:DD:EE:0{}", n + 1);
			objects.push(
				json!({"id": 10 + n, "type": "PipeWire:Interface:Device", "info": {
				"props": {"api.bluez5.address": address,
					"api.bluez5.path": format!("/org/bluez/hci0/dev_{}", address.replace(':', "_")),
					"object.serial": 100 + n},
				"params": {"EnumProfile": [
					{"index": 0, "name": "off"}, {"index": 1, "name": "a2dp-sink-sbc"}, {"index": 2, "name": "headset-head-unit"}],
					"Profile": [{"index": current}]}}}),
			);
			let sink = 100 + n * 10;
			let (profile, source) = if *current == 2 {
				("headset-head-unit", hfp_count <= sim.limit)
			} else {
				("a2dp-sink-sbc", false)
			};
			if *current == 0 || (*current == 2 && !source) {
				continue;
			}
			objects.push(
				json!({"id": sink, "type": "PipeWire:Interface:Node", "info": {"props": {
				"device.id": 10 + n, "media.class": "Audio/Sink", "api.bluez5.profile": profile}}}),
			);
			objects.push(
				json!({"id": sink + 1, "type": "PipeWire:Interface:Port", "info": {"props": {
				"node.id": sink, "port.direction": "in"}}}),
			);
			if source {
				objects.push(
					json!({"id": sink + 2, "type": "PipeWire:Interface:Node", "info": {"props": {
					"device.id": 10 + n, "media.class": "Audio/Source", "api.bluez5.profile": profile}}}),
				);
				objects.push(
					json!({"id": sink + 3, "type": "PipeWire:Interface:Port", "info": {"props": {
					"node.id": sink + 2, "port.direction": "out"}}}),
				);
			}
		}
		Value::Array(objects)
	}

	fn run(headsets: usize, limit: usize) -> (Vec<Finding>, Vec<u64>) {
		let sim = Rc::new(RefCell::new(Sim {
			profiles: vec![1; headsets],
			limit,
		}));
		let allowed = (1..=headsets)
			.map(|i| Address::parse(&format!("AA:BB:CC:DD:EE:0{i}")).unwrap())
			.collect();
		let shared = Rc::clone(&sim);
		let findings = probe(
			&allowed,
			&AtomicBool::new(false),
			Timing {
				poll: Duration::from_millis(1),
				connect: Duration::from_millis(50),
				profile: Duration::from_millis(50),
			},
			move |args| match args {
				["bluetoothctl", "show"] => Ok("Powered: yes\n".into()),
				["bluetoothctl", "info", _] => Ok("Paired: yes\nConnected: yes\n".into()),
				["pw-dump"] => Ok(dump(&shared.borrow()).to_string()),
				["pw-cli", "set-param", id, "Profile", value] => {
					let index = if value.contains("index: 2") {
						2
					} else if value.contains("index: 0") {
						0
					} else {
						1
					};
					shared.borrow_mut().profiles[id.parse::<usize>().unwrap() - 10] = index;
					Ok(String::new())
				}
				other => Err(format!("unexpected {other:?}")),
			},
		);
		let profiles = sim.borrow().profiles.clone();
		(findings, profiles)
	}

	#[test]
	fn measures_the_simultaneous_sco_limit_and_releases_profiles() {
		let (findings, profiles) = run(3, 2);
		assert!(
			findings.iter().any(|f| f.level == Level::Warn
				&& f.text
					.contains("2 headset(s) worked with simultaneous HFP/SCO")
				&& f.text.contains("--sco-limit 2")),
			"{findings:#?}"
		);
		assert!(
			findings
				.iter()
				.any(|f| f.text.contains("3 of 3 headsets connected"))
		);
		assert_eq!(profiles, [0, 0, 0]);
	}

	#[test]
	fn reports_all_headsets_working_when_limit_is_not_reached() {
		let (findings, profiles) = run(2, 4);
		assert!(
			findings
				.iter()
				.any(|f| f.level == Level::Info && f.text.contains("all 2 tested headset(s)"))
		);
		assert_eq!(profiles, [0, 0]);
	}
}
