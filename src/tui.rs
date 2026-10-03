// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Ratatui control panel for live headset monitoring and talk-group editing.

use std::collections::{BTreeMap, BTreeSet};
use std::io::IsTerminal;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::{Duration, Instant};

use crossterm::event::{self, Event, KeyCode, KeyEventKind};
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, List, ListItem, ListState, Paragraph, Row, Table};
use ratatui::{DefaultTerminal, Frame};

use crate::bluez::{bluetooth_name, device_flag, property};
use crate::command_cancellable;
use crate::groups::{TalkGroup, load, normalize_name, save};
use crate::router::{Headset, Router};
use crate::terminal_style::{Meaning, Palette};

#[derive(Clone, Debug)]
struct HeadsetStatus {
	name: Option<String>,
	connected: Option<bool>,
	duplex: bool,
	rssi: Option<i32>,
}

struct StatusUpdate {
	statuses: BTreeMap<String, HeadsetStatus>,
	error: Option<String>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Focus {
	Groups,
	Members,
}

struct ControlPanel {
	allowed: BTreeSet<String>,
	groups_path: PathBuf,
	groups: Vec<TalkGroup>,
	status: BTreeMap<String, HeadsetStatus>,
	selected_group: Option<usize>,
	selected_headset: usize,
	focus: Focus,
	new_group_name: Option<String>,
	error: Option<String>,
	persistence_error: Option<String>,
	status_error: Option<String>,
	last_updated: Option<Instant>,
	palette: Palette,
}

impl ControlPanel {
	fn new(allowed: BTreeSet<String>, groups_path: PathBuf) -> Result<Self, String> {
		let groups = load(&groups_path)?;
		Ok(Self {
			allowed,
			groups_path,
			selected_group: (!groups.is_empty()).then_some(0),
			groups,
			status: BTreeMap::new(),
			selected_headset: 0,
			focus: Focus::Groups,
			new_group_name: None,
			error: None,
			persistence_error: None,
			status_error: None,
			last_updated: None,
			palette: Palette::detect(std::io::stdout().is_terminal()),
		})
	}

	fn persist_groups(&mut self) {
		match save(&self.groups_path, &self.groups) {
			Ok(()) => {
				self.error = None;
				self.persistence_error = None;
			}
			Err(error) => self.persistence_error = Some(error),
		}
	}

	fn apply_status_update(&mut self, update: StatusUpdate) {
		self.status = update.statuses;
		self.error = None;
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
		if name.is_empty() {
			self.error = Some("Talk-group name cannot be empty.".into());
		} else if self
			.groups
			.iter()
			.any(|group| normalize_name(&group.name) == normalize_name(&name))
		{
			self.error = Some(format!("A talk group named {name:?} already exists."));
		} else {
			self.groups.push(TalkGroup {
				name,
				members: BTreeSet::new(),
			});
			self.selected_group = Some(self.groups.len() - 1);
			self.focus = Focus::Members;
			self.persist_groups();
		}
	}

	fn remove_selected_group(&mut self) {
		if let Some(index) = self
			.selected_group
			.filter(|index| *index < self.groups.len())
		{
			self.groups.remove(index);
			self.selected_group = if self.groups.is_empty() {
				None
			} else {
				Some(index.min(self.groups.len() - 1))
			};
			self.persist_groups();
		}
	}

	fn toggle_selected_member(&mut self) {
		let Some(group) = self
			.selected_group
			.and_then(|index| self.groups.get_mut(index))
		else {
			self.error = Some("Create or select a talk group first.".into());
			return;
		};
		let Some(address) = self.allowed.iter().nth(self.selected_headset).cloned() else {
			return;
		};
		if !group.members.remove(&address) {
			group.members.insert(address);
		}
		self.persist_groups();
	}

