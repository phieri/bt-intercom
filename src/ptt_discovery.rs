// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Automatic discovery of the evdev device behind a headset's play/pause button.
//!
//! BlueZ exposes AVRCP buttons as a kernel input device whose `uniq` property
//! is the headset's Bluetooth address.

use std::fs;
use std::path::Path;

use crate::bluez::Address;
use crate::ptt::{PLAY_PAUSE_KEYS, PttButton};

/// Value accepted in place of an input device path to request discovery.
pub(crate) const AUTO: &str = "auto";

const SYS_INPUT: &str = "/sys/class/input";

/// Returns whether a `capabilities/key` bitmask contains `code`. The kernel
/// prints `unsigned long` words, so the width is that of the kernel, not of this
/// process (a 32-bit userland may run on a 64-bit kernel): non-leading words
/// are zero-padded to the full width.
fn has_key(mask: &str, code: u16) -> bool {
	let words: Vec<&str> = mask.split_whitespace().collect();
	let widest = words
		.iter()
		.skip(1)
		.map(|word| word.len())
		.max()
		.unwrap_or(0);
	let bits = match widest {
		16.. => 64,
		8.. => 32,
		_ => usize::BITS as usize,
	};
	let code = usize::from(code);
	words
		.len()
		.checked_sub(code / bits + 1)
		.and_then(|index| u64::from_str_radix(words[index], 16).ok())
		.is_some_and(|word| word >> (code % bits) & 1 == 1)
}

/// BlueZ names the AVRCP input device after the headset address and sets its
/// `uniq`; accept either so a missing `uniq` does not hide the device.
fn belongs_to(device: &Path, address: &Address) -> bool {
	let read = |file: &str| fs::read_to_string(device.join(file)).unwrap_or_default();
	let wanted = address.as_str();
	read("uniq").trim().eq_ignore_ascii_case(wanted)
		|| read("name")
			.trim()
			.get(..wanted.len())
			.is_some_and(|prefix| prefix.eq_ignore_ascii_case(wanted))
}

/// Finds the event device that reports play/pause for `address` under `root`.
/// `Ok(None)` means none exists yet, e.g. the headset is not connected.
fn find_in(root: &Path, address: &Address) -> Result<Option<String>, String> {
	let mut matches = vec![];
	let entries = fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))?;
	for entry in entries.flatten() {
		let name = entry.file_name().to_string_lossy().into_owned();
		if !name.starts_with("event") {
			continue;
		}
		let device = entry.path().join("device");
		if !belongs_to(&device, address) {
			continue;
		}
		let keys = fs::read_to_string(device.join("capabilities/key")).unwrap_or_default();
		if PLAY_PAUSE_KEYS.iter().any(|&code| has_key(&keys, code)) {
			matches.push(name);
		}
	}
	matches.sort();
	match matches.as_slice() {
		[name] => Ok(Some(format!("/dev/input/{name}"))),
		[] => Ok(None),
		_ => Err(format!(
			"several play/pause input devices match {address} ({}); pass --ptt {address}=/dev/input/eventX",
			matches.join(", ")
		)),
	}
}

fn missing(address: &Address) -> String {
	format!(
		"no play/pause input device found for {address}; connect the headset (its AVRCP input device appears only while connected) or pass --ptt {address}=/dev/input/eventX"
	)
}

pub(crate) fn find(address: &Address) -> Result<String, String> {
	find_in(Path::new(SYS_INPUT), address)?.ok_or_else(|| missing(address))
}

/// Replaces `auto` mappings with discovered input paths.
///
/// A bare `--ptt auto` expands to every listed headset. A headset without an
/// input device yet (typically still connecting) keeps `auto`; its reader
/// attaches once the device appears. Ambiguous matches remain errors.
pub(crate) fn resolve_pending(
	buttons: &mut Vec<PttButton>,
	allowed: &std::collections::BTreeSet<Address>,
) -> Result<(), String> {
	resolve_at(Path::new(SYS_INPUT), buttons, allowed)
}

