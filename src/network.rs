// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Persistent allowlist for the configured headset network.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

use crate::bluez::Address;

pub(crate) fn headset_path() -> Result<PathBuf, String> {
	let directory = env::var_os("XDG_CONFIG_HOME")
		.map(PathBuf::from)
		.filter(|path| path.is_absolute())
		.or_else(|| env::var_os("HOME").map(|home| PathBuf::from(home).join(".config")))
		.ok_or("could not determine the config directory; set XDG_CONFIG_HOME or HOME")?;
	Ok(directory.join("bt-intercom").join("headsets"))
}

pub(crate) fn runtime_directory(xdg_runtime_dir: Option<&OsStr>) -> PathBuf {
	xdg_runtime_dir
		.map(PathBuf::from)
		.filter(|path| path.is_absolute())
		.unwrap_or_else(env::temp_dir)
}

pub(crate) fn save(path: &Path, allowed: &BTreeSet<Address>) -> Result<(), String> {
	if allowed.is_empty() {
		return Err("cannot save an empty headset network".into());
	}
	let directory = path
		.parent()
		.ok_or_else(|| format!("invalid headset network path: {}", path.display()))?;
	fs::create_dir_all(directory)
		.map_err(|error| format!("could not create {}: {error}", directory.display()))?;

	let contents = format!(
		"{}\n",
		allowed
			.iter()
			.map(Address::as_str)
			.collect::<Vec<_>>()
			.join("\n")
	);
	crate::atomic_file::write(path, contents.as_bytes()).map_err(|error| {
		format!(
			"could not save headset network to {}: {error}",
			path.display()
		)
	})?;
	Ok(())
}

pub(crate) fn remove(path: &Path, address: &Address) -> Result<(), String> {
	let mut allowed = load(path)?;
	if !allowed.remove(address) {
		return Err(format!("{address} is not in the saved headset network"));
	}
	if allowed.is_empty() {
		fs::remove_file(path).map_err(|error| {
			format!(
				"could not remove saved headset network {}: {error}",
				path.display()
			)
		})?;
	} else {
		save(path, &allowed)?;
	}
	Ok(())
}

pub(crate) fn load(path: &Path) -> Result<BTreeSet<Address>, String> {
	let contents = fs::read_to_string(path).map_err(|error| {
		if error.kind() == std::io::ErrorKind::NotFound {
			format!(
				"no saved headset network at {}; run with headset addresses first",
				path.display()
			)
		} else {
			format!(
				"could not read headset network from {}: {error}",
				path.display()
			)
		}
	})?;
	let mut allowed = BTreeSet::new();
	for (index, line) in contents.lines().enumerate() {
		let line = line.trim();
		if !line.is_empty() {
			allowed.insert(crate::bluez::parse_address(line).map_err(|error| {
				format!(
					"invalid headset network at {}:{}: {error}",
					path.display(),
					index + 1
				)
			})?);
		}
	}
	if allowed.is_empty() {
		return Err(format!(
			"saved headset network at {} is empty",
			path.display()
		));
	}
	Ok(allowed)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::{SystemTime, UNIX_EPOCH};

	fn address(value: &str) -> Address {
		Address::parse(value).unwrap()
	}

	fn temporary_network_path() -> PathBuf {
		let nonce = SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.unwrap()
			.as_nanos();
		env::temp_dir().join(format!(
			"bt-intercom-headsets-{}-{nonce}/headsets",
			std::process::id()
		))
	}

	#[test]
	fn uses_absolute_xdg_runtime_dir_for_ptt_beeps() {
		assert_eq!(
			runtime_directory(Some(OsStr::new("/run/user/1000"))),
			PathBuf::from("/run/user/1000")
		);
		assert_eq!(
			runtime_directory(Some(OsStr::new("relative"))),
			env::temp_dir()
		);
		assert_eq!(runtime_directory(None), env::temp_dir());
	}

	#[test]
	fn saves_and_restores_headset_network() {
		let path = temporary_network_path();
		let allowed = BTreeSet::from([address("AA:BB:CC:DD:EE:02"), address("AA:BB:CC:DD:EE:01")]);
		save(&path, &allowed).unwrap();
		assert_eq!(load(&path).unwrap(), allowed);
		assert_eq!(
			fs::read_to_string(&path).unwrap(),
			"AA:BB:CC:DD:EE:01\nAA:BB:CC:DD:EE:02\n"
		);
		fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	#[test]
	fn removes_headsets_from_saved_network() {
		let path = temporary_network_path();
		let allowed = BTreeSet::from([address("AA:BB:CC:DD:EE:01"), address("AA:BB:CC:DD:EE:02")]);
		save(&path, &allowed).unwrap();

		remove(&path, &address("AA:BB:CC:DD:EE:01")).unwrap();

		assert_eq!(
			load(&path).unwrap(),
			BTreeSet::from([address("AA:BB:CC:DD:EE:02")])
		);
		fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	#[test]
	fn removes_last_headset_and_rejects_unknown_address_without_changes() {
		let path = temporary_network_path();
		let allowed = BTreeSet::from([address("AA:BB:CC:DD:EE:01")]);
		save(&path, &allowed).unwrap();

		assert!(remove(&path, &address("AA:BB:CC:DD:EE:02")).is_err());
		assert_eq!(load(&path).unwrap(), allowed);
		remove(&path, &address("AA:BB:CC:DD:EE:01")).unwrap();
		assert!(!path.exists());
		assert!(load(&path).is_err());
		fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	#[test]
	fn loads_normalized_addresses_and_rejects_invalid_network_files() {
		let path = temporary_network_path();
		fs::create_dir_all(path.parent().unwrap()).unwrap();
		fs::write(&path, "\naa:bb:cc:dd:ee:01\nAA:BB:CC:DD:EE:01\n \n").unwrap();
		assert_eq!(
			load(&path).unwrap(),
			BTreeSet::from([address("AA:BB:CC:DD:EE:01")])
		);
		fs::write(&path, "not-a-bluetooth-address\n").unwrap();
		assert!(load(&path).is_err());
		fs::write(&path, "\n \n").unwrap();
		assert!(load(&path).is_err());
		fs::remove_dir_all(path.parent().unwrap()).unwrap();
	}

	#[test]
	fn missing_saved_network_explains_how_to_create_it() {
		let error = load(&temporary_network_path()).unwrap_err();
		assert!(error.contains("run with headset addresses first"));
	}
}
