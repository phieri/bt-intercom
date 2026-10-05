// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Shared Bluetooth status sampling for terminal views.

use std::collections::{BTreeMap, BTreeSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::time::Duration;

use crate::bluez::{bluetooth_name, device_flag, signal_strength};
use crate::process::{COMMAND_TIMEOUT, command_cancellable};
use crate::router::{Headset, Router};
use crate::worker::Worker;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct HeadsetStatus {
	pub name: Option<String>,
	pub connected: Option<bool>,
	pub duplex: bool,
	pub rssi: Option<i32>,
}

impl HeadsetStatus {
	fn from_info(info: &str, duplex: bool) -> Self {
		Self {
			name: bluetooth_name(info),
			connected: Some(device_flag(info, "Connected")),
			duplex,
			rssi: signal_strength(info),
		}
	}
}

pub(crate) struct StatusUpdate {
	pub statuses: BTreeMap<String, HeadsetStatus>,
	pub error: Option<String>,
}

/// Dashboard routing is already sampled by the session; only the standalone
/// control panel needs an additional PipeWire inspection.
pub(crate) fn start_polling(
	allowed: BTreeSet<String>,
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
	allowed: &BTreeSet<String>,
	include_audio: bool,
	stopped: &AtomicBool,
	mut execute: impl FnMut(&[&str]) -> Result<String, String>,
) -> Option<StatusUpdate> {
	if stopped.load(Ordering::SeqCst) {
		return None;
	}
	let mut errors = Vec::new();
	let headsets = if include_audio {
		let mut router = Router::with_executor(allowed.clone(), &mut execute);
		match router.inspect() {
			Ok((headsets, _)) => headsets,
			Err(error) => {
				errors.push(error);
				BTreeMap::new()
			}
		}
	} else {
		BTreeMap::new()
	};
	let mut statuses = BTreeMap::new();
	for address in allowed {
		if stopped.load(Ordering::SeqCst) {
			return None;
		}
		let duplex = headsets.get(address).is_some_and(Headset::has_duplex_audio);
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
				true
			),
			HeadsetStatus {
				name: Some("Alex's headset".into()),
				connected: Some(true),
				duplex: true,
				rssi: Some(-67)
			}
		);
		assert_eq!(
			HeadsetStatus::from_info("RSSI: unavailable\n", false).rssi,
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
		let allowed = BTreeSet::from(["A".into(), "B".into()]);
		for include_audio in [false, true] {
			let mut calls = Vec::new();
			let update = poll_status(&allowed, include_audio, &AtomicBool::new(false), |args| {
				calls.push(args.join(" "));
				match args {
					["pw-dump"] => Ok("[]".into()),
					["bluetoothctl", "info", "A"] => Ok("Connected: yes\nAlias: Headset".into()),
					_ => Err("device unavailable".into()),
				}
			})
			.unwrap();
			let expected = if include_audio {
				vec!["pw-dump", "bluetoothctl info A", "bluetoothctl info B"]
			} else {
				vec!["bluetoothctl info A", "bluetoothctl info B"]
			};
			assert_eq!(calls, expected);
			assert_eq!(update.statuses["A"].connected, Some(true));
			assert_eq!(update.statuses["B"].connected, None);
			assert!(update.error.unwrap().contains("B: device unavailable"));
		}
	}

	#[test]
	fn cancellation_discards_partial_updates_and_stops_commands() {
		let stopped = AtomicBool::new(false);
		let mut calls = 0;
		assert!(
			poll_status(&["A".into(), "B".into()].into(), false, &stopped, |_| {
				calls += 1;
				stopped.store(true, Ordering::SeqCst);
				Ok("Connected: yes".into())
			})
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
		let update = poll_status(
			&["A".into()].into(),
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
		assert_eq!(update.statuses["A"].connected, Some(false));
		assert!(!update.statuses["A"].duplex);
		assert_eq!(update.error.as_deref(), Some("PipeWire unavailable"));
	}
}
