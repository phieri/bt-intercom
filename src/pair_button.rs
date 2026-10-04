// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Opt-in Raspberry Pi GPIO pairing, independent of the audio polling loop.

use std::collections::BTreeSet;
use std::io::{Read, Write};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use rppal::gpio::{Gpio, InputPin};

use crate::bluez::device_flag;
use crate::process::COMMAND_TIMEOUT;

const GPIO: u8 = 17;
const DEBOUNCE: Duration = Duration::from_millis(60);

struct Button {
	candidate: bool,
	since: Instant,
	stable: bool,
	armed: bool,
}

impl Button {
	fn new(pressed: bool, now: Instant) -> Self {
		Self {
			candidate: pressed,
			since: now,
			stable: true,
			armed: false,
		}
	}

	fn sample(&mut self, pressed: bool, now: Instant) -> bool {
		if pressed != self.candidate {
			self.candidate = pressed;
			self.since = now;
		}
		if now.duration_since(self.since) < DEBOUNCE || pressed == self.stable {
			return false;
		}
		self.stable = pressed;
		if !pressed {
			self.armed = true;
			false
		} else {
			std::mem::take(&mut self.armed)
		}
	}
}

pub(crate) struct PairButton {
	pub(crate) events: Receiver<Result<String, String>>,
	cancelled: Arc<AtomicBool>,
	worker: Option<JoinHandle<()>>,
}

impl PairButton {
	// Acquire the pin before starting other workers, so permission/model errors
	// cannot leave background activity behind.
	pub(crate) fn open() -> Result<InputPin, String> {
		Gpio::new()
			.and_then(|gpio| gpio.get(GPIO))
			.map(|pin| pin.into_input_pullup())
			.map_err(|error| {
				format!(
					"GPIO17 pairing button unavailable: {error}; use a supported Raspberry Pi and grant this user GPIO access (do not run with sudo)"
				)
			})
	}

	pub(crate) fn start(
		pin: InputPin,
		mut allowed: BTreeSet<String>,
		cancelled: Arc<AtomicBool>,
		network_path: PathBuf,
	) -> Self {
		let stopped = Arc::clone(&cancelled);
		let (sender, events) = mpsc::channel();
		let worker = thread::spawn(move || {
			let mut button = Button::new(pin.is_low(), Instant::now());
			log::info!("Pairing button ready on BCM GPIO17 (physical pin 11)");
			while !stopped.load(Ordering::SeqCst) {
				if button.sample(pin.is_low(), Instant::now()) {
					log::info!("Pairing button pressed; scanning for one unpaired headset");
					let result = pair_headset(
						&allowed,
						|args, timeout| {
							crate::process::command_cancellable(args, timeout, Some(&stopped))
						},
						|device| {
							headless_pair_session(
								&["bluetoothctl", "--agent", "NoInputNoOutput"],
								device,
								&stopped,
							)
						},
						rollback_headset,
					);
					let result = result.and_then(|device| {
						if let Err(error) =
							crate::network::enroll(&network_path, &mut allowed, device.clone())
						{
							return Err(rollback_error(&device, error, rollback_headset));
						}
						Ok(device)
					});
					if stopped.load(Ordering::SeqCst) {
						if let Err(error) = result {
							log::warn!("Button pairing stopped: {error}");
						}
						break;
					}
					if sender.send(result).is_err() {
						break;
					}
					// Ignore presses during pairing; require a fresh stable release.
					button = Button::new(pin.is_low(), Instant::now());
				}
				thread::sleep(Duration::from_millis(20));
			}
		});
		Self {
			events,
			cancelled,
			worker: Some(worker),
		}
	}
}

impl Drop for PairButton {
	fn drop(&mut self) {
		self.cancelled.store(true, Ordering::SeqCst);
		if let Some(worker) = self.worker.take() {
			let _ = worker.join();
		}
	}
}

