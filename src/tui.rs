// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Ratatui control panel for live headset monitoring and talk-group editing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

use crate::bluez::Address;
use crate::device_status::{
	ConnectionState, HeadsetStatus, SignalQuality, StatusPresentation, StatusUpdate, start_polling,
};
use crate::group_editor::GroupEditor;
use crate::groups::load;
use crate::terminal_style::{Meaning, Palette};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
	Groups,
	Members,
}

struct ControlPanel {
	allowed: BTreeSet<Address>,
	group_editor: GroupEditor,
	status: BTreeMap<Address, HeadsetStatus>,
	selected_group: Option<usize>,
	selected_headset: usize,
	focus: Focus,
	new_group_name: Option<String>,
	status_error: Option<String>,
	last_updated: Option<Instant>,
	palette: Palette,
}

impl ControlPanel {
	fn new(allowed: BTreeSet<Address>, groups_path: PathBuf) -> Result<Self, String> {
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
			status_error: None,
			last_updated: None,
			palette: Palette::detect(std::io::stdout().is_terminal()),
		})
	}

	fn apply_status_update(&mut self, update: StatusUpdate) {
		self.status = update.statuses;
		self.group_editor.error = None;
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
		if let Some(index) = self
			.selected_group
			.filter(|index| *index < self.group_editor.groups.len())
			.filter(|index| self.group_editor.remove(*index))
		{
			self.selected_group = if self.group_editor.groups.is_empty() {
				None
			} else {
				Some(index.min(self.group_editor.groups.len() - 1))
			};
		}
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
				let next = if down {
					(current + 1).min(self.group_editor.groups.len() - 1)
				} else {
					current.saturating_sub(1)
				};
				self.selected_group = Some(next);
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

	fn handle_key(&mut self, code: KeyCode) -> bool {
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

	fn status_rows(&self) -> Vec<Row<'_>> {
		self.allowed
			.iter()
			.map(|address| {
				let status = self.status.get(address);
				let presentation = StatusPresentation::new(
					status,
					Some(status.is_some_and(|status| status.duplex)),
				);
				let connection_meaning = match presentation.connection {
					ConnectionState::Connected => Meaning::Success,
					ConnectionState::Disconnected => Meaning::Error,
					ConnectionState::Unknown => Meaning::Warning,
				};
				let signal_meaning = match SignalQuality::from_rssi(presentation.rssi) {
					SignalQuality::Strong | SignalQuality::Good => Meaning::Success,
					SignalQuality::Fair => Meaning::Warning,
					SignalQuality::Weak => Meaning::Error,
					SignalQuality::Unknown => Meaning::Info,
				};
				Row::new([
					Cell::from(presentation.name_or("—").to_string()),
					Cell::from(Span::styled(
						address.as_str(),
						self.palette.style(Meaning::Info),
					)),
					Cell::from(Span::styled(
						presentation.connection.panel_label(),
						self.palette.style(connection_meaning),
					)),
					Cell::from(Span::styled(
						presentation.panel_duplex_label(),
						self.palette.style(if presentation.duplex == Some(true) {
							Meaning::Success
						} else {
							Meaning::Warning
						}),
					)),
					Cell::from(Span::styled(
						presentation.rssi_label(),
						self.palette.style(signal_meaning),
					)),
				])
			})
			.collect()
	}

	fn member_items(&self) -> Vec<ListItem<'_>> {
		self.allowed
			.iter()
			.map(|address| {
				let included = self
					.selected_group
					.and_then(|index| self.group_editor.groups.get(index))
					.is_some_and(|group| group.members.contains(address));
				let status = self.status.get(address);
				let presentation = StatusPresentation::new(
					status,
					Some(status.is_some_and(|status| status.duplex)),
				);
				let name = presentation.name_or(address.as_str());
				ListItem::new(Line::from(vec![
					Span::styled(
						if included { "[x] " } else { "[ ] " },
						self.palette.style(if included {
							Meaning::Success
						} else {
							Meaning::Warning
						}),
					),
					Span::raw(name),
					Span::styled(format!(" ({address})"), self.palette.style(Meaning::Info)),
				]))
			})
			.collect()
	}

	fn render(&self, frame: &mut Frame<'_>) {
		let areas: [ratatui::layout::Rect; 3] = Layout::vertical([
			Constraint::Length(3),
			Constraint::Min(8),
			Constraint::Length(3),
		])
		.areas(frame.area());
		frame.render_widget(
			Paragraph::new("bt-intercom | Headsets and talk groups")
				.block(Block::default().borders(Borders::ALL)),
			areas[0],
		);

		let sections: [ratatui::layout::Rect; 2] =
			Layout::vertical([Constraint::Percentage(55), Constraint::Percentage(45)])
				.areas(areas[1]);
		let rows = self.status_rows();
		let table = Table::new(
			rows,
			[
				Constraint::Percentage(22),
				Constraint::Percentage(32),
				Constraint::Percentage(18),
				Constraint::Percentage(16),
				Constraint::Percentage(12),
			],
		)
		.header(
			Row::new(["HEADSET", "ADDRESS", "BLUETOOTH", "DUPLEX", "SIGNAL"])
				.style(Style::default().add_modifier(Modifier::BOLD)),
		)
		.block(
			Block::default()
				.title("Live headset status")
				.borders(Borders::ALL),
		);
		frame.render_widget(table, sections[0]);

		let lower: [ratatui::layout::Rect; 2] =
			Layout::horizontal([Constraint::Percentage(40), Constraint::Percentage(60)])
				.areas(sections[1]);
		let group_items: Vec<ListItem<'_>> = self
			.group_editor
			.groups
			.iter()
			.map(|group| ListItem::new(group.name.as_str()))
			.collect();
		let group_title = if self.new_group_name.is_some() {
			format!(
				"New group: {}",
				self.new_group_name.as_deref().unwrap_or_default()
			)
		} else {
			"Talk groups [n add, d delete]".into()
		};
		let group_list = List::new(group_items)
			.block(
				Block::default()
					.title(group_title)
					.borders(Borders::ALL)
					.border_style(focus_style(self.palette, self.focus == Focus::Groups)),
			)
			.highlight_style(
				self.palette
					.style(Meaning::Selection)
					.add_modifier(Modifier::BOLD),
			)
			.highlight_symbol("> ");
		let mut group_state = ListState::default();
		group_state.select(self.selected_group);
		frame.render_stateful_widget(group_list, lower[0], &mut group_state);

		let members = self.member_items();
		let group_name = self
			.selected_group
			.and_then(|index| self.group_editor.groups.get(index))
			.map_or("Select a talk group", |group| group.name.as_str());
		let members_list = List::new(members)
			.block(
				Block::default()
					.title(format!("{group_name} [space toggle]"))
					.borders(Borders::ALL)
					.border_style(focus_style(self.palette, self.focus == Focus::Members)),
			)
			.highlight_style(
				self.palette
					.style(Meaning::Selection)
					.add_modifier(Modifier::BOLD),
			)
			.highlight_symbol("> ");
		let mut member_state = ListState::default();
		member_state.select((!self.allowed.is_empty()).then_some(self.selected_headset));
		frame.render_stateful_widget(members_list, lower[1], &mut member_state);

		let hint = self
			.group_editor
			.persistence_error
			.as_deref()
			.or(self.group_editor.error.as_deref())
			.or(self.status_error.as_deref())
			.unwrap_or({
				if let Some(name) = &self.new_group_name {
					if name.is_empty() {
						"Type a group name, Enter to save, Esc to cancel"
					} else {
						"Continue typing, Enter to save, Esc to cancel"
					}
				} else {
					"Tab switch pane | ↑/↓ select | n add | d delete | Space toggle member | q quit"
				}
			});
		let refreshed = self.last_updated.map_or_else(
			|| "waiting for status".to_string(),
			|updated| format!("status updated {}s ago", updated.elapsed().as_secs()),
		);
		frame.render_widget(
			Paragraph::new(format!("{hint} | {refreshed}"))
				.style(
					if self.group_editor.error.is_some()
						|| self.group_editor.persistence_error.is_some()
						|| self.status_error.is_some()
					{
						self.palette.style(Meaning::Error)
					} else {
						Style::default()
					},
				)
				.block(Block::default().borders(Borders::ALL)),
			areas[2],
		);
	}
}

