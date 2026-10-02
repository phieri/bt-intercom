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
	fn parses_device_properties_and_flags() {
		assert_eq!(property("  Connected : yes  \n", "Connected"), Some("yes"));
		assert_eq!(property("Not Connected: yes\n", "Connected"), None);
		assert!(device_flag("Paired: yes\n", "Paired"));
		assert!(!device_flag("Paired: no\n", "Paired"));
	}

	#[test]
	fn prefers_alias_and_ignores_control_characters() {
		assert_eq!(
			bluetooth_name("Name: Device name\nAlias: User name\n"),
			Some("User name".into())
		);
		assert_eq!(
			bluetooth_name("Alias: \nName: Device name\n"),
			Some("Device name".into())
		);
		assert_eq!(
			bluetooth_name("Alias: Unsafe\u{1b}[31m name"),
			Some("Unsafe[31m name".into())
		);
		assert_eq!(bluetooth_name("Alias: \nName: \n"), None);
	}
}
