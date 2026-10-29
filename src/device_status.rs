// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Shared Bluetooth status sampling for terminal views.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crate::bluez::Address;
use crate::bluez::{
	FAIR_RSSI, GOOD_RSSI, STRONG_RSSI, battery_percentage, bluetooth_name, device_flag,
	signal_strength,
};
use crate::process::{COMMAND_TIMEOUT, command_cancellable};
use crate::router::{Headset, Router};
use crate::worker::Worker;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeadsetStatus {
	pub name: Option<String>,
	pub connected: Option<bool>,
	pub duplex: Option<bool>,
	pub rssi: Option<i32>,
	pub battery: Option<u8>,
}

impl HeadsetStatus {
	fn from_info(info: &str, duplex: Option<bool>) -> Self {
		Self {
			name: bluetooth_name(info),
			connected: Some(device_flag(info, "Connected")),
			duplex,
			rssi: signal_strength(info),
			battery: battery_percentage(info),
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ConnectionState {
	Connected,
	Disconnected,
	Unknown,
}

impl ConnectionState {
	pub(crate) fn from_flag(connected: Option<bool>) -> Self {
		match connected {
			Some(true) => Self::Connected,
			Some(false) => Self::Disconnected,
			None => Self::Unknown,
		}
	}

	pub(crate) fn dashboard_label(self) -> &'static str {
		match self {
			Self::Connected => "yes",
			Self::Disconnected => "no",
			Self::Unknown => "?",
		}
	}

	pub(crate) fn panel_label(self) -> &'static str {
		match self {
			Self::Connected => "connected",
			Self::Disconnected => "disconnected",
			Self::Unknown => "unknown",
		}
	}
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SignalQuality {
	Strong,
	Good,
	Fair,
	Weak,
	Unknown,
}

impl SignalQuality {
	pub(crate) fn from_rssi(rssi: Option<i32>) -> Self {
		match rssi {
			Some(rssi) if rssi >= STRONG_RSSI => Self::Strong,
			Some(rssi) if rssi >= GOOD_RSSI => Self::Good,
			Some(rssi) if rssi >= FAIR_RSSI => Self::Fair,
			Some(_) => Self::Weak,
			None => Self::Unknown,
		}
	}

	pub(crate) fn label(self) -> &'static str {
		match self {
			Self::Strong => "strong",
			Self::Good => "good",
			Self::Fair => "fair",
			Self::Weak => "weak",
			Self::Unknown => "unknown",
		}
	}
}

pub(crate) struct StatusPresentation<'a> {
	pub(crate) name: Option<&'a str>,
	pub(crate) connection: ConnectionState,
	pub(crate) duplex: Option<bool>,
	pub(crate) rssi: Option<i32>,
	pub(crate) battery: Option<u8>,
}

impl<'a> StatusPresentation<'a> {
	pub(crate) fn new(status: Option<&'a HeadsetStatus>, duplex: Option<bool>) -> Self {
		Self {
			name: status.and_then(|status| status.name.as_deref()),
			connection: ConnectionState::from_flag(status.and_then(|status| status.connected)),
			duplex,
			rssi: status.and_then(|status| status.rssi),
			battery: status.and_then(|status| status.battery),
		}
	}

	pub(crate) fn name_or(&self, fallback: &'a str) -> &'a str {
		self.name.unwrap_or(fallback)
	}

	pub(crate) fn dashboard_duplex_label(&self) -> &'static str {
		match self.duplex {
			Some(true) => "yes",
			Some(false) => "no",
			None => "?",
		}
	}

	pub(crate) fn panel_duplex_label(&self) -> &'static str {
		match self.duplex {
			Some(true) => "ready",
			Some(false) => "unavailable",
			None => "unknown",
		}
	}

	pub(crate) fn rssi_label(&self) -> String {
		self.rssi
			.map_or_else(|| "—".to_string(), |rssi| format!("{rssi} dBm"))
	}

	/// Battery is only meaningful while the headset is connected.
	pub(crate) fn battery_label(&self) -> String {
		match (self.connection, self.battery) {
			(ConnectionState::Connected, Some(percent)) => format!("{percent}%"),
			_ => "—".to_string(),
		}
	}

	pub(crate) fn dashboard_signal(&self) -> String {
		match (self.connection, self.rssi) {
			(ConnectionState::Connected, Some(rssi)) => format!(
				"{rssi} dBm ({})",
				SignalQuality::from_rssi(Some(rssi)).label()
			),
			_ => "unknown".to_string(),
		}
	}
}

pub(crate) struct StatusUpdate {
	pub statuses: BTreeMap<Address, HeadsetStatus>,
	pub error: Option<String>,
}

