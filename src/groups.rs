// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Persistent talk-group configuration.

use std::collections::BTreeSet;
use std::fs::{self, File};
use std::io::Write;
use std::path::{Path, PathBuf};

use serde_json::json;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TalkGroup {
	pub name: String,
	pub members: BTreeSet<String>,
}

pub fn config_path(network_path: &Path) -> Result<PathBuf, String> {
	network_path
		.parent()
		.map(|directory| directory.join("talk-groups.json"))
		.ok_or_else(|| format!("invalid headset network path: {}", network_path.display()))
}

pub fn load(path: &Path) -> Result<Vec<TalkGroup>, String> {
	let contents = match fs::read_to_string(path) {
		Ok(contents) => contents,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
		Err(error) => return Err(format!("could not read {}: {error}", path.display())),
	};
	let value: serde_json::Value = serde_json::from_str(&contents)
		.map_err(|error| format!("invalid talk-group file {}: {error}", path.display()))?;
	let entries = value["groups"].as_array().ok_or_else(|| {
		format!(
			"invalid talk-group file {}: missing groups array",
			path.display()
		)
	})?;
	let mut groups = Vec::with_capacity(entries.len());
	let mut names = BTreeSet::new();
	for entry in entries {
		let name = entry["name"]
			.as_str()
			.map(str::trim)
			.filter(|name| !name.is_empty())
			.ok_or_else(|| {
				format!(
					"invalid talk-group file {}: group name is empty",
					path.display()
				)
			})?
			.to_string();
		if !names.insert(normalize_name(&name)) {
			return Err(format!(
				"invalid talk-group file {}: duplicate group name",
				path.display()
			));
		}
		let members = entry["members"]
			.as_array()
			.ok_or_else(|| {
				format!(
					"invalid talk-group file {}: group members must be an array",
					path.display()
				)
			})?
			.iter()
			.map(|member| {
				let address = member.as_str().ok_or_else(|| {
					format!(
						"invalid talk-group file {}: member address must be a string",
						path.display()
					)
				})?;
				validate_address(address).ok_or_else(|| {
					format!(
						"invalid talk-group file {}: invalid headset address {address}",
						path.display()
					)
				})
			})
			.collect::<Result<BTreeSet<_>, _>>()?;
		groups.push(TalkGroup { name, members });
	}
	Ok(groups)
}

pub fn save(path: &Path, groups: &[TalkGroup]) -> Result<(), String> {
	let mut names = BTreeSet::new();
	for group in groups {
		if group.name.trim().is_empty() || !names.insert(normalize_name(&group.name)) {
			return Err("talk-group names must be non-empty and unique".into());
		}
		if group
			.members
			.iter()
			.any(|member| validate_address(member).is_none())
		{
			return Err(format!(
				"talk group {:?} contains an invalid headset address",
				group.name
			));
		}
	}
	let directory = path
		.parent()
		.ok_or_else(|| format!("invalid talk-group path: {}", path.display()))?;
	fs::create_dir_all(directory)
		.map_err(|error| format!("could not create {}: {error}", directory.display()))?;
	let contents = json!({
		"groups": groups.iter().map(|group| json!({
			"name": group.name.trim(),
			"members": group.members.iter().filter_map(|address| validate_address(address)).collect::<Vec<_>>(),
		})).collect::<Vec<_>>(),
	})
	.to_string();
	let mut temporary = path.as_os_str().to_os_string();
	temporary.push(format!(".{}.tmp", std::process::id()));
	let temporary = PathBuf::from(temporary);
	let result = (|| {
		let mut file = File::create(&temporary)?;
		file.write_all(contents.as_bytes())?;
		file.sync_all()?;
		fs::rename(&temporary, path)
	})();
	if let Err(error) = result {
		let _ = fs::remove_file(&temporary);
		return Err(format!(
			"could not save talk groups to {}: {error}",
			path.display()
		));
	}
	Ok(())
}

pub fn normalize_name(name: &str) -> String {
	name.trim().to_lowercase()
}

fn validate_address(address: &str) -> Option<String> {
	let bytes = address.as_bytes();
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
	Some(address.to_ascii_uppercase())
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn saves_and_loads_normalized_talk_groups() {
		let path = std::env::temp_dir().join(format!(
			"rpi-intercom-groups-{}-{}.json",
			std::process::id(),
			std::thread::current().name().unwrap_or("test")
		));
		let groups = vec![TalkGroup {
			name: "Team".into(),
			members: ["aa:bb:cc:dd:ee:01".into()].into(),
		}];
		save(&path, &groups).unwrap();
		assert_eq!(
			load(&path).unwrap(),
			[TalkGroup {
				name: "Team".into(),
				members: ["AA:BB:CC:DD:EE:01".into()].into(),
			}]
		);
		fs::remove_file(path).unwrap();
	}

	#[test]
	fn missing_group_file_preserves_legacy_network_behavior() {
		let path = std::env::temp_dir().join(format!(
			"rpi-intercom-missing-groups-{}.json",
			std::process::id()
		));
		let _ = fs::remove_file(&path);
		assert!(load(&path).unwrap().is_empty());
	}
}
