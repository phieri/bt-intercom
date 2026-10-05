// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Background Bluetooth status polling and terminal dashboard rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::time::Duration;

use crate::bluez::{FAIR_RSSI, GOOD_RSSI, STRONG_RSSI};
use crate::device_status::{HeadsetStatus, StatusUpdate, start_polling};
use crate::router::Headset;
use crate::routing_status::RoutingStatus;
use crate::transmit::Mode;
use crate::worker::Worker;

/// Collects Bluetooth status updates and draws the current routing state.
///
/// The polling worker can be stopped explicitly with [`Dashboard::stop`].
pub struct Dashboard {
	receiver: Receiver<StatusUpdate>,
	state: BTreeMap<String, HeadsetStatus>,
	status_error: Option<String>,
	worker: Option<Worker>,
}

pub struct RunStatus {
	pub mode: Mode,
	pub transmitting: bool,
}

impl Dashboard {
	/// Starts polling each allowlisted device until shutdown or cancellation.
	pub fn start(allowed: BTreeSet<String>, stopped: Arc<AtomicBool>) -> Self {
		let (worker, receiver) = start_polling(allowed, false, Duration::from_secs(10), stopped);
		Self {
			receiver,
			state: BTreeMap::new(),
			status_error: None,
			worker: Some(worker),
		}
	}

	/// Applies queued worker updates and reports whether the displayed state changed.
	pub fn refresh(&mut self) -> bool {
		let mut changed = false;
		while let Ok(update) = self.receiver.try_recv() {
			self.state = update.statuses;
			self.status_error = update.error;
			changed = true;
		}
		changed
	}

	/// Renders the latest Bluetooth and routing state to the supplied writer.
	pub fn draw(
		&self,
		allowed: &BTreeSet<String>,
		headsets: &BTreeMap<String, Headset>,
		links: &BTreeSet<(u64, u64)>,
		status: RunStatus,
		error: Option<&str>,
		output: &mut impl Write,
	) -> io::Result<()> {
		let routes = RoutingStatus::new(headsets, links);
		writeln!(
			output,
			"bt-intercom | {} | {} | {} owned active links",
			match status.mode {
				Mode::HalfDuplex => "half-duplex",
				Mode::FullDuplex => "full-duplex",
			},
			if status.transmitting {
				"transmitting"
			} else {
				"muted"
			},
			routes.total_links()
		)?;
		writeln!(
			output,
			"HEADSET NAME             ADDRESS             CONNECTED  DUPLEX  TX/RX LINKS  SIGNAL"
		)?;
		for address in allowed {
			let bluetooth = self.state.get(address);
			let status = |flag: Option<bool>| match flag {
				Some(true) => "yes",
				Some(false) => "no",
				None => "?",
			};
			let headset = headsets.get(address);
			let (tx, rx) = routes.counts(address);
			let signal = bluetooth
				.filter(|info| info.connected == Some(true))
				.and_then(|info| info.rssi)
				.map_or_else(
					|| "unknown".to_string(),
					|rssi| {
						let quality = if rssi >= STRONG_RSSI {
							"strong"
						} else if rssi >= GOOD_RSSI {
							"good"
						} else if rssi >= FAIR_RSSI {
							"fair"
						} else {
							"weak"
						};
						format!("{rssi} dBm ({quality})")
					},
				);
			writeln!(
				output,
				"{:<24} {address}  {:<9}  {:<6}  {tx:>2}/{rx:<2}         {signal}",
				bluetooth
					.and_then(|info| info.name.as_deref())
					.unwrap_or("—")
					.chars()
					.take(24)
					.collect::<String>(),
				status(bluetooth.and_then(|info| info.connected)),
				status(headset.map(Headset::has_duplex_audio))
			)?;
		}
		writeln!(
			output,
			"\nLinks represent routing, not measured speech; RSSI is shown only if BlueZ reports it."
		)?;
		if let Some(error) = error {
			writeln!(
				output,
				"Last routing error: {}",
				error
					.chars()
					.filter(|ch| !ch.is_control())
					.collect::<String>()
			)?;
		}
		if let Some(error) = &self.status_error {
			writeln!(
				output,
				"Last status error: {}",
				error
					.chars()
					.filter(|ch| !ch.is_control())
					.collect::<String>()
			)?;
		}
		output.flush()
	}