	fn move_selection(&mut self, down: bool) {
		match self.focus {
			Focus::Groups if !self.groups.is_empty() => {
				let current = self.selected_group.unwrap_or(0);
				let next = if down {
					(current + 1).min(self.groups.len() - 1)
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
				self.error = None;
			}
			KeyCode::Char('d') if self.focus == Focus::Groups => self.remove_selected_group(),
			KeyCode::Char(' ') if self.focus == Focus::Members => self.toggle_selected_member(),
			_ => {}
		}
		false
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
		let rows: Vec<Row<'_>> = self
			.allowed
			.iter()
			.map(|address| {
				let status = self.status.get(address);
				let name = status
					.and_then(|status| status.name.as_deref())
					.unwrap_or("—");
				let connected_state = status.and_then(|status| status.connected);
				let connected = match connected_state {
					Some(true) => "connected",
					Some(false) => "disconnected",
					None => "unknown",
				};
				let duplex_ready = status.is_some_and(|status| status.duplex);
				let duplex = if duplex_ready { "ready" } else { "unavailable" };
				let rssi = status
					.and_then(|status| status.rssi)
					.map_or_else(|| "—".to_string(), |rssi| format!("{rssi} dBm"));
				let connection_meaning = match connected_state {
					Some(true) => Meaning::Success,
					Some(false) => Meaning::Error,
					None => Meaning::Warning,
				};
				let signal_meaning = match status.and_then(|status| status.rssi) {
					Some(-70..) => Meaning::Success,
					Some(-80..=-71) => Meaning::Warning,
					Some(_) => Meaning::Error,
					None => Meaning::Info,
				};
				Row::new([
					Cell::from(name.to_string()),
					Cell::from(Span::styled(
						address.clone(),
						self.palette.style(Meaning::Info),
					)),
					Cell::from(Span::styled(
						connected,
						self.palette.style(connection_meaning),
					)),
					Cell::from(Span::styled(
						duplex,
						self.palette.style(if duplex_ready {
							Meaning::Success
						} else {
							Meaning::Warning
						}),
					)),
					Cell::from(Span::styled(rssi, self.palette.style(signal_meaning))),
				])
			})
			.collect();
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

		let members: Vec<ListItem<'_>> = self
			.allowed
			.iter()
			.map(|address| {
				let included = self
					.selected_group
					.and_then(|index| self.groups.get(index))
					.is_some_and(|group| group.members.contains(address));
				let status = self.status.get(address);
				let name = status
					.and_then(|status| status.name.as_deref())
					.unwrap_or(address.as_str());
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
			.collect();
		let group_name = self
			.selected_group
			.and_then(|index| self.groups.get(index))
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
			.persistence_error
			.as_deref()
			.or(self.error.as_deref())
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
					if self.error.is_some()
						|| self.persistence_error.is_some()
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

struct PollingWorker {
	stopped: Arc<AtomicBool>,
	shutdown: mpsc::Sender<()>,
	worker: Option<JoinHandle<()>>,
}

impl PollingWorker {
	fn start(allowed: BTreeSet<String>, status_tx: mpsc::Sender<StatusUpdate>) -> Self {
		let stopped = Arc::new(AtomicBool::new(false));
		let worker_stopped = Arc::clone(&stopped);
		let (shutdown, shutdown_rx) = mpsc::channel();
		let worker = thread::spawn(move || {
			while !worker_stopped.load(Ordering::SeqCst) {
				let Some(update) = poll_status(&allowed, &worker_stopped) else {
					break;
				};
				if status_tx.send(update).is_err() {
					break;
				}
				if shutdown_rx.recv_timeout(Duration::from_secs(5)).is_ok() {
					break;
				}
			}
		});
		Self {
			stopped,
			shutdown,
			worker: Some(worker),
		}
	}
}

impl Drop for PollingWorker {
	fn drop(&mut self) {
		self.stopped.store(true, Ordering::SeqCst);
		let _ = self.shutdown.send(());
		if let Some(worker) = self.worker.take() {
			let _ = worker.join();
		}
	}
}

pub fn run(allowed: BTreeSet<String>, groups_path: PathBuf) -> Result<(), String> {
	if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
		return Err("the TUI requires an interactive terminal".into());
	}
	let mut app = ControlPanel::new(allowed, groups_path)?;
	let (status_tx, status_rx) = mpsc::channel();
	let mut terminal = ratatui::try_init().map_err(|error| error.to_string())?;
	let _restore = TerminalRestore;
	let polling = PollingWorker::start(app.allowed.clone(), status_tx);
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

fn poll_status(allowed: &BTreeSet<String>, stopped: &AtomicBool) -> Option<StatusUpdate> {
	let mut router = Router::with_executor(allowed.clone(), |args| {
		command_cancellable(args, Duration::from_secs(15), Some(stopped))
	});
	let (headsets, error) = match router.inspect() {
		Ok((headsets, _)) => (headsets, None),
		Err(error) => (BTreeMap::new(), Some(error)),
	};
	if stopped.load(Ordering::SeqCst) {
		return None;
	}
	let mut statuses = BTreeMap::new();
	for address in allowed {
		if stopped.load(Ordering::SeqCst) {
			return None;
		}
		let info = command_cancellable(
			&["bluetoothctl", "info", address],
			Duration::from_secs(15),
			Some(stopped),
		)
		.ok();
		if stopped.load(Ordering::SeqCst) {
			return None;
		}
		statuses.insert(
			address.clone(),
			HeadsetStatus {
				name: info.as_deref().and_then(bluetooth_name),
				connected: info.as_deref().map(|info| device_flag(info, "Connected")),
				duplex: headsets.get(address).is_some_and(Headset::has_duplex_audio),
				rssi: info.as_deref().and_then(|info| {
					property(info, "RSSI")
						.and_then(|value| value.split_whitespace().next()?.parse().ok())
				}),
			},
		);
	}
	Some(StatusUpdate { statuses, error })
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
		let address = "AA:BB:CC:DD:EE:01".to_string();
		let mut panel = ControlPanel::new([address.clone()].into(), path.clone()).unwrap();

		assert!(!panel.handle_key(KeyCode::Char('n')));
		for character in "Dispatch".chars() {
			panel.handle_key(KeyCode::Char(character));
		}
		panel.handle_key(KeyCode::Enter);
		assert_eq!(panel.groups[0].name, "Dispatch");
		assert_eq!(panel.focus, Focus::Members);
		panel.handle_key(KeyCode::Char(' '));
		assert!(panel.groups[0].members.contains(&address));
		assert_eq!(load(&path).unwrap(), panel.groups);

		panel.handle_key(KeyCode::Tab);
		panel.handle_key(KeyCode::Char('d'));
		assert!(panel.groups.is_empty());
		assert!(load(&path).unwrap().is_empty());
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
	}

	#[test]
	fn group_name_uniqueness_matches_persistence_normalization() {
		let path = std::env::temp_dir()
			.join(format!("bt-intercom-tui-unicode-{}", std::process::id()))
			.join("talk-groups.json");
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
		save(
			&path,
			&[TalkGroup {
				name: "équipe".into(),
				members: BTreeSet::new(),
			}],
		)
		.unwrap();
		let mut panel = ControlPanel::new(BTreeSet::new(), path.clone()).unwrap();
		panel.new_group_name = Some("Équipe".into());

		panel.add_group();

		assert_eq!(panel.groups.len(), 1);
		assert!(panel.error.as_deref().unwrap().contains("already exists"));
		let _ = std::fs::remove_dir_all(path.parent().unwrap());
	}

	#[test]
	fn cancelling_status_poll_stops_before_running_commands() {
		let stopped = AtomicBool::new(true);
		assert!(poll_status(&BTreeSet::new(), &stopped).is_none());
	}

	#[test]
	fn status_updates_clear_transient_errors_but_keep_save_errors() {
		let path = std::env::temp_dir()
			.join(format!("bt-intercom-tui-errors-{}", std::process::id()))
			.join("talk-groups.json");
		let mut panel = ControlPanel::new(BTreeSet::new(), path).unwrap();
		panel.error = Some("transient action error".into());
		panel.persistence_error = Some("save failed".into());

		panel.apply_status_update(StatusUpdate {
			statuses: BTreeMap::new(),
			error: None,
		});

		assert!(panel.error.is_none());
		assert_eq!(panel.persistence_error.as_deref(), Some("save failed"));
	}
}
