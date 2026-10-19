// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Bounded Bluetooth reconnection for a running session.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::Duration;

use crate::bluez;
use crate::bluez::Address;
use crate::process::COMMAND_TIMEOUT;

const MAX_CONCURRENT_RECONNECTS: usize = 2;

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
					let connected = execute(&["bluetoothctl", "info", device], COMMAND_TIMEOUT)
						.is_ok_and(|info| bluez::device_flag(&info, "Connected"));
					if !connected
						&& !stopped.load(Ordering::SeqCst)
						&& let Err(error) = execute(
							&["bluetoothctl", "--timeout", "30", "connect", device],
							Duration::from_secs(35),
						) {
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

pub(crate) fn reconnect_worker(
	allowed: Arc<Mutex<BTreeSet<Address>>>,
	stopped: Arc<AtomicBool>,
	shutdown: Receiver<()>,
	retry: Duration,
	execute: impl Fn(&[&str], Duration) -> Result<String, String> + Sync,
) {
	loop {
		let devices = allowed.lock().unwrap().clone();
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
