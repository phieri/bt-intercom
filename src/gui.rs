// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Optional Iced control panel for live headset status and talk-group editing.

use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::time::Duration;

use iced::widget::{button, checkbox, column, container, row, scrollable, text, text_input};
use iced::{Element, Length, Subscription, Task};

use crate::bluez::{bluetooth_name, device_flag, property};
use crate::groups::{TalkGroup, load, save};
use crate::router::Router;
use crate::command;

#[derive(Clone, Debug)]
struct HeadsetStatus {
	name: Option<String>,
	connected: Option<bool>,
	duplex: bool,
	rssi: Option<i32>,
}

#[derive(Debug, Clone)]
enum Message {
	Poll,
	StatusUpdated(Result<BTreeMap<String, HeadsetStatus>, String>),
	GroupNameChanged(String),
	AddGroup,
	RemoveGroup(String),
	SetMember(String, String, bool),
}

struct ControlPanel {
	allowed: BTreeSet<String>,
	groups_path: PathBuf,
	groups: Vec<TalkGroup>,
	new_group_name: String,
	status: BTreeMap<String, HeadsetStatus>,
	polling: bool,
	error: Option<String>,
}

impl ControlPanel {
	fn update(&mut self, message: Message) -> Task<Message> {
		match message {
			Message::Poll if !self.polling => {
				self.polling = true;
				let allowed = self.allowed.clone();
				Task::perform(async move { poll_status(&allowed) }, Message::StatusUpdated)
			}
			Message::Poll => Task::none(),
			Message::StatusUpdated(result) => {
				self.polling = false;
				match result {
					Ok(status) => {
						self.status = status;
						self.error = None;
					}
					Err(error) => self.error = Some(error),
				}
				Task::none()
			}
			Message::GroupNameChanged(name) => {
				self.new_group_name = name;
				Task::none()
			}
			Message::AddGroup => {
				let name = self.new_group_name.trim();
				if name.is_empty() {
					self.error = Some("Enter a name for the talk group.".into());
				} else if self
					.groups
					.iter()
					.any(|group| group.name.eq_ignore_ascii_case(name))
				{
					self.error = Some(format!("A talk group named {name:?} already exists."));
				} else {
					self.groups.push(TalkGroup {
						name: name.to_string(),
						members: BTreeSet::new(),
					});
					self.new_group_name.clear();
					self.persist_groups();
				}
				Task::none()
			}
			Message::RemoveGroup(name) => {
				self.groups.retain(|group| group.name != name);
				self.persist_groups();
				Task::none()
			}
			Message::SetMember(group_name, address, included) => {
				if self.allowed.contains(&address)
					&& let Some(group) = self.groups.iter_mut().find(|g| g.name == group_name)
				{
					if included {
						group.members.insert(address);
					} else {
						group.members.remove(&address);
					}
					self.persist_groups();
				}
				Task::none()
			}
		}
	}

	fn view(&self) -> Element<'_, Message> {
		let mut content = column![
			text("rpi-intercom control panel").size(30),
			text("Headset monitoring refreshes every five seconds."),
			text("Headsets").size(24),
		]
		.spacing(12)
		.padding(20);

		for address in &self.allowed {
			let status = self.status.get(address);
			let name = status
				.and_then(|status| status.name.as_deref())
				.unwrap_or("Unnamed headset");
			let connected = match status.and_then(|status| status.connected) {
				Some(true) => "connected",
				Some(false) => "disconnected",
				None => "unknown",
			};
			let duplex = if status.is_some_and(|status| status.duplex) {
				"ready"
			} else {
				"unavailable"
			};
			let signal = status
				.and_then(|status| status.rssi)
				.map_or_else(|| "unknown signal".to_string(), |rssi| format!("{rssi} dBm"));
			content = content.push(text(format!(
				"{name} ({address}) — {connected}, duplex {duplex}, {signal}"
			)));
		}

		content = content.push(
			row![
				text_input("New talk group name", &self.new_group_name)
					.on_input(Message::GroupNameChanged)
					.on_submit(Message::AddGroup),
				button("Add group").on_press(Message::AddGroup),
			]
			.spacing(8),
		);
		content = content.push(text("Talk groups").size(24));
		if self.groups.is_empty() {
			content = content.push(text(
				"No groups configured. The intercom keeps its existing all-to-all routing.",
			));
		}
		for group in &self.groups {
			let name = group.name.clone();
			let mut members = column![
				row![
					text(&group.name).size(20),
					button("Remove group").on_press(Message::RemoveGroup(name.clone())),
				]
				.spacing(8),
			]
			.spacing(6);
			for address in &self.allowed {
				let group_name = group.name.clone();
				let address = address.clone();
				members = members.push(
					checkbox(address.clone(), group.members.contains(&address)).on_toggle(
						move |included| {
							Message::SetMember(group_name.clone(), address.clone(), included)
						},
					),
				);
			}
			content = content.push(container(members).padding(12));
		}
		if let Some(error) = &self.error {
			content = content.push(text(error));
		}
		scrollable(container(content).width(Length::Fill)).into()
	}

	fn subscription(&self) -> Subscription<Message> {
		iced::time::every(Duration::from_secs(5)).map(|_| Message::Poll)
	}

	fn persist_groups(&mut self) {
		if let Err(error) = save(&self.groups_path, &self.groups) {
			self.error = Some(error);
		} else {
			self.error = None;
		}
	}
}

pub fn run(allowed: BTreeSet<String>, groups_path: PathBuf) -> Result<(), String> {
	let groups = load(&groups_path)?;
	let status_allowed = allowed.clone();
	let state = ControlPanel {
		allowed,
		groups_path,
		groups,
		new_group_name: String::new(),
		status: BTreeMap::new(),
		polling: true,
		error: None,
	};
	iced::application("rpi-intercom", ControlPanel::update, ControlPanel::view)
		.subscription(ControlPanel::subscription)
		.run_with(move || {
			(
				state,
				Task::perform(async move { poll_status(&status_allowed) }, Message::StatusUpdated),
			)
		})
		.map_err(|error| error.to_string())
}

fn poll_status(allowed: &BTreeSet<String>) -> Result<BTreeMap<String, HeadsetStatus>, String> {
	let mut router = Router::new(allowed.clone());
	let (headsets, _) = router.inspect()?;
	let mut statuses = BTreeMap::new();
	for address in allowed {
		let info = command(&["bluetoothctl", "info", address], Duration::from_secs(15)).ok();
		let status = info.as_deref().map(|info| HeadsetStatus {
			name: bluetooth_name(info),
			connected: Some(device_flag(info, "Connected")),
			duplex: headsets
				.get(address)
				.is_some_and(|headset| headset.has_duplex_audio()),
			rssi: property(info, "RSSI")
				.and_then(|value| value.split_whitespace().next()?.parse().ok()),
		});
		statuses.insert(
			address.clone(),
			status.unwrap_or(HeadsetStatus {
				name: None,
				connected: None,
				duplex: headsets
					.get(address)
					.is_some_and(|headset| headset.has_duplex_audio()),
				rssi: None,
			}),
		);
	}
	Ok(statuses)
}
