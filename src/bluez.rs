// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Helpers for parsing BlueZ device information.

pub(crate) const STRONG_RSSI: i32 = -60;
pub(crate) const GOOD_RSSI: i32 = -70;
pub(crate) const FAIR_RSSI: i32 = -80;

pub(crate) fn property<'a>(info: &'a str, field: &str) -> Option<&'a str> {
	info.lines().find_map(|line| {
		let (key, value) = line.trim().split_once(':')?;
		(key.trim() == field).then_some(value.trim())
	})
}

pub(crate) fn device_flag(info: &str, flag: &str) -> bool {
	property(info, flag) == Some("yes")
}

pub(crate) fn normalize_address(value: &str) -> Option<String> {
	let bytes = value.as_bytes();
	if bytes.len() != 17
		|| !bytes.iter().enumerate().all(|(index, byte)| {
			if index % 3 == 2 {
				*byte == b':'
			} else {
				byte.is_ascii_hexdigit()
			}
		}) {
		return None;
	}
	Some(value.to_ascii_uppercase())
}

pub(crate) fn controller_addresses(info: &str) -> std::collections::BTreeSet<String> {
	info.lines()
		.filter_map(|line| {
			let address = line
				.trim()
				.strip_prefix("Controller ")?
				.split_whitespace()
				.next()?;
			normalize_address(address)
		})
		.collect()
}

pub(crate) fn signal_strength(info: &str) -> Option<i32> {
	property(info, "RSSI").and_then(|value| value.split_whitespace().next()?.parse().ok())
}

pub(crate) fn bluetooth_name(info: &str) -> Option<String> {
	["Alias", "Name"].into_iter().find_map(|field| {
		let name: String = property(info, field)?
			.chars()
			.filter(|character| !character.is_control())
			.collect();
		let name = name.trim();
		(!name.is_empty()).then(|| name.to_owned())
	})
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn parses_properties_and_flags() {
		for (info, field, expected) in [
			("  Connected : yes  \n", "Connected", Some("yes")),
			("Name: device: one\n", "Name", Some("device: one")),
			("Connected: no\nConnected: yes\n", "Connected", Some("no")),
			("Not Connected: yes\n", "Connected", None),
		] {
			assert_eq!(property(info, field), expected);
		}
		for (info, flag, expected) in [
			("Paired: yes\n", "Paired", true),
			("Paired: no\n", "Paired", false),
			("Paired: YES\n", "Paired", false),
			("Not Paired: yes\n", "Paired", false),
		] {
			assert_eq!(device_flag(info, flag), expected);
		}
	}

	#[test]
	fn controller_list_uses_addresses_not_aliases_or_default_markers() {
		assert_eq!(
			controller_addresses(
				"Controller aa:bb:cc:dd:ee:01 Built-in [default]\n\
				 Controller AA:BB:CC:DD:EE:02 USB radio\n\
				 Controller AA:BB:CC:DD:EE:02 duplicate\n\
				 Controller invalid alias\nDevice AA:BB:CC:DD:EE:03 headset\n"
			),
			["AA:BB:CC:DD:EE:01".into(), "AA:BB:CC:DD:EE:02".into()].into()
		);
	}

	#[test]
	fn chooses_a_safe_nonempty_bluetooth_name() {
		for (info, expected) in [
			("Name: Device name\nAlias: User name\n", Some("User name")),
			("Alias: \nName: Device name\n", Some("Device name")),
			("Alias: Unsafe\u{1b}[31m name", Some("Unsafe[31m name")),
			("Alias: \nName: \n", None),
			("Alias: \u{7f}\nName: \u{1}Fallback\n", Some("Fallback")),
		] {
			assert_eq!(bluetooth_name(info).as_deref(), expected);
		}
	}

	#[test]
	fn normalizes_only_valid_bluetooth_addresses() {
		assert_eq!(
			normalize_address("aa:bb:cc:dd:ee:ff").as_deref(),
			Some("AA:BB:CC:DD:EE:FF")
		);
		for value in [
			"",
			"AA:BB:CC:DD:EE",
			"AA:BB:CC:DD:EE:FG",
			"AA-BB-CC-DD-EE-FF",
			"AA:BB:CC:DD:EE:FF ",
		] {
			assert_eq!(normalize_address(value), None, "{value:?}");
		}
	}

	#[test]
	fn parses_optional_rssi() {
		assert_eq!(signal_strength("RSSI: -67 (0xffffffbd)\n"), Some(-67));
		assert_eq!(signal_strength("RSSI: unavailable\n"), None);
		assert_eq!(signal_strength("Connected: yes\n"), None);
	}
}
