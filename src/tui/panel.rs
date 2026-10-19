// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Control-panel interaction state, separate from terminal rendering.

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::time::Instant;

use crossterm::event::KeyCode;

use crate::bluez::Address;
use crate::device_status::{HeadsetStatus, StatusUpdate};
use crate::group_editor::GroupEditor;
use crate::groups::load;
use crate::terminal_style::Palette;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Focus {
	Groups,
	Members,
}

pub(super) struct ControlPanel {
	pub(super) allowed: BTreeSet<Address>,
	pub(super) group_editor: GroupEditor,
	pub(super) status: BTreeMap<Address, HeadsetStatus>,
	pub(super) selected_group: Option<usize>,
	pub(super) selected_headset: usize,
	pub(super) focus: Focus,
	pub(super) new_group_name: Option<String>,
	pub(super) confirm_last_group_removal: bool,
	pub(super) status_error: Option<String>,
	pub(super) last_updated: Option<Instant>,
	pub(super) palette: Palette,
}

impl ControlPanel {
	pub(super) fn new(allowed: BTreeSet<Address>, groups_path: PathBuf) -> Result<Self, String> {
		let groups = load(&groups_path)?;
		let selected_group = (!groups.is_empty()).then_some(0);
		Ok(Self {
			allowed,
			group_editor: GroupEditor::new(groups_path, groups),
			selected_group,
			status: BTreeMap::new(),
			selected_headset: 0,
			focus: Focus::Groups,
			new_group_name: None,
			confirm_last_group_removal: false,
			status_error: None,
			last_updated: None,
			palette: Palette::detect(std::io::stdout().is_terminal()),
		})
	}

	pub(super) fn apply_status_update(&mut self, update: StatusUpdate) {
		self.status = update.statuses;
		self.status_error = update.error;
		self.last_updated = Some(Instant::now());
	}

	fn add_group(&mut self) {
		let name = self
			.new_group_name
			.take()
			.unwrap_or_default()
			.trim()
			.to_string();
		if self.group_editor.add(&name) {
			self.selected_group = Some(self.group_editor.groups.len() - 1);
			self.focus = Focus::Members;
		}
	}

	fn remove_selected_group(&mut self) {
		if self.group_editor.groups.len() == 1 && self.selected_group == Some(0) {
			self.confirm_last_group_removal = true;
			return;
		}
		if let Some(index) = self.selected_group.filter(|index| {
			*index < self.group_editor.groups.len() && self.group_editor.remove(*index)
		}) {
			self.finish_group_removal(index);
		}
	}

	fn finish_group_removal(&mut self, index: usize) {
		self.selected_group = if self.group_editor.groups.is_empty() {
			None
		} else {
			Some(index.min(self.group_editor.groups.len() - 1))
		};
	}

	fn toggle_selected_member(&mut self) {
		let Some(group_index) = self
			.selected_group
			.filter(|index| *index < self.group_editor.groups.len())
		else {
			self.group_editor.error = Some("Create or select a talk group first.".into());
			return;
		};
		let Some(address) = self.allowed.iter().nth(self.selected_headset).cloned() else {
			return;
		};
		self.group_editor.toggle_member(group_index, address);
	}

	fn move_selection(&mut self, down: bool) {
		match self.focus {
			Focus::Groups if !self.group_editor.groups.is_empty() => {
				let current = self.selected_group.unwrap_or(0);
				self.selected_group = Some(if down {
					(current + 1).min(self.group_editor.groups.len() - 1)
				} else {
					current.saturating_sub(1)
				});
			}
			Focus::Members if !self.allowed.is_empty() => {
				self.selected_headset = if down {
					(self.selected_headset + 1).min(self.allowed.len() - 1)
				} else {
					self.selected_headset.saturating_sub(1)
				};
			}
			_ => {}
		}
	}

	pub(super) fn handle_key(&mut self, code: KeyCode) -> bool {
		self.group_editor.error = None;
		if self.confirm_last_group_removal {
			match code {
				KeyCode::Char('y' | 'Y') | KeyCode::Enter => {
					self.confirm_last_group_removal = false;
					if self.group_editor.remove(0) {
						self.finish_group_removal(0);
					}
				}
				KeyCode::Char('n' | 'N') | KeyCode::Esc => {
					self.confirm_last_group_removal = false;
				}
				_ => {}
			}
			return false;
		}
		if let Some(name) = self.new_group_name.as_mut() {
			match code {
				KeyCode::Esc => self.new_group_name = None,
				KeyCode::Enter => self.add_group(),
				KeyCode::Backspace => {
					name.pop();
				}
				KeyCode::Char(character) if !character.is_control() => name.push(character),
				_ => {}
			}
			return false;
		}
		match code {
			KeyCode::Char('q') | KeyCode::Esc => return true,
			KeyCode::Tab => {
				self.focus = match self.focus {
					Focus::Groups => Focus::Members,
					Focus::Members => Focus::Groups,
				}
			}
			KeyCode::Up => self.move_selection(false),
			KeyCode::Down => self.move_selection(true),
			KeyCode::Char('n') if self.focus == Focus::Groups => {
				self.new_group_name = Some(String::new());
				self.group_editor.error = None;
			}
			KeyCode::Char('d') if self.focus == Focus::Groups => self.remove_selected_group(),
			KeyCode::Char(' ') if self.focus == Focus::Members => self.toggle_selected_member(),
			_ => {}
		}
		false
	}
}