fn discovered_devices(output: &str) -> BTreeSet<String> {
	output
		.lines()
		.filter_map(|line| {
			let mut words = line.split_whitespace();
			words.find(|word| *word == "Device")?;
			crate::address(words.next()?).ok()
		})
		.collect()
}

fn is_headset(info: &str) -> bool {
	info.lines().any(|line| {
		line.trim().starts_with("UUID:")
			&& [
				"00001108-0000-1000-8000-00805f9b34fb",
				"0000111e-0000-1000-8000-00805f9b34fb",
				"00001131-0000-1000-8000-00805f9b34fb",
			]
			.iter()
			.any(|uuid| line.to_ascii_lowercase().contains(uuid))
	})
}

fn pair_headset(
	allowed: &BTreeSet<String>,
	mut execute: impl FnMut(&[&str], Duration) -> Result<String, String>,
	mut pair: impl FnMut(&str) -> Result<(), String>,
	mut rollback: impl FnMut(&str) -> Result<(), String>,
) -> Result<String, String> {
	let scan = execute(
		&["bluetoothctl", "--timeout", "15", "scan", "bredr"],
		Duration::from_secs(20),
	)?;
	let observed = discovered_devices(&scan);
	if observed.len() > 16 {
		return Err(
			"too many Bluetooth devices nearby; move away from other devices and try again".into(),
		);
	}
	let mut candidates = Vec::new();
	for device in observed.difference(allowed) {
		let info = execute(&["bluetoothctl", "info", device], COMMAND_TIMEOUT)?;
		if !device_flag(&info, "Paired") && is_headset(&info) {
			candidates.push(device.clone());
		}
	}
	let device = match candidates.as_slice() {
		[device] => device,
		[] => return Err("no unpaired HFP/HSP headset discovered; put one headset in pairing mode and press the button again".into()),
		_ => return Err("multiple unpaired headsets discovered; leave only the intended headset in pairing mode and try again".into()),
	};
	let enrollment = (|| {
		pair(device)?;
		let info = execute(&["bluetoothctl", "info", device], COMMAND_TIMEOUT)?;
		if !device_flag(&info, "Paired") {
			return Err(format!(
				"pairing did not succeed for {device}; PIN/confirmation headsets require interactive pairing"
			));
		}
		execute(
			&["bluetoothctl", "--timeout", "15", "trust", device],
			Duration::from_secs(20),
		)?;
		let info = execute(&["bluetoothctl", "info", device], COMMAND_TIMEOUT)?;
		if !device_flag(&info, "Trusted") {
			return Err(format!("trust did not succeed for {device}"));
		}
		Ok(())
	})();
	if let Err(error) = enrollment {
		return Err(rollback_error(device, error, &mut rollback));
	}
	// Retain a successfully paired headset even if its first connection fails.
	if let Err(error) = execute(
		&["bluetoothctl", "--timeout", "30", "connect", device],
		Duration::from_secs(35),
	) {
		log::warn!("Paired {device}, but initial connection failed: {error}");
	}
	Ok(device.clone())
}

fn rollback_headset(device: &str) -> Result<(), String> {
	// Only called for a device selected as unpaired by this attempt. Cleanup
	// must remain bounded but run even when the pairing operation was cancelled.
	crate::process::command(&["bluetoothctl", "remove", device], Duration::from_secs(20))
		.map(|_| ())
}

fn rollback_error(
	device: &str,
	error: String,
	mut rollback: impl FnMut(&str) -> Result<(), String>,
) -> String {
	match rollback(device) {
		Ok(()) => error,
		Err(cleanup) => format!(
			"{error}; could not remove incomplete pairing: {cleanup}; run bluetoothctl remove {device} before retrying"
		),
	}
}