/// Dashboard routing is already sampled by the session; only the standalone
/// control panel needs an additional PipeWire inspection.
pub(crate) fn start_polling(
	allowed: BTreeSet<Address>,
	include_audio: bool,
	interval: Duration,
	session_stopped: Arc<AtomicBool>,
) -> (Worker, Receiver<StatusUpdate>) {
	let (sender, receiver) = mpsc::channel();
	// A view may be replaced without cancelling the rest of the session.
	let worker = Worker::spawn(Arc::new(AtomicBool::new(false)), move |stopped, wake| {
		while !session_stopped.load(Ordering::SeqCst) && !stopped.load(Ordering::SeqCst) {
			let Some(update) = poll_status(&allowed, include_audio, &stopped, |args| {
				command_cancellable(args, COMMAND_TIMEOUT, Some(&stopped))
			}) else {
				break;
			};
			if sender.send(update).is_err()
				|| !matches!(
					wake.recv_timeout(interval),
					Err(mpsc::RecvTimeoutError::Timeout)
				) {
				break;
			}
		}
	});
	(worker, receiver)
}

fn poll_status(
	allowed: &BTreeSet<Address>,
	include_audio: bool,
	stopped: &AtomicBool,
	mut execute: impl FnMut(&[&str]) -> Result<String, String>,
) -> Option<StatusUpdate> {
	if stopped.load(Ordering::SeqCst) {
		return None;
	}
	let mut errors = Vec::new();
	let (headsets, audio_checked) = if include_audio {
		let mut router = Router::with_executor(allowed.clone(), &mut execute);
		match router.inspect() {
			Ok((headsets, _)) => (headsets, true),
			Err(error) => {
				errors.push(error);
				(BTreeMap::new(), false)
			}
		}
	} else {
		(BTreeMap::new(), false)
	};
	let mut statuses = BTreeMap::new();
	for address in allowed {
		if stopped.load(Ordering::SeqCst) {
			return None;
		}
		let duplex =
			audio_checked.then(|| headsets.get(address).is_some_and(Headset::has_duplex_audio));
		let status = match execute(&["bluetoothctl", "info", address]) {
			Ok(info) => HeadsetStatus::from_info(&info, duplex),
			Err(error) => {
				errors.push(format!("{address}: {error}"));
				HeadsetStatus {
					duplex,
					..HeadsetStatus::default()
				}
			}
		};
		statuses.insert(address.clone(), status);
	}
	if stopped.load(Ordering::SeqCst) {
		return None;
	}
	Some(StatusUpdate {
		statuses,
		error: (!errors.is_empty()).then(|| errors.join("; ")),
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn stopping_a_view_joins_its_worker_without_cancelling_the_session() {
		let session_stopped = Arc::new(AtomicBool::new(false));
		let (mut worker, updates) = start_polling(
			BTreeSet::new(),
			false,
			Duration::from_secs(30),
			Arc::clone(&session_stopped),
		);
		updates.recv_timeout(Duration::from_secs(2)).unwrap();
		worker.stop();
		assert!(!session_stopped.load(Ordering::SeqCst));
		assert!(matches!(
			updates.try_recv(),
			Err(mpsc::TryRecvError::Disconnected)
		));
	}

	#[test]
	fn parses_bluetooth_status_and_optional_signal() {
		assert_eq!(
			HeadsetStatus::from_info(
				"Name: Generic headset\nAlias: Alex's headset\nPaired: yes\nConnected: yes\nRSSI: -67 (0xffffffbd)\n",
				Some(true)
			),
			HeadsetStatus {
				name: Some("Alex's headset".into()),
				connected: Some(true),
				duplex: Some(true),
				rssi: Some(-67),
				battery: None,
			}
		);
		assert_eq!(
			HeadsetStatus::from_info("RSSI: unavailable\n", Some(false)).rssi,
			None
		);
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
	fn each_device_is_queried_once_and_failures_remain_unknown() {
		let first = "AA:BB:CC:DD:EE:01";
		let second = "AA:BB:CC:DD:EE:02";
		let allowed = BTreeSet::from([first.into(), second.into()]);
		for include_audio in [false, true] {
			let mut calls = Vec::new();
			let update = poll_status(&allowed, include_audio, &AtomicBool::new(false), |args| {
				calls.push(args.join(" "));
				match args {
					["pw-dump"] => Ok("[]".into()),
					["bluetoothctl", "info", address] if *address == first => {
						Ok("Connected: yes\nAlias: Headset".into())
					}
					_ => Err("device unavailable".into()),
				}
			})
			.unwrap();
			let expected = if include_audio {
				vec![
					"pw-dump",
					"bluetoothctl info AA:BB:CC:DD:EE:01",
					"bluetoothctl info AA:BB:CC:DD:EE:02",
				]
			} else {
				vec![
					"bluetoothctl info AA:BB:CC:DD:EE:01",
					"bluetoothctl info AA:BB:CC:DD:EE:02",
				]
			};
			assert_eq!(calls, expected);
			assert_eq!(update.statuses[first].connected, Some(true));
			assert_eq!(update.statuses[second].connected, None);
			assert_eq!(
				update.statuses[first].duplex,
				include_audio.then_some(false)
			);
			assert!(
				update
					.error
					.unwrap()
					.contains(&format!("{second}: device unavailable"))
			);
		}
	}

	#[test]
	fn cancellation_discards_partial_updates_and_stops_commands() {
		let stopped = AtomicBool::new(false);
		let mut calls = 0;
		let first = "AA:BB:CC:DD:EE:01";
		let second = "AA:BB:CC:DD:EE:02";
		assert!(
			poll_status(
				&[first.into(), second.into()].into(),
				false,
				&stopped,
				|_| {
					calls += 1;
					stopped.store(true, Ordering::SeqCst);
					Ok("Connected: yes".into())
				}
			)
			.is_none()
		);
		assert_eq!(calls, 1);
		assert!(
			poll_status(&BTreeSet::new(), true, &stopped, |_| {
				panic!("cancelled poll must not execute commands");
			})
			.is_none()
		);
	}

	#[test]
	fn audio_errors_do_not_hide_bluetooth_status() {
		let address = "AA:BB:CC:DD:EE:01";
		let update = poll_status(
			&[address.into()].into(),
			true,
			&AtomicBool::new(false),
			|args| {
				if args == ["pw-dump"] {
					Err("PipeWire unavailable".into())
				} else {
					Ok("Connected: no".into())
				}
			},
		)
		.unwrap();
		assert_eq!(update.statuses[address].connected, Some(false));
		assert_eq!(update.statuses[address].duplex, None);
		assert_eq!(update.error.as_deref(), Some("PipeWire unavailable"));
	}

	#[test]
	fn presentation_shares_status_semantics_without_collapsing_view_labels() {
		let status = HeadsetStatus {
			name: Some("Dispatch headset".into()),
			connected: Some(true),
			duplex: Some(true),
			rssi: Some(-67),
			battery: Some(80),
		};
		let presentation = StatusPresentation::new(Some(&status), status.duplex);

		assert_eq!(presentation.name_or("fallback"), "Dispatch headset");
		assert_eq!(presentation.connection.dashboard_label(), "yes");
		assert_eq!(presentation.connection.panel_label(), "connected");
		assert_eq!(presentation.dashboard_duplex_label(), "yes");
		assert_eq!(presentation.panel_duplex_label(), "ready");
		assert_eq!(presentation.rssi_label(), "-67 dBm");
		assert_eq!(presentation.dashboard_signal(), "-67 dBm (good)");
		assert_eq!(presentation.battery_label(), "80%");

		let unknown = StatusPresentation::new(None, None);
		assert_eq!(unknown.name_or("fallback"), "fallback");
		assert_eq!(unknown.connection.dashboard_label(), "?");
		assert_eq!(unknown.connection.panel_label(), "unknown");
		assert_eq!(unknown.dashboard_duplex_label(), "?");
		assert_eq!(unknown.panel_duplex_label(), "unknown");
		assert_eq!(unknown.rssi_label(), "—");
		assert_eq!(unknown.dashboard_signal(), "unknown");
		assert_eq!(unknown.battery_label(), "—");

		let disconnected_status = HeadsetStatus {
			connected: Some(false),
			rssi: Some(-40),
			battery: Some(50),
			..HeadsetStatus::default()
		};
		let disconnected = StatusPresentation::new(Some(&disconnected_status), Some(false));
		assert_eq!(disconnected.dashboard_signal(), "unknown");
		assert_eq!(disconnected.rssi_label(), "-40 dBm");
		assert_eq!(disconnected.battery_label(), "—");
		assert_eq!(SignalQuality::from_rssi(Some(-60)), SignalQuality::Strong);
		assert_eq!(SignalQuality::from_rssi(Some(-70)), SignalQuality::Good);
		assert_eq!(SignalQuality::from_rssi(Some(-80)), SignalQuality::Fair);
		assert_eq!(SignalQuality::from_rssi(Some(-81)), SignalQuality::Weak);
		assert_eq!(SignalQuality::from_rssi(None), SignalQuality::Unknown);
	}
}
