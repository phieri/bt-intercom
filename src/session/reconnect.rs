// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Bounded Bluetooth reconnection for a running session.

use std::collections::BTreeSet;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread;
use std::time::Duration;

use crate::bluez;
use crate::bluez::Address;
use crate::bluez::checked;
use crate::process::COMMAND_TIMEOUT;

const MAX_CONCURRENT_RECONNECTS: usize = 1;

pub(crate) fn connect_disconnected(
	allowed: &BTreeSet<Address>,
	stopped: &AtomicBool,
	execute: impl Fn(&[&str], Duration) -> Result<String, String> + Sync,
) {
	let devices: Vec<_> = allowed.iter().collect();
	for batch in devices.chunks(MAX_CONCURRENT_RECONNECTS) {
		thread::scope(|scope| {
			for device in batch {
				let execute = &execute;
				scope.spawn(move || {
					if stopped.load(Ordering::SeqCst) {
						return;
					}
					let info = execute(&["bluetoothctl", "info", device], COMMAND_TIMEOUT);
					let connected = match &info {
						Ok(info) => {
							if bluez::property(info, "Paired") == Some("no") {
								log::warn!(
									"{device} is not paired; run `bt-intercom pair {device}` first"
								);
								return;
							}
							if bluez::device_flag(info, "Blocked") {
								log::warn!(
									"{device} is blocked in BlueZ; run `bluetoothctl unblock {device}`"
								);
								return;
							}
							// Untrusted devices may not be reconnected by BlueZ.
							if bluez::property(info, "Trusted") == Some("no")
								&& let Err(error) = checked(execute(
									&["bluetoothctl", "trust", device],
									COMMAND_TIMEOUT,
								)) {
								log::warn!("Could not trust {device}: {error}");
							}
							bluez::device_flag(info, "Connected")
						}
						Err(_) => false,
					};
					if !connected
						&& !stopped.load(Ordering::SeqCst)
						&& let Err(error) = checked(execute(
							&["bluetoothctl", "--timeout", "30", "connect", device],
							Duration::from_secs(35),
						)) {
						log::warn!("Could not connect {device}: {error}");
					}
				});
			}
		});
		if stopped.load(Ordering::SeqCst) {
			break;
		}
	}
}

/// Powers on the adapter when BlueZ reports it off, e.g. after boot or rfkill.
/// Failures are warnings: the next cycle retries.
pub(crate) fn ensure_adapter_powered(
	execute: impl Fn(&[&str], Duration) -> Result<String, String>,
) {
	let Ok(info) = execute(&["bluetoothctl", "show"], COMMAND_TIMEOUT) else {
		return;
	};
	if info.contains("No default controller") {
		log::warn!(
			"No Bluetooth controller found; check that bluetooth.service is running and the adapter firmware has initialized"
		);
	} else if bluez::property(&info, "Powered") == Some("no") {
		log::info!("Bluetooth adapter is powered off; powering it on");
		if let Err(error) = checked(execute(&["bluetoothctl", "power", "on"], COMMAND_TIMEOUT)) {
			log::warn!(
				"Could not power on the Bluetooth adapter: {error}; check `rfkill list` and run `rfkill unblock bluetooth`"
			);
		}
	}
}

pub(crate) fn reconnect_worker(
	devices: BTreeSet<Address>,
	stopped: Arc<AtomicBool>,
	shutdown: Receiver<()>,
	retry: Duration,
	execute: impl Fn(&[&str], Duration) -> Result<String, String> + Sync,
) {
	loop {
		ensure_adapter_powered(&execute);
		connect_disconnected(&devices, &stopped, &execute);
		if stopped.load(Ordering::SeqCst) {
			break;
		}
		if !matches!(
			shutdown.recv_timeout(retry),
			Err(mpsc::RecvTimeoutError::Timeout)
		) {
			break;
		}
	}
}