fn focus_style(palette: Palette, focused: bool) -> Style {
	if focused {
		palette.style(Meaning::Selection)
	} else {
		Style::default()
	}
}

struct TerminalRestore;

impl Drop for TerminalRestore {
	fn drop(&mut self) {
		ratatui::restore();
	}
}

pub fn run(allowed: BTreeSet<Address>, groups_path: PathBuf) -> Result<(), String> {
	if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
		return Err("the control panel requires an interactive terminal".into());
	}
	let mut app = ControlPanel::new(allowed, groups_path)?;
	let mut terminal = ratatui::try_init().map_err(|error| error.to_string())?;
	let _restore = TerminalRestore;
	let (polling, status_rx) = start_polling(
		app.allowed.clone(),
		true,
		Duration::from_secs(5),
		Arc::new(AtomicBool::new(false)),
	);
	let result = event_loop(&mut terminal, &mut app, &status_rx);
	drop(_restore);
	drop(polling);
	result
}

fn event_loop(
	terminal: &mut DefaultTerminal,
	app: &mut ControlPanel,
	status_rx: &Receiver<StatusUpdate>,
) -> Result<(), String> {
	loop {
		while let Ok(update) = status_rx.try_recv() {
			app.apply_status_update(update);
		}
		terminal
			.draw(|frame| app.render(frame))
			.map_err(|error| error.to_string())?;
		if event::poll(Duration::from_millis(100)).map_err(|error| error.to_string())?
			&& let Event::Key(key) = event::read().map_err(|error| error.to_string())?
			&& key.kind == KeyEventKind::Press
			&& app.handle_key(key.code)
		{
			return Ok(());
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn keyboard_controls_persist_group_membership_and_removal() {
		let path = std::env::temp_dir()
			.join(format!("bt-intercom-tui-{}", std::process::id()))
			.join("talk-groups.json");
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
		let address = Address::parse("AA:BB:CC:DD:EE:01").unwrap();
		let mut panel = ControlPanel::new([address.clone()].into(), path.clone()).unwrap();

		assert!(!panel.handle_key(KeyCode::Char('n')));
		for character in "Dispatch".chars() {
			panel.handle_key(KeyCode::Char(character));
		}
		panel.handle_key(KeyCode::Enter);
		assert_eq!(panel.group_editor.groups[0].name, "Dispatch");
		assert_eq!(panel.focus, Focus::Members);
		panel.handle_key(KeyCode::Char(' '));
		assert!(panel.group_editor.groups[0].members.contains(&address));
		assert_eq!(load(&path).unwrap(), panel.group_editor.groups);

		panel.handle_key(KeyCode::Tab);
		panel.handle_key(KeyCode::Char('d'));
		assert!(panel.group_editor.groups.is_empty());
		assert!(load(&path).unwrap().is_empty());
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
	}

	#[test]
	fn status_updates_clear_transient_errors_but_keep_save_errors() {
		let path = std::env::temp_dir()
			.join(format!("bt-intercom-tui-errors-{}", std::process::id()))
			.join("talk-groups.json");
		let mut panel = ControlPanel::new(BTreeSet::new(), path).unwrap();
		panel.group_editor.error = Some("transient action error".into());
		panel.group_editor.persistence_error = Some("save failed".into());

		panel.apply_status_update(StatusUpdate {
			statuses: BTreeMap::new(),
			error: None,
		});

		assert!(panel.group_editor.error.is_none());
		assert_eq!(
			panel.group_editor.persistence_error.as_deref(),
			Some("save failed")
		);
	}
}