fn resolve_at(
	root: &Path,
	buttons: &mut Vec<PttButton>,
	allowed: &std::collections::BTreeSet<Address>,
) -> Result<(), String> {
	if let Some(position) = buttons.iter().position(|button| button.address.is_none()) {
		if buttons.len() > 1 {
			return Err("--ptt auto cannot be combined with other --ptt mappings".into());
		}
		buttons.remove(position);
		buttons.extend(allowed.iter().map(|address| PttButton {
			address: Some(address.clone()),
			path: AUTO.into(),
		}));
	}
	for button in buttons.iter_mut().filter(|button| button.path == AUTO) {
		let Some(address) = &button.address else {
			return Err("--ptt auto must be resolved to a headset address".into());
		};
		match find_in(root, address)? {
			Some(path) => button.path = path,
			None => log::warn!("{}; waiting for it to appear", missing(address)),
		}
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::BTreeSet;

	fn address(value: &str) -> Address {
		Address::parse(value).unwrap()
	}

	fn device(root: &Path, name: &str, uniq: &str, keys: &str) {
		let dir = root.join(name).join("device");
		fs::create_dir_all(dir.join("capabilities")).unwrap();
		fs::write(dir.join("uniq"), format!("{uniq}\n")).unwrap();
		fs::write(dir.join("capabilities/key"), format!("{keys}\n")).unwrap();
	}

	fn temp(name: &str) -> std::path::PathBuf {
		let root = std::env::temp_dir().join(format!("bt-intercom-{name}-{}", std::process::id()));
		let _ = fs::remove_dir_all(&root);
		fs::create_dir_all(&root).unwrap();
		root
	}

	#[test]
	fn detects_play_pause_bit_for_either_word_size() {
		let bit = 164 % usize::BITS as usize;
		let word = format!("{:x}", 1usize << bit);
		let zeros = "0 ".repeat(164 / usize::BITS as usize);
		assert!(has_key(format!("{word} {zeros}").trim(), 164));
		assert!(!has_key("0 0 0", 164));
		assert!(!has_key("", 164));
	}

	#[test]
	fn detects_key_in_32_and_64_bit_kernel_words() {
		let wide = "0 0000001000000000 0000000000000000 0000000000000000";
		assert!(has_key(wide, 164));
		assert!(!has_key(wide, 165));
		let narrow = "0 00000010 00000000 00000000 00000000 00000000 00000000";
		assert!(has_key(narrow, 164));
		assert!(!has_key(narrow, 163));
	}

	#[test]
	fn falls_back_to_device_name_when_uniq_is_missing() {
		let root = temp("ptt-name");
		device(&root, "event3", "", "0");
		let dir = root.join("event3/device");
		fs::write(dir.join("name"), "AA:BB:CC:DD:EE:01 (AVRCP)\n").unwrap();
		assert!(belongs_to(&dir, &address("aa:bb:cc:dd:ee:01")));
		assert!(!belongs_to(&dir, &address("aa:bb:cc:dd:ee:02")));
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn finds_device_by_address_and_capability() {
		let root = temp("ptt-find");
		let bit = 164 % usize::BITS as usize;
		let word = format!("{:x}", 1usize << bit);
		let keys = format!("{word} {}", "0 ".repeat(164 / usize::BITS as usize))
			.trim()
			.to_string();
		device(&root, "event3", "aa:bb:cc:dd:ee:01", &keys);
		device(&root, "event4", "aa:bb:cc:dd:ee:02", "0");
		device(&root, "event5", "aa:bb:cc:dd:ee:02", &keys);
		assert_eq!(
			find_in(&root, &address("AA:BB:CC:DD:EE:01")).unwrap(),
			Some("/dev/input/event3".into())
		);
		assert_eq!(
			find_in(&root, &address("AA:BB:CC:DD:EE:02")).unwrap(),
			Some("/dev/input/event5".into())
		);
		assert_eq!(find_in(&root, &address("AA:BB:CC:DD:EE:03")).unwrap(), None);
		device(&root, "event6", "aa:bb:cc:dd:ee:01", &keys);
		assert!(find_in(&root, &address("AA:BB:CC:DD:EE:01")).is_err());
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn missing_device_defers_instead_of_failing() {
		let root = temp("ptt-defer");
		let allowed = BTreeSet::from([address("AA:BB:CC:DD:EE:01")]);
		let mut buttons = vec![PttButton {
			address: None,
			path: AUTO.into(),
		}];
		resolve_at(&root, &mut buttons, &allowed).unwrap();
		assert_eq!(buttons[0].path, AUTO);
		fs::remove_dir_all(root).unwrap();
	}

	#[test]
	fn bare_auto_expands_to_network() {
		let root = temp("ptt-expand");
		let bit = 164 % usize::BITS as usize;
		let word = format!("{:x}", 1usize << bit);
		let keys = format!("{word} {}", "0 ".repeat(164 / usize::BITS as usize))
			.trim()
			.to_string();
		device(&root, "event1", "aa:bb:cc:dd:ee:01", &keys);
		device(&root, "event2", "aa:bb:cc:dd:ee:02", &keys);
		let allowed = BTreeSet::from([address("AA:BB:CC:DD:EE:01"), address("AA:BB:CC:DD:EE:02")]);
		let mut buttons = vec![PttButton {
			address: None,
			path: AUTO.into(),
		}];
		resolve_at(&root, &mut buttons, &allowed).unwrap();
		assert_eq!(buttons.len(), 2);
		assert_eq!(buttons[1].path, "/dev/input/event2");
		fs::remove_dir_all(root).unwrap();
	}
}
