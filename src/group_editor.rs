// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Talk-group edits and persistence, independent of terminal presentation.

use std::collections::BTreeSet;
use std::path::PathBuf;

use crate::bluez::Address;
use crate::groups::{TalkGroup, normalize_name, save};

pub(crate) struct GroupEditor {
	pub(crate) groups: Vec<TalkGroup>,
	pub(crate) persistence_error: Option<String>,
	pub(crate) error: Option<String>,
	groups_path: PathBuf,
}

impl GroupEditor {
	pub(crate) fn new(groups_path: PathBuf, groups: Vec<TalkGroup>) -> Self {
		Self {
			groups,
			persistence_error: None,
			error: None,
			groups_path,
		}
	}

	pub(crate) fn add(&mut self, name: &str) -> bool {
		let name = name.trim();
		if name.is_empty() {
			self.error = Some("Talk-group name cannot be empty.".into());
			return false;
		}
		if self
			.groups
			.iter()
			.any(|group| normalize_name(&group.name) == normalize_name(name))
		{
			self.error = Some(format!("A talk group named {name:?} already exists."));
			return false;
		}
		self.groups.push(TalkGroup {
			name: name.to_string(),
			members: BTreeSet::new(),
		});
		self.persist();
		true
	}

	pub(crate) fn remove(&mut self, index: usize) -> bool {
		if index >= self.groups.len() {
			return false;
		}
		self.groups.remove(index);
		self.persist();
		true
	}

	pub(crate) fn toggle_member(&mut self, index: usize, address: Address) -> bool {
		let Some(group) = self.groups.get_mut(index) else {
			return false;
		};
		if !group.members.remove(&address) {
			group.members.insert(address);
		}
		self.persist();
		true
	}

	fn persist(&mut self) {
		match save(&self.groups_path, &self.groups) {
			Ok(()) => {
				self.error = None;
				self.persistence_error = None;
			}
			Err(error) => self.persistence_error = Some(error),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::groups::load;

	fn path(label: &str) -> PathBuf {
		std::env::temp_dir()
			.join(format!(
				"bt-intercom-group-editor-{label}-{}",
				std::process::id()
			))
			.join("talk-groups.json")
	}

	#[test]
	fn edits_persist_and_duplicate_names_use_persistence_normalization() {
		let path = path("unicode");
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
		let mut editor = GroupEditor::new(
			path.clone(),
			vec![TalkGroup {
				name: "équipe".into(),
				members: BTreeSet::new(),
			}],
		);

		assert!(!editor.add("Équipe"));
		assert!(editor.error.as_deref().unwrap().contains("already exists"));
		assert!(editor.add(" Dispatch "));
		assert_eq!(editor.groups[1].name, "Dispatch");
		assert!(editor.toggle_member(1, "AA:BB:CC:DD:EE:01".into()));
		assert!(editor.remove(0));
		assert_eq!(load(&path).unwrap(), editor.groups);

		let _ = std::fs::remove_dir_all(path.parent().unwrap());
	}

	#[test]
	fn persistence_errors_are_reported_without_discarding_edits() {
		let path = path("failure");
		let directory = path.parent().unwrap();
		std::fs::create_dir_all(directory).unwrap();
		std::fs::write(directory.join("not-a-directory"), "file").unwrap();
		let mut editor = GroupEditor::new(
			directory.join("not-a-directory").join("groups.json"),
			Vec::new(),
		);

		assert!(editor.add("Dispatch"));

		assert_eq!(editor.groups.len(), 1);
		assert!(editor.persistence_error.is_some());
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
	}
}
