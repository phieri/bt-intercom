// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Automatic discovery of the evdev device behind a headset's play/pause button.
//!
//! BlueZ exposes AVRCP buttons as a kernel input device whose `uniq` property
//! is the headset's Bluetooth address.

use std::fs;
use std::path::Path;

use crate::{KEY_PLAYPAUSE, PttButton};

/// Value accepted in place of an input device path to request discovery.
pub(crate) const AUTO: &str = "auto";

const SYS_INPUT: &str = "/sys/class/input";

/// Returns whether a `capabilities/key` bitmask contains `code`.
fn has_key(mask: &str, code: u16) -> bool {
	let bits = usize::BITS as usize;
	let words: Vec<&str> = mask.split_whitespace().collect();
	let code = usize::from(code);
	words
		.len()
		.checked_sub(code / bits + 1)
		.and_then(|index| usize::from_str_radix(words[index], 16).ok())
		.is_some_and(|word| word >> (code % bits) & 1 == 1)
}

/// Finds the event device that reports play/pause for `address` under `root`.
fn find_in(root: &Path, address: &str) -> Result<String, String> {
	let mut matches = vec![];
	let entries = fs::read_dir(root).map_err(|error| format!("{}: {error}", root.display()))?;
	for entry in entries.flatten() {
		let name = entry.file_name().to_string_lossy().into_owned();
		if !name.starts_with("event") {
			continue;
		}
		let device = entry.path().join("device");
		let uniq = fs::read_to_string(device.join("uniq")).unwrap_or_default();
		if !uniq.trim().eq_ignore_ascii_case(address) {
			continue;
		}
		let keys = fs::read_to_string(device.join("capabilities/key")).unwrap_or_default();
		if has_key(&keys, KEY_PLAYPAUSE) {
			matches.push(name);
		}
	}
	matches.sort();
	match matches.as_slice() {
		[name] => Ok(format!("/dev/input/{name}")),
		[] => Err(format!(
			"no play/pause input device found for {address}; connect the headset first or pass --ptt {address}=/dev/input/eventX"
		)),
		_ => Err(format!(
			"several play/pause input devices match {address} ({}); pass --ptt {address}=/dev/input/eventX",
			matches.join(", ")
		)),
	}
}

/// Replaces `auto` mappings with discovered input paths.
///
/// A bare `--ptt auto` expands to every listed headset.
pub(crate) fn resolve_pending(
	buttons: &mut Vec<PttButton>,
	allowed: &std::collections::BTreeSet<String>,
) -> Result<(), String> {
	resolve_at(Path::new(SYS_INPUT), buttons, allowed)
}

fn resolve_at(
	root: &Path,
	buttons: &mut Vec<PttButton>,
	allowed: &std::collections::BTreeSet<String>,
) -> Result<(), String> {
	if let Some(position) = buttons.iter().position(|b| b.address == AUTO) {
		if buttons.len() > 1 {
			return Err("--ptt auto cannot be combined with other --ptt mappings".into());
		}
		buttons.remove(position);
		buttons.extend(allowed.iter().map(|address| PttButton {
			address: address.clone(),
			path: AUTO.into(),
		}));
	}
	for button in buttons.iter_mut().filter(|b| b.path == AUTO) {
		button.path = find_in(root, &button.address)?;
	}
	Ok(())
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::collections::BTreeSet;

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
			find_in(&root, "AA:BB:CC:DD:EE:01").unwrap(),
			"/dev/input/event3"
		);
		assert_eq!(
			find_in(&root, "AA:BB:CC:DD:EE:02").unwrap(),
			"/dev/input/event5"
		);
		assert!(find_in(&root, "AA:BB:CC:DD:EE:03").is_err());
		device(&root, "event6", "aa:bb:cc:dd:ee:01", &keys);
		assert!(find_in(&root, "AA:BB:CC:DD:EE:01").is_err());
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
		let allowed = BTreeSet::from(["AA:BB:CC:DD:EE:01".to_string(), "AA:BB:CC:DD:EE:02".into()]);
		let mut buttons = vec![PttButton {
			address: AUTO.into(),
			path: AUTO.into(),
		}];
		resolve_at(&root, &mut buttons, &allowed).unwrap();
		assert_eq!(buttons.len(), 2);
		assert_eq!(buttons[1].path, "/dev/input/event2");
		fs::remove_dir_all(root).unwrap();
	}
}
