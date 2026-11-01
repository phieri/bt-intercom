// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Startup and on-demand diagnosis of the adapter/headset combination.
//!
//! Everything here is advisory: a failing probe becomes a finding, never an
//! error, so diagnosis cannot prevent the intercom from starting.

use std::collections::{BTreeMap, BTreeSet};

use std::time::Duration;

use crate::bluez::{self, Address};
use crate::router::Router;
use crate::transmit::Mode;
use crate::transport::{Device, Transport, headsets_per_controller};

mod probe;
pub(crate) use probe::{Timing, probe};

/// Deadline for a diagnostic subprocess; connecting may take up to 30 s.
pub(crate) fn command_timeout(args: &[&str]) -> Duration {
	if args.contains(&"--timeout") {
		Duration::from_secs(35)
	} else {
		crate::process::COMMAND_TIMEOUT
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Level {
	Info,
	Warn,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Finding {
	pub(crate) level: Level,
	pub(crate) text: String,
}

impl Finding {
	fn info(text: impl Into<String>) -> Self {
		Self {
			level: Level::Info,
			text: text.into(),
		}
	}

	fn warn(text: impl Into<String>) -> Self {
		Self {
			level: Level::Warn,
			text: text.into(),
		}
	}
}

#[derive(Debug, Default)]
struct Controller {
	address: String,
	powered: Option<bool>,
	audio_source: bool,
	hfp_gateway: bool,
}

#[derive(Debug)]
struct Headset {
	address: Address,
	label: String,
	info: Result<String, String>,
	pipewire: Option<Device>,
}

#[derive(Debug, Default)]
struct Inputs {
	controllers: Option<Vec<Controller>>,
	headsets: Vec<Headset>,
	pipewire_error: Option<String>,
}

fn collect(
	allowed: &BTreeSet<Address>,
	mut execute: impl FnMut(&[&str]) -> Result<String, String>,
) -> Inputs {
	let controllers = execute(&["bluetoothctl", "list"]).ok().map(|list| {
		bluez::controller_addresses(&list)
			.into_iter()
			.map(|address| {
				let show = execute(&["bluetoothctl", "show", address.as_str()]).unwrap_or_default();
				Controller {
					powered: bluez::property(&show, "Powered").map(|value| value == "yes"),
					audio_source: bluez::has_uuid(&show, "110a"),
					hfp_gateway: bluez::has_uuid(&show, "111f"),
					address: address.to_string(),
				}
			})
			.collect()
	});
	let (mut devices, pipewire_error) =
		match Router::with_executor(allowed.clone(), &mut execute).transport_devices() {
			Ok(devices) => (devices, None),
			Err(error) => (BTreeMap::new(), Some(error)),
		};
	let headsets = allowed
		.iter()
		.map(|address| {
			let info = execute(&["bluetoothctl", "info", address.as_str()]);
			let label = info
				.as_ref()
				.ok()
				.and_then(|info| bluez::bluetooth_name(info))
				.map_or_else(|| address.to_string(), |name| format!("{name} ({address})"));
			Headset {
				address: address.clone(),
				label,
				info,
				pipewire: devices.remove(address),
			}
		})
		.collect();
	Inputs {
		controllers,
		headsets,
		pipewire_error,
	}
}

fn yes_no(value: bool) -> &'static str {
	if value { "yes" } else { "no" }
}

fn evaluate(
	inputs: &Inputs,
	mode: Mode,
	transport: Transport,
	sco_limit: usize,
	a2dp_limit: usize,
) -> Vec<Finding> {
	let mut findings = Vec::new();
	match &inputs.controllers {
		None => findings.push(Finding::warn(
			"Could not list Bluetooth controllers; is bluetooth.service running and bluetoothctl installed?",
		)),
		Some(controllers) if controllers.is_empty() => findings.push(Finding::warn(
			"No Bluetooth controller found; check `rfkill list`, bluetooth.service and the adapter firmware",
		)),
		Some(controllers) => {
			for controller in controllers {
				let mut notes = Vec::new();
				match controller.powered {
					Some(true) => {}
					Some(false) => notes.push("powered off (`bluetoothctl power on`)"),
					None => notes.push("power state unknown"),
				}
				if !controller.audio_source {
					notes.push(
						"A2DP source not registered (is the PipeWire session manager's Bluetooth support running, e.g. WirePlumber or pipewire-media-session?)",
					);
				}
				if !controller.hfp_gateway {
					notes.push(
						"HFP audio gateway not registered (is the PipeWire session manager's Bluetooth support running, e.g. WirePlumber or pipewire-media-session?)",
					);
				}
				let text = format!(
					"Controller {}: {}",
					controller.address,
					if notes.is_empty() {
						"powered, A2DP source and HFP gateway registered".into()
					} else {
						notes.join("; ")
					}
				);
				findings.push(if notes.is_empty() {
					Finding::info(text)
				} else {
					Finding::warn(text)
				});
			}
		}
	}
	if let Some(error) = &inputs.pipewire_error {
		findings.push(Finding::warn(format!(
			"PipeWire devices unavailable ({error}); profiles and controller assignment are from BlueZ only"
		)));
	}

	let mut per_controller: BTreeMap<&str, Vec<&Address>> = BTreeMap::new();
	let mut usable = 0;
	for headset in &inputs.headsets {
		let Ok(info) = &headset.info else {
			findings.push(Finding::warn(format!(
				"{}: BlueZ does not know this device ({}); pair it with `bt-intercom pair`",
				headset.address,
				headset.info.as_ref().err().map_or("", String::as_str)
			)));
			continue;
		};
		let flag = |name| bluez::property(info, name).map(|value| value == "yes");
		let mut problems = Vec::new();
		if flag("Paired") != Some(true) {
			problems.push("not paired");
		}
		if flag("Blocked") == Some(true) {
			problems.push("blocked");
		}
		if flag("Trusted") == Some(false) {
			problems.push("not trusted");
		}
		// BlueZ keeps the UUIDs of paired devices while disconnected.
		let hfp_uuid = bluez::has_uuid(info, "111e") || bluez::has_uuid(info, "1108");
		let a2dp_uuid = bluez::has_uuid(info, "110b");
		let (hfp, a2dp) = match &headset.pipewire {
			Some(device) => (device.hfp_available, device.a2dp_available),
			None => (hfp_uuid, a2dp_uuid),
		};
		let source = if headset.pipewire.is_some() {
			"PipeWire"
		} else {
			"BlueZ UUIDs"
		};
		let connected = match flag("Connected") {
			Some(true) => "connected",
			Some(false) => "disconnected",
			None => "connection unknown",
		};
		let controller = headset.pipewire.as_ref().map(|device| {
			device
				.controller
				.rsplit('/')
				.next()
				.unwrap_or(&device.controller)
		});
		if let Some(controller) = controller {
			per_controller
				.entry(controller)
				.or_default()
				.push(&headset.address);
		}
		let mut line = format!(
			"{}: {connected}{}; HFP {}, A2DP {} (from {source})",
			headset.label,
			controller.map_or(String::new(), |name| format!(" on {name}")),
			yes_no(hfp),
			yes_no(a2dp),
		);
		let supported = match transport {
			Transport::ScoA2dp => hfp && a2dp,
			Transport::Hfp => hfp,
		};
		if !supported {
			problems.push(match (transport, hfp, a2dp) {
				(Transport::ScoA2dp, true, false) => {
					"no A2DP: cannot listen with --transport sco-a2dp (use --transport hfp)"
				}
				(Transport::ScoA2dp, false, true) => "no HFP: cannot talk",
				_ if hfp => "profile support incomplete",
				_ => "no HFP: cannot talk or use --transport hfp",
			});
		}
		if connected != "connected" && headset.pipewire.is_none() {
			line.push_str("; PipeWire profiles are known only once connected");
		}
		if problems.is_empty() {
			usable += 1;
			findings.push(Finding::info(line));
		} else {
			findings.push(Finding::warn(format!("{line}; {}", problems.join(", "))));
		}
	}

	let known = inputs.controllers.as_ref().map(Vec::len);
	let limit = sco_limit.max(1);
	let a2dp = a2dp_limit.max(1);
	let per_adapter = headsets_per_controller(mode, transport, limit, a2dp);
	for (controller, addresses) in &per_controller {
		if addresses.len() > per_adapter {
			findings.push(Finding::warn(format!(
				"{controller}: {} headsets exceed the {per_adapter} per adapter supported with these options ({}); add a USB Bluetooth adapter per further {per_adapter} headsets",
				addresses.len(),
				if per_adapter > limit {
					format!("one talker plus --a2dp-limit {a2dp} A2DP listener(s)")
				} else {
					format!("--sco-limit {limit}; every headset keeps HFP/SCO open")
				}
			)));
		}
	}
	if let Some(controllers) = known.filter(|count| *count > 0) {
		findings.push(Finding::info(format!(
			"Capacity (assumes --a2dp-limit {a2dp} A2DP stream(s) and --sco-limit {limit} SCO link(s) per adapter; not measured, verify on hardware): \
			 half-duplex with sco-a2dp supports up to {} headsets on {controllers} adapter(s) (one talker plus {a2dp} A2DP listener(s) per adapter); \
			 full-duplex or hfp supports up to {} headsets",
			controllers * headsets_per_controller(Mode::HalfDuplex, Transport::ScoA2dp, limit, a2dp),
			controllers * limit,
		)));
		if usable < inputs.headsets.len() {
			findings.push(Finding::warn(format!(
				"{usable} of {} configured headsets are usable with --transport {}",
				inputs.headsets.len(),
				match transport {
					Transport::ScoA2dp => "sco-a2dp",
					Transport::Hfp => "hfp",
				}
			)));
		}
	}
	findings
}

/// Probes the adapters and headsets without changing any state.
pub(crate) fn diagnose(
	allowed: &BTreeSet<Address>,
	mode: Mode,
	transport: Transport,
	sco_limit: usize,
	a2dp_limit: usize,
	execute: impl FnMut(&[&str]) -> Result<String, String>,
) -> Vec<Finding> {
	evaluate(
		&collect(allowed, execute),
		mode,
		transport,
		sco_limit,
		a2dp_limit,
	)
}

pub(crate) fn log_findings(findings: &[Finding]) {
	for finding in findings {
		let text: String = finding.text.chars().filter(|c| !c.is_control()).collect();
		match finding.level {
			Level::Info => log::info!("Diagnostics: {text}"),
			Level::Warn => log::warn!("Diagnostics: {text}"),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const ONE: &str = "AA:BB:CC:DD:EE:01";

	fn fake(info: &'static str) -> impl FnMut(&[&str]) -> Result<String, String> {
		move |args| {
			match args {
			["bluetoothctl", "list"] => Ok("Controller 00:11:22:33:44:55 pi [default]\n".into()),
			["bluetoothctl", "show", _] => Ok("Powered: yes\n\tUUID: Audio Source (0000110a-0000-1000-8000-00805f9b34fb)\n\tUUID: Handsfree Audio Gateway (0000111f-0000-1000-8000-00805f9b34fb)\n".into()),
			["bluetoothctl", "info", _] => Ok(info.into()),
			["pw-dump"] => Ok("[]".into()),
			_ => Err("unexpected".into()),
		}
		}
	}

	fn run(info: &'static str, transport: Transport) -> Vec<Finding> {
		let allowed = BTreeSet::from([Address::parse(ONE).unwrap()]);
		diagnose(&allowed, Mode::HalfDuplex, transport, 1, 1, fake(info))
	}

	#[test]
	fn reports_capacity_and_ready_headset_from_bluez_uuids() {
		let findings = run(
			"Name: Buds\nPaired: yes\nTrusted: yes\nConnected: no\n\tUUID: Audio Sink (0000110b-0000-1000-8000-00805f9b34fb)\n\tUUID: Handsfree (0000111e-0000-1000-8000-00805f9b34fb)\n",
			Transport::ScoA2dp,
		);
		assert!(
			findings.iter().all(|f| f.level == Level::Info),
			"{findings:?}"
		);
		assert!(
			findings
				.iter()
				.any(|f| f.text.contains("HFP yes, A2DP yes"))
		);
		assert!(
			findings
				.iter()
				.any(|f| f.text.contains("up to 2 headsets on 1 adapter(s)"))
		);
	}

	#[test]
	fn warns_when_a2dp_or_pairing_is_missing() {
		let findings = run(
			"Name: Buds\nPaired: no\nConnected: no\n\tUUID: Handsfree (0000111e-0000-1000-8000-00805f9b34fb)\n",
			Transport::ScoA2dp,
		);
		let warning = findings.iter().find(|f| f.level == Level::Warn).unwrap();
		assert!(warning.text.contains("not paired") && warning.text.contains("no A2DP"));
	}

	#[test]
	fn unknown_device_and_missing_controller_are_findings_not_errors() {
		let allowed = BTreeSet::from([Address::parse(ONE).unwrap()]);
		let findings = diagnose(&allowed, Mode::HalfDuplex, Transport::ScoA2dp, 1, 1, |_| {
			Err("boom".into())
		});
		assert!(findings.iter().filter(|f| f.level == Level::Warn).count() >= 3);
	}
}
