// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Helpers for parsing BlueZ device information.

use std::borrow::Borrow;
use std::fmt;
use std::str::FromStr;

pub(crate) const STRONG_RSSI: i32 = -60;
pub(crate) const GOOD_RSSI: i32 = -70;
pub(crate) const FAIR_RSSI: i32 = -80;

#[derive(Clone, Debug, Eq, Hash, Ord, PartialEq, PartialOrd)]
pub(crate) struct Address(String);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) enum AddressErrorKind {
	Length,
	Separator { index: usize },
	HexDigit { index: usize },
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AddressError {
	pub(crate) input: String,
	pub(crate) kind: AddressErrorKind,
}

impl Address {
	pub(crate) fn parse(value: &str) -> Result<Self, AddressError> {
		let bytes = value.as_bytes();
		if bytes.len() != 17 {
			return Err(AddressError {
				input: value.into(),
				kind: AddressErrorKind::Length,
			});
		}
		for (index, byte) in bytes.iter().enumerate() {
			if index % 3 == 2 {
				if *byte != b':' {
					return Err(AddressError {
						input: value.into(),
						kind: AddressErrorKind::Separator { index },
					});
				}
			} else if !byte.is_ascii_hexdigit() {
				return Err(AddressError {
					input: value.into(),
					kind: AddressErrorKind::HexDigit { index },
				});
			}
		}
		Ok(Self(value.to_ascii_uppercase()))
	}

	pub(crate) fn as_str(&self) -> &str {
		&self.0
	}
}

impl Borrow<str> for Address {
	fn borrow(&self) -> &str {
		self.as_str()
	}
}

impl AsRef<str> for Address {
	fn as_ref(&self) -> &str {
		self.as_str()
	}
}

impl std::ops::Deref for Address {
	type Target = str;

	fn deref(&self) -> &Self::Target {
		self.as_str()
	}
}

impl fmt::Display for Address {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		formatter.write_str(self.as_str())
	}
}

impl fmt::Display for AddressError {
	fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
		let reason = match self.kind {
			AddressErrorKind::Length => "must contain six hexadecimal octets",
			AddressErrorKind::Separator { .. } => "must separate octets with colons",
			AddressErrorKind::HexDigit { .. } => "contains a non-hexadecimal character",
		};
		write!(
			formatter,
			"expected a Bluetooth MAC address (XX:XX:XX:XX:XX:XX): {} ({reason})",
			self.input
		)
	}
}

impl std::error::Error for AddressError {}

impl FromStr for Address {
	type Err = AddressError;

	fn from_str(value: &str) -> Result<Self, Self::Err> {
		Self::parse(value)
	}
}

#[cfg(test)]
impl From<&str> for Address {
	fn from(value: &str) -> Self {
		Self::parse(value).expect("test fixture must use a valid Bluetooth address")
	}
}

#[cfg(test)]
impl From<String> for Address {
	fn from(value: String) -> Self {
		Self::parse(&value).expect("test fixture must use a valid Bluetooth address")
	}
}

pub(crate) fn property<'a>(info: &'a str, field: &str) -> Option<&'a str> {
	info.lines().find_map(|line| {
		let (key, value) = line.trim().split_once(':')?;
		(key.trim() == field).then_some(value.trim())
	})
}

pub(crate) fn device_flag(info: &str, flag: &str) -> bool {
	property(info, flag) == Some("yes")
}

pub(crate) fn normalize_address(value: &str) -> Option<Address> {
	Address::parse(value).ok()
}

pub(crate) fn parse_address(value: &str) -> Result<Address, AddressError> {
	Address::parse(value)
}

pub(crate) fn controller_addresses(info: &str) -> std::collections::BTreeSet<Address> {
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
			normalize_address("aa:bb:cc:dd:ee:ff")
				.as_ref()
				.map(Address::as_str),
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
	fn address_errors_identify_the_invalid_component() {
		assert_eq!(
			Address::parse("AA:BB:CC:DD:EE").unwrap_err().kind,
			AddressErrorKind::Length
		);
		assert_eq!(
			Address::parse("AA-BB:CC:DD:EE:FF").unwrap_err().kind,
			AddressErrorKind::Separator { index: 2 }
		);
		let error = Address::parse("AA:BB:CC:DD:EE:FG").unwrap_err();
		assert_eq!(error.kind, AddressErrorKind::HexDigit { index: 16 });
		assert!(error.to_string().contains("non-hexadecimal character"));
	}

	#[test]
	fn parses_optional_rssi() {
		assert_eq!(signal_strength("RSSI: -67 (0xffffffbd)\n"), Some(-67));
		assert_eq!(signal_strength("RSSI: unavailable\n"), None);
		assert_eq!(signal_strength("Connected: yes\n"), None);
	}
}