	/// Signals the polling worker to exit and waits for it to finish.
	pub fn stop(&mut self) {
		self.worker.take();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;
	use std::sync::mpsc;

	#[test]
	fn polling_errors_are_reported_and_cleared_by_recovery() {
		let (sender, receiver) = mpsc::channel();
		let mut dashboard = Dashboard {
			receiver,
			state: BTreeMap::new(),
			status_error: None,
			worker: None,
		};
		sender
			.send(StatusUpdate {
				statuses: BTreeMap::new(),
				error: Some("Bluetooth\nunavailable".into()),
			})
			.unwrap();
		assert!(dashboard.refresh());
		let mut output = Vec::new();
		dashboard
			.draw(
				&BTreeSet::new(),
				&BTreeMap::new(),
				&BTreeSet::new(),
				RunStatus {
					mode: Mode::FullDuplex,
					transmitting: false,
				},
				None,
				&mut output,
			)
			.unwrap();
		assert!(
			String::from_utf8(output)
				.unwrap()
				.contains("Last status error: Bluetoothunavailable")
		);
		sender
			.send(StatusUpdate {
				statuses: BTreeMap::new(),
				error: None,
			})
			.unwrap();
		assert!(dashboard.refresh());
		assert!(dashboard.status_error.is_none());
		assert!(!dashboard.refresh());
	}

	#[test]
	fn renders_unknown_status_and_observed_owned_links() {
		let allowed = BTreeSet::from(["AA:BB:CC:DD:EE:01".to_string()]);
		let (sender, receiver) = mpsc::channel();
		drop(sender);
		let dashboard = Dashboard {
			receiver,
			state: BTreeMap::new(),
			status_error: None,
			worker: None,
		};
		let objects = json!([
			{"type":"PipeWire:Interface:Device","id":1,"info":{"props":{"api.bluez5.address":"AA:BB:CC:DD:EE:01"}}},
			{"type":"PipeWire:Interface:Node","id":2,"info":{"props":{"device.id":"1","media.class":"Audio/Source","api.bluez5.profile":"headset-head-unit"}}},
			{"type":"PipeWire:Interface:Port","id":3,"info":{"props":{"node.id":"2","port.direction":"out"}}}
		]);
		let (headsets, _) = crate::router::topology(&objects, &allowed).unwrap();
		let mut output = Vec::new();
		dashboard
			.draw(
				&allowed,
				&headsets,
				&BTreeSet::from([(3, 4)]),
				RunStatus {
					mode: Mode::HalfDuplex,
					transmitting: false,
				},
				None,
				&mut output,
			)
			.unwrap();
		let text = String::from_utf8(output).unwrap();
		assert!(!text.contains('\u{1b}'));
		assert!(text.contains("half-duplex | muted | 0 owned active links"));
		assert!(text.contains("HEADSET NAME             ADDRESS             CONNECTED"));
		assert!(!text.contains("PAIRED"));
		assert!(text.contains("AA:BB:CC:DD:EE:01  ?"));
		assert!(text.contains("no"));
		assert!(text.contains("0/0"));
		assert!(text.contains("unknown"));
	}

	#[test]
	fn rendered_counts_match_confirmation_readiness() {
		let allowed = BTreeSet::from([
			"AA:BB:CC:DD:EE:01".to_string(),
			"AA:BB:CC:DD:EE:02".to_string(),
		]);
		let objects = json!([
			{"type":"PipeWire:Interface:Device","id":1,"info":{"props":{"api.bluez5.address":"AA:BB:CC:DD:EE:01"}}},
			{"type":"PipeWire:Interface:Node","id":2,"info":{"props":{"device.id":1,"media.class":"Audio/Source","api.bluez5.profile":"headset-head-unit"}}},
			{"type":"PipeWire:Interface:Port","id":3,"info":{"props":{"node.id":2,"port.direction":"out"}}},
			{"type":"PipeWire:Interface:Device","id":4,"info":{"props":{"api.bluez5.address":"AA:BB:CC:DD:EE:02"}}},
			{"type":"PipeWire:Interface:Node","id":5,"info":{"props":{"device.id":4,"media.class":"Audio/Sink","api.bluez5.profile":"headset-head-unit"}}},
			{"type":"PipeWire:Interface:Port","id":6,"info":{"props":{"node.id":5,"port.direction":"in"}}}
		]);
		let (headsets, _) = crate::router::topology(&objects, &allowed).unwrap();
		let links = BTreeSet::from([(3, 6), (3, 99), (99, 6)]);
		let routes = RoutingStatus::new(&headsets, &links);
		assert!(routes.has_active_source_route("AA:BB:CC:DD:EE:01"));
		assert!(!routes.has_active_intercom_connection("AA:BB:CC:DD:EE:01"));
		let (_, receiver) = mpsc::channel();
		let dashboard = Dashboard {
			receiver,
			state: BTreeMap::new(),
			status_error: None,
			worker: None,
		};
		let mut output = Vec::new();
		dashboard
			.draw(
				&allowed,
				&headsets,
				&links,
				RunStatus {
					mode: Mode::HalfDuplex,
					transmitting: true,
				},
				None,
				&mut output,
			)
			.unwrap();
		let text = String::from_utf8(output).unwrap();
		assert!(text.contains("1 owned active links"));
		assert!(
			text.lines()
				.any(|line| line.contains("EE:01") && line.contains("1/0"))
		);
		assert!(
			text.lines()
				.any(|line| line.contains("EE:02") && line.contains("0/1"))
		);
	}

	#[test]
	fn renders_rssi_quality_boundaries_and_hides_disconnected_signal() {
		let levels = [
			(-60, "strong"),
			(-61, "good"),
			(-70, "good"),
			(-71, "fair"),
			(-80, "fair"),
			(-81, "weak"),
		];
		let addresses = levels
			.iter()
			.enumerate()
			.map(|(index, _)| format!("AA:BB:CC:DD:EE:{:02}", index + 1))
			.collect::<BTreeSet<_>>();
		let disconnected = "AA:BB:CC:DD:EE:07".to_string();
		let allowed = addresses
			.iter()
			.cloned()
			.chain([disconnected.clone()])
			.collect();
		let state = levels
			.iter()
			.enumerate()
			.map(|(index, (rssi, _))| {
				(
					format!("AA:BB:CC:DD:EE:{:02}", index + 1),
					HeadsetStatus {
						name: None,
						connected: Some(true),
						duplex: false,
						rssi: Some(*rssi),
					},
				)
			})
			.chain([(
				disconnected.clone(),
				HeadsetStatus {
					name: None,
					connected: Some(false),
					duplex: false,
					rssi: Some(-40),
				},
			)])
			.collect();
		let (sender, receiver) = mpsc::channel();
		drop(sender);
		let dashboard = Dashboard {
			receiver,
			state,
			status_error: None,
			worker: None,
		};
		let mut output = Vec::new();
		dashboard
			.draw(
				&allowed,
				&BTreeMap::new(),
				&BTreeSet::new(),
				RunStatus {
					mode: Mode::FullDuplex,
					transmitting: false,
				},
				None,
				&mut output,
			)
			.unwrap();
		let text = String::from_utf8(output).unwrap();
		for (rssi, quality) in levels {
			assert!(text.contains(&format!("{rssi} dBm ({quality})")));
		}
		let disconnected_row = text
			.lines()
			.find(|line| line.contains(&disconnected))
			.unwrap();
		assert!(disconnected_row.contains("unknown"));
	}

	#[test]
	fn renders_bluetooth_alias_with_the_address() {
		let address = "AA:BB:CC:DD:EE:01".to_string();
		let allowed = BTreeSet::from([address.clone()]);
		let (sender, receiver) = mpsc::channel();
		drop(sender);
		let dashboard = Dashboard {
			receiver,
			state: BTreeMap::from([(
				address,
				HeadsetStatus {
					name: Some("Alex's headset".into()),
					connected: Some(true),
					duplex: false,
					rssi: None,
				},
			)]),
			status_error: None,
			worker: None,
		};
		let mut output = Vec::new();
		dashboard
			.draw(
				&allowed,
				&BTreeMap::new(),
				&BTreeSet::new(),
				RunStatus {
					mode: Mode::FullDuplex,
					transmitting: false,
				},
				None,
				&mut output,
			)
			.unwrap();
		let text = String::from_utf8(output).unwrap();
		assert!(text.contains("Alex's headset           AA:BB:CC:DD:EE:01"));
	}
}