// Positional bluetoothctl commands do not register their own agent. Keep an
// agent-enabled session alive and issue pairing only after registration.
fn headless_pair_session(args: &[&str], device: &str, stopped: &AtomicBool) -> Result<(), String> {
	if stopped.load(Ordering::SeqCst) {
		return Err("Bluetooth pairing cancelled".into());
	}
	let mut child = Command::new(args[0])
		.args(&args[1..])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.stderr(Stdio::null())
		.process_group(0)
		.spawn()
		.map_err(|error| format!("bluetoothctl pairing agent: {error}"))?;
	let mut input = child.stdin.take().expect("piped stdin");
	let mut output = child.stdout.take().expect("piped stdout");
	let (sender, messages) = mpsc::sync_channel(16);
	let reader = thread::spawn(move || {
		let mut buffer = [0; 1024];
		loop {
			match output.read(&mut buffer) {
				Ok(0) => break,
				Ok(size) => {
					if sender.send(Ok(buffer[..size].to_vec())).is_err() {
						break;
					}
				}
				Err(error) => {
					let _ = sender.send(Err(error));
					break;
				}
			}
		}
	});
	let result = (|| {
		let start = Instant::now();
		let mut requested = false;
		let mut pending = Vec::new();
		loop {
			if stopped.load(Ordering::SeqCst) {
				return Err("Bluetooth pairing cancelled".into());
			}
			let deadline = if requested { 75 } else { 15 };
			if start.elapsed() >= Duration::from_secs(deadline) {
				return Err("Bluetooth pairing timed out; PIN/confirmation headsets require interactive pairing".into());
			}
			match messages.recv_timeout(Duration::from_millis(20)) {
				Ok(Ok(chunk)) => {
					pending.extend(chunk);
					if pending.len() > 8192 {
						pending.drain(..pending.len() - 4096);
					}
					let text = String::from_utf8_lossy(&pending);
					if !requested && text.contains("Agent registered") {
						writeln!(input, "pair {device}").map_err(|error| error.to_string())?;
						requested = true;
						pending.clear();
					} else if requested && text.contains("Pairing successful") {
						return Ok(());
					} else if text.contains("Failed to pair")
						|| text.contains("Failed to register agent")
						|| text.contains("not available")
						|| text.contains("No default controller")
						|| text.contains("[agent]")
					{
						return Err("headless pairing failed; ensure Bluetooth is powered and the headset supports Just Works (no PIN/confirmation)".into());
					}
				}
				Ok(Err(error)) => return Err(format!("pairing agent output: {error}")),
				Err(mpsc::RecvTimeoutError::Disconnected) => {
					return Err("Bluetooth pairing agent exited before pairing succeeded".into());
				}
				Err(mpsc::RecvTimeoutError::Timeout) => {}
			}
		}
	})();
	drop(messages);
	drop(input);
	crate::process::kill_process_group(&mut child);
	let _ = child.wait();
	let _ = reader.join();
	result
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn debounce_requires_release_and_does_not_repeat() {
		let now = Instant::now();
		let mut button = Button::new(true, now);
		assert!(!button.sample(true, now + DEBOUNCE));
		assert!(!button.sample(false, now + DEBOUNCE));
		assert!(!button.sample(false, now + DEBOUNCE * 2));
		assert!(!button.sample(true, now + DEBOUNCE * 3));
		assert!(!button.sample(false, now + DEBOUNCE * 3 + Duration::from_millis(10)));
		assert!(!button.sample(true, now + DEBOUNCE * 4));
		assert!(button.sample(true, now + DEBOUNCE * 5));
		assert!(!button.sample(true, now + DEBOUNCE * 6));
		assert!(!button.sample(false, now + DEBOUNCE * 7));
		assert!(!button.sample(false, now + DEBOUNCE * 8));
		assert!(!button.sample(true, now + DEBOUNCE * 9));
		assert!(button.sample(true, now + DEBOUNCE * 10));
	}

	#[test]
	fn released_startup_arms_only_after_debounce() {
		let now = Instant::now();
		let mut button = Button::new(false, now);
		assert!(!button.sample(false, now));
		assert!(!button.sample(false, now + DEBOUNCE));
		assert!(!button.sample(true, now + DEBOUNCE + Duration::from_millis(1)));
		assert!(button.sample(true, now + DEBOUNCE * 2 + Duration::from_millis(1)));
	}

	#[test]
	fn parses_only_valid_observed_devices_and_headset_uuids() {
		assert_eq!(
			discovered_devices(
				"\x1b[0;92m[NEW]\x1b[0m Device aa:bb:cc:dd:ee:01 Headset\n\
				 [CHG] Device AA:BB:CC:DD:EE:01 RSSI: -40\n\
				 [NEW] Device invalid Keyboard\nController AA:BB:CC:DD:EE:02\n"
			),
			BTreeSet::from(["AA:BB:CC:DD:EE:01".into()])
		);
		assert!(is_headset(
			"UUID: Handsfree (0000111e-0000-1000-8000-00805f9b34fb)"
		));
		assert!(!is_headset(
			"UUID: Audio Sink (0000110b-0000-1000-8000-00805f9b34fb)"
		));
		assert!(!is_headset("Name: 0000111e-0000-1000-8000-00805f9b34fb"));
	}

	fn headset_info(paired: bool, trusted: bool) -> String {
		format!(
			"Paired: {}\nTrusted: {}\nUUID: Handsfree (0000111e-0000-1000-8000-00805f9b34fb)\n",
			if paired { "yes" } else { "no" },
			if trusted { "yes" } else { "no" },
		)
	}

	#[test]
	fn pairs_verifies_trusts_and_connects_with_noninteractive_agent() {
		let mut calls = Vec::new();
		let mut infos = 0;
		let mut paired = None;
		let result = pair_headset(
			&BTreeSet::new(),
			|args, timeout| {
				assert!(timeout <= Duration::from_secs(65));
				calls.push(args.join(" "));
				if args.contains(&"scan") {
					Ok("[NEW] Device AA:BB:CC:DD:EE:01 Headset".into())
				} else if args[1] == "info" {
					infos += 1;
					Ok(headset_info(infos > 1, infos > 2))
				} else {
					Ok(String::new())
				}
			},
			|device| {
				paired = Some(device.to_string());
				Ok(())
			},
			|_| panic!("successful pairing must not be rolled back"),
		)
		.unwrap();
		assert_eq!(result, "AA:BB:CC:DD:EE:01");
		assert_eq!(paired, Some(result));
		assert_eq!(calls.len(), 6);
		assert!(calls[3].contains("trust"));
		assert!(calls[5].contains("connect"));
	}

	#[test]
	fn refuses_ambiguous_already_paired_and_allowed_devices() {
		for (paired, allowed, expect_error) in [
			(false, BTreeSet::new(), "multiple"),
			(true, BTreeSet::new(), "no unpaired"),
			(
				false,
				BTreeSet::from(["AA:BB:CC:DD:EE:01".into(), "AA:BB:CC:DD:EE:02".into()]),
				"no unpaired",
			),
		] {
			let error = pair_headset(
				&allowed,
				|args, _| {
					if args.contains(&"scan") {
						Ok("Device AA:BB:CC:DD:EE:01 One\nDevice AA:BB:CC:DD:EE:02 Two\n".into())
					} else {
						assert_eq!(args[1], "info");
						Ok(headset_info(paired, false))
					}
				},
				|_| panic!("must not pair"),
				|_| panic!("must not remove existing or ambiguous devices"),
			)
			.unwrap_err();
			assert!(error.contains(expect_error), "{error}");
		}
	}

	#[test]
	fn stops_on_command_failure_or_unverified_pairing_and_trust() {
		for fail_at in 0..6 {
			let mut call = 0;
			let mut rolled_back = false;
			let result = pair_headset(
				&BTreeSet::new(),
				|args, _| {
					let current = call;
					call += 1;
					if current == fail_at {
						return Err("cancelled".into());
					}
					if args.contains(&"scan") {
						Ok("Device AA:BB:CC:DD:EE:01 Headset".into())
					} else {
						Ok(headset_info(current >= 2, current >= 4))
					}
				},
				|_| Ok(()),
				|device| {
					assert_eq!(device, "AA:BB:CC:DD:EE:01");
					rolled_back = true;
					Ok(())
				},
			);
			assert_eq!(call, fail_at + 1);
			assert_eq!(result.is_ok(), fail_at == 5);
			assert_eq!(rolled_back, (2..5).contains(&fail_at));
		}
		for fail_at in [2, 4] {
			let mut call = 0;
			assert!(
				pair_headset(
					&BTreeSet::new(),
					|_, _| {
						let current = call;
						call += 1;
						Ok(if current == 0 {
							"Device AA:BB:CC:DD:EE:01 Headset".into()
						} else {
							headset_info(current >= 2 && current != fail_at, false)
						})
					},
					|_| Ok(()),
					|_| Ok(()),
				)
				.is_err()
			);
			assert_eq!(call, fail_at + 1);
		}
	}

	#[test]
	fn failed_pairing_rolls_back_and_reports_cleanup_failures() {
		let mut calls = 0;
		let error = pair_headset(
			&BTreeSet::new(),
			|args, _| {
				calls += 1;
				if args.contains(&"scan") {
					Ok("Device AA:BB:CC:DD:EE:01 Headset".into())
				} else {
					Ok(headset_info(false, false))
				}
			},
			|_| Err("pairing cancelled".into()),
			|_| Err("BlueZ unavailable".into()),
		)
		.unwrap_err();
		assert_eq!(calls, 2);
		assert!(error.contains("pairing cancelled"));
		assert!(error.contains("BlueZ unavailable"));
		assert!(error.contains("bluetoothctl remove AA:BB:CC:DD:EE:01"));
	}

	#[test]
	fn agent_session_waits_for_registration_and_cleans_up() {
		let stopped = AtomicBool::new(false);
		let device = "AA:BB:CC:DD:EE:01";
		assert!(headless_pair_session(
			&["sh", "-c", "echo 'Agent registered'; read -r action address; test \"$action\" = pair && test \"$address\" = AA:BB:CC:DD:EE:01 && echo 'Pairing successful'; sleep 60"],
			device,
			&stopped,
		).is_ok());
		for output in [
			"Failed to register agent",
			"[agent] Confirm passkey",
			"Failed to pair",
			"No default controller available",
		] {
			assert!(headless_pair_session(&["printf", "%s\\n", output], device, &stopped).is_err());
		}
		stopped.store(true, Ordering::SeqCst);
		assert!(
			headless_pair_session(&["false"], device, &stopped)
				.unwrap_err()
				.contains("cancelled")
		);
	}

	#[test]
	fn cancellation_kills_agent_descendants_holding_output() {
		let stopped = Arc::new(AtomicBool::new(false));
		let flag = Arc::clone(&stopped);
		let cancel = thread::spawn(move || {
			thread::sleep(Duration::from_millis(100));
			flag.store(true, Ordering::SeqCst);
		});
		let start = Instant::now();
		assert!(
			headless_pair_session(
				&["sh", "-c", "sleep 60 & wait"],
				"AA:BB:CC:DD:EE:01",
				&stopped,
			)
			.unwrap_err()
			.contains("cancelled")
		);
		cancel.join().unwrap();
		assert!(start.elapsed() < Duration::from_secs(5));
	}

	#[test]
	fn detects_fragmented_registration_and_prompts_without_newlines() {
		let start = Instant::now();
		let error = headless_pair_session(
			&["sh", "-c", "printf 'Agent regi'; sleep 0.02; printf 'stered\\n'; read -r action address; printf '[agent] Confirm passkey'; sleep 60"],
			"AA:BB:CC:DD:EE:01",
			&AtomicBool::new(false),
		).unwrap_err();
		assert!(error.contains("headless pairing failed"));
		assert!(start.elapsed() < Duration::from_secs(5));
	}
}
