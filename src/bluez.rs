// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Helpers for parsing BlueZ device information.

pub(crate) fn property<'a>(info: &'a str, field: &str) -> Option<&'a str> {
	info.lines().find_map(|line| {
		let (key, value) = line.trim().split_once(':')?;
		(key.trim() == field).then_some(value.trim())
	})
}

pub(crate) fn device_flag(info: &str, flag: &str) -> bool {
	property(info, flag) == Some("yes")
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
	fn chooses_a_safe_nonempty_bluetooth_name() {
		for (info, expected) in [
			(
				"Name: Device name\nAlias: User name\n",
				Some("User name"),
			),
			("Alias: \nName: Device name\n", Some("Device name")),
			("Alias: Unsafe\u{1b}[31m name", Some("Unsafe[31m name")),
			("Alias: \nName: \n", None),
			("Alias: \u{7f}\nName: \u{1}Fallback\n", Some("Fallback")),
		] {
			assert_eq!(bluetooth_name(info).as_deref(), expected);
		}
	}
}
