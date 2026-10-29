// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Ratatui control panel for live headset monitoring and talk-group editing.

use std::collections::BTreeSet;
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::Receiver;
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

use crate::bluez::Address;
use crate::device_status::{
	ConnectionState, SignalQuality, StatusPresentation, StatusUpdate, start_polling,
};
use crate::terminal_style::{Meaning, Palette};

mod panel;
use panel::{ControlPanel, Focus};

impl ControlPanel {
	fn status_rows(&self) -> Vec<Row<'_>> {
		self.allowed
			.iter()
			.map(|address| {
				let status = self.status.get(address);
				let presentation = StatusPresentation::new(status, status.and_then(|s| s.duplex));
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
						presentation.battery_label(),
						self.palette.style(match presentation.battery {
							Some(0..=15)
								if presentation.connection == ConnectionState::Connected =>
							{
								Meaning::Error
							}
							Some(16..=30)
								if presentation.connection == ConnectionState::Connected =>
							{
								Meaning::Warning
							}
							_ => Meaning::Info,
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
				let presentation = StatusPresentation::new(status, status.and_then(|s| s.duplex));
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
				Constraint::Percentage(20),
				Constraint::Percentage(28),
				Constraint::Percentage(16),
				Constraint::Percentage(14),
				Constraint::Percentage(10),
				Constraint::Percentage(12),
			],
		)
		.header(
			Row::new([
				"HEADSET",
				"ADDRESS",
				"BLUETOOTH",
				"DUPLEX",
				"BATTERY",
				"SIGNAL",
			])
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
			.map(|group| {
				ListItem::new(format!("{} ({} headsets)", group.name, group.members.len()))
			})
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

		let hint = if self.confirm_last_group_removal {
			"Removing the last group restores all-to-all routing. Press y/Enter to confirm or n/Esc to cancel"
		} else {
			self.group_editor
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
						"Tab switch pane | ↑/↓ select | Home/End first/last | n add | d delete | Space toggle | q quit"
					}
				})
		};
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
	let mut redraw = true;
	let mut last_render = Instant::now();
	loop {
		while let Ok(update) = status_rx.try_recv() {
			app.apply_status_update(update);
			redraw = true;
		}
		if redraw || last_render.elapsed() >= Duration::from_secs(1) {
			terminal
				.draw(|frame| app.render(frame))
				.map_err(|error| error.to_string())?;
			redraw = false;
			last_render = Instant::now();
		}
		if event::poll(Duration::from_millis(100)).map_err(|error| error.to_string())?
			&& let Event::Key(key) = event::read().map_err(|error| error.to_string())?
			&& key.kind == KeyEventKind::Press
		{
			if app.handle_key(key.code) {
				return Ok(());
			}
			redraw = true;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::groups::load;
	use crossterm::event::KeyCode;
	use std::collections::BTreeMap;

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
		assert!(panel.confirm_last_group_removal);
		panel.handle_key(KeyCode::Enter);
		assert!(panel.group_editor.groups.is_empty());
		assert!(load(&path).unwrap().is_empty());
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
	}

	#[test]
	fn status_updates_keep_action_errors_until_the_next_key_but_keep_save_errors() {
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

		assert_eq!(
			panel.group_editor.error.as_deref(),
			Some("transient action error")
		);
		assert_eq!(
			panel.group_editor.persistence_error.as_deref(),
			Some("save failed")
		);
		panel.handle_key(KeyCode::Char('x'));
		assert!(panel.group_editor.error.is_none());
	}

	#[test]
	fn deleting_the_final_group_requires_confirmation() {
		let path = std::env::temp_dir()
			.join(format!("bt-intercom-tui-confirm-{}", std::process::id()))
			.join("talk-groups.json");
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
		let mut panel = ControlPanel::new(BTreeSet::new(), path.clone()).unwrap();
		assert!(panel.group_editor.add("Dispatch"));
		panel.focus = Focus::Groups;
		panel.selected_group = Some(0);

		panel.handle_key(KeyCode::Char('d'));
		assert!(panel.confirm_last_group_removal);
		assert_eq!(panel.group_editor.groups.len(), 1);

		panel.handle_key(KeyCode::Esc);
		assert!(!panel.confirm_last_group_removal);
		assert_eq!(panel.group_editor.groups.len(), 1);

		panel.handle_key(KeyCode::Char('d'));
		panel.handle_key(KeyCode::Char('y'));
		assert!(panel.group_editor.groups.is_empty());
		assert!(load(&path).unwrap().is_empty());
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
	}

	#[test]
	fn home_and_end_select_the_first_and_last_items_in_each_pane() {
		let path = std::env::temp_dir()
			.join(format!("bt-intercom-tui-edges-{}", std::process::id()))
			.join("talk-groups.json");
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
		let addresses = [
			Address::parse("AA:BB:CC:DD:EE:01").unwrap(),
			Address::parse("AA:BB:CC:DD:EE:02").unwrap(),
		]
		.into();
		let mut panel = ControlPanel::new(addresses, path.clone()).unwrap();
		assert!(panel.group_editor.add("Dispatch"));
		assert!(panel.group_editor.add("Stage"));
		panel.selected_group = Some(0);
		panel.handle_key(KeyCode::End);
		assert_eq!(panel.selected_group, Some(1));
		panel.handle_key(KeyCode::Home);
		assert_eq!(panel.selected_group, Some(0));

		panel.focus = Focus::Members;
		panel.handle_key(KeyCode::End);
		assert_eq!(panel.selected_headset, 1);
		panel.handle_key(KeyCode::Home);
		assert_eq!(panel.selected_headset, 0);
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
	}
}
