// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Persistent allowlist for the configured headset network.

use std::collections::BTreeSet;
use std::env;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};

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

pub(crate) fn save(path: &Path, allowed: &BTreeSet<String>) -> Result<(), String> {
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
		allowed.iter().cloned().collect::<Vec<_>>().join("\n")
	);
	crate::atomic_file::write(path, contents.as_bytes()).map_err(|error| {
		format!(
			"could not save headset network to {}: {error}",
			path.display()
		)
	})?;
	Ok(())
}

pub(crate) fn remove(path: &Path, address: &str) -> Result<(), String> {
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

pub(crate) fn load(path: &Path) -> Result<BTreeSet<String>, String> {
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
			allowed.insert(
				crate::bluez::normalize_address(line).ok_or_else(|| {
					format!(
						"invalid headset network at {}:{}: expected a Bluetooth MAC address (XX:XX:XX:XX:XX:XX): {line}",
						path.display(),
						index + 1
					)
				})?,
			);
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

pub(crate) fn load_for_run(path: &Path, pair_button: bool) -> Result<BTreeSet<String>, String> {
	if pair_button && !path.try_exists().map_err(|error| error.to_string())? {
		Ok(BTreeSet::new())
	} else {
		load(path)
	}
}

pub(crate) fn enroll(
	path: &Path,
	allowed: &mut BTreeSet<String>,
	device: String,
) -> Result<(), String> {
	let mut enrolled = allowed.clone();
	enrolled.insert(device);
	save(path, &enrolled)?;
	*allowed = enrolled;
	Ok(())
}
