// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Background Bluetooth status polling and terminal dashboard rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::bluez::{bluetooth_name, device_flag, property};
use crate::command_cancellable;
use crate::router::Headset;

#[derive(Debug, PartialEq, Eq)]
/// Bluetooth connection and optional signal-strength information.
pub struct Bluetooth {
	name: Option<String>,
	connected: bool,
	rssi: Option<i32>,
}

fn bluetooth_info(info: &str) -> Bluetooth {
	let rssi =
		property(info, "RSSI").and_then(|value| value.split_whitespace().next()?.parse().ok());
	Bluetooth {
		name: bluetooth_name(info),
		connected: device_flag(info, "Connected"),
		rssi,
	}
}

/// Collects Bluetooth status updates and draws the current routing state.
///
/// The polling worker can be stopped explicitly with [`Dashboard::stop`].
pub struct Dashboard {
	receiver: Receiver<(String, Option<Bluetooth>)>,
	state: BTreeMap<String, Option<Bluetooth>>,
	shutdown: mpsc::Sender<()>,
	worker: Option<JoinHandle<()>>,
}

impl Dashboard {
	/// Starts polling each allowlisted device until shutdown or cancellation.
	pub fn start(allowed: BTreeSet<String>, stopped: Arc<AtomicBool>) -> Self {
		let (sender, receiver) = mpsc::channel();
		let (shutdown, wake) = mpsc::channel();
		let worker = thread::spawn(move || {
			loop {
				for address in &allowed {
					if stopped.load(Ordering::SeqCst) {
						return;
					}
					let info = command_cancellable(
						&["bluetoothctl", "info", address],
						Duration::from_secs(15),
						Some(&stopped),
					)
					.ok()
					.map(|output| bluetooth_info(&output));
					if sender.send((address.clone(), info)).is_err() {
						return;
					}
				}
				if wake.recv_timeout(Duration::from_secs(10)).is_ok()
					|| stopped.load(Ordering::SeqCst)
				{
					return;
				}
			}
		});
		Self {
			receiver,
			state: BTreeMap::new(),
			shutdown,
			worker: Some(worker),
		}
	}

	/// Applies queued worker updates and reports whether the displayed state changed.
	pub fn refresh(&mut self) -> bool {
		let mut changed = false;
		while let Ok((address, info)) = self.receiver.try_recv() {
			self.state.insert(address, info);
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
		transmitting: bool,
		error: Option<&str>,
		output: &mut impl Write,
	) -> io::Result<()> {
		writeln!(
			output,
			"\x1b[H\x1b[2Jbt-intercom | {} | {} owned active links",
			if transmitting {
				"transmitting"
			} else {
				"muted"
			},
			links.len()
		)?;
		writeln!(
			output,
			"HEADSET NAME             ADDRESS             CONNECTED  DUPLEX  TX/RX LINKS  SIGNAL"
		)?;
		for address in allowed {
			let bluetooth = self.state.get(address).and_then(Option::as_ref);
			let status = |flag: Option<bool>| match flag {
				Some(true) => "yes",
				Some(false) => "no",
				None => "?",
			};
			let headset = headsets.get(address);
			let (tx, rx) = headset.map_or((0, 0), |headset| {
				(
					links
						.iter()
						.filter(|(out, _)| headset.sources.iter().any(|port| port.id == *out))
						.count(),
					links
						.iter()
						.filter(|(_, input)| headset.sinks.iter().any(|port| port.id == *input))
						.count(),
				)
			});
			let signal = bluetooth
				.filter(|info| info.connected)
				.and_then(|info| info.rssi)
				.map_or_else(
					|| "unknown".to_string(),
					|rssi| {
						let quality = match rssi {
							-60.. => "strong",
							-70..=-61 => "good",
							-80..=-71 => "fair",
							_ => "weak",
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
				status(bluetooth.map(|info| info.connected)),
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
		output.flush()
	}

	/// Signals the polling worker to exit and waits for it to finish.
	pub fn stop(&mut self) {
		let _ = self.shutdown.send(());
		if let Some(worker) = self.worker.take() {
			let _ = worker.join();
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn parses_bluetooth_status_and_optional_signal() {
		assert_eq!(
			bluetooth_info(
				"Name: Generic headset\nAlias: Alex's headset\nPaired: yes\nConnected: yes\nRSSI: -67 (0xffffffbd)\n"
			),
			Bluetooth {
				name: Some("Alex's headset".into()),
				connected: true,
				rssi: Some(-67)
			}
		);
		assert_eq!(bluetooth_info("RSSI: unavailable\n").rssi, None);
		assert_eq!(
			bluetooth_name("Name: Device name\nAlias: \n"),
			Some("Device name".into())
		);
		assert_eq!(
			bluetooth_name("Alias: Unsafe\u{1b}[31m name"),
			Some("Unsafe[31m name".into())
		);
	}

	#[test]
	fn renders_unknown_status_and_observed_owned_links() {
		let allowed = BTreeSet::from(["AA:BB:CC:DD:EE:01".to_string()]);
		let (sender, receiver) = mpsc::channel();
		drop(sender);
		let (shutdown, _) = mpsc::channel();
		let dashboard = Dashboard {
			receiver,
			state: BTreeMap::new(),
			shutdown,
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
				false,
				None,
				&mut output,
			)
			.unwrap();
		let text = String::from_utf8(output).unwrap();
		assert!(text.contains("muted | 1 owned active links"));
		assert!(text.contains("HEADSET NAME             ADDRESS             CONNECTED"));
		assert!(!text.contains("PAIRED"));
		assert!(text.contains("AA:BB:CC:DD:EE:01  ?"));
		assert!(text.contains("no"));
		assert!(text.contains("1/0"));
		assert!(text.contains("unknown"));
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
					Some(Bluetooth {
						name: None,
						connected: true,
						rssi: Some(*rssi),
					}),
				)
			})
			.chain([(
				disconnected.clone(),
				Some(Bluetooth {
					name: None,
					connected: false,
					rssi: Some(-40),
				}),
			)])
			.collect();
		let (sender, receiver) = mpsc::channel();
		drop(sender);
		let (shutdown, _) = mpsc::channel();
		let dashboard = Dashboard {
			receiver,
			state,
			shutdown,
			worker: None,
		};
		let mut output = Vec::new();
		dashboard
			.draw(
				&allowed,
				&BTreeMap::new(),
				&BTreeSet::new(),
				false,
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
		let (shutdown, _) = mpsc::channel();
		let dashboard = Dashboard {
			receiver,
			state: BTreeMap::from([(
				address,
				Some(Bluetooth {
					name: Some("Alex's headset".into()),
					connected: true,
					rssi: None,
				}),
			)]),
			shutdown,
			worker: None,
		};
		let mut output = Vec::new();
		dashboard
			.draw(
				&allowed,
				&BTreeMap::new(),
				&BTreeSet::new(),
				false,
				None,
				&mut output,
			)
			.unwrap();
		let text = String::from_utf8(output).unwrap();
		assert!(text.contains("Alex's headset           AA:BB:CC:DD:EE:01"));
	}
}
