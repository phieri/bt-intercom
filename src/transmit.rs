// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Timestamped, hardware-independent transmit policy.
//!
//! Headsets cannot report a held button: each press is a tap that sends AVRCP
//! PLAY or PAUSE (or a toggling play/pause). A play tap opens the headset's
//! microphone request and a pause tap releases it; the request is latched in
//! between. Full-duplex lets every requesting headset transmit at once,
//! half-duplex grants one shared floor in FIFO order.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::Instant;

use crate::bluez::Address;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
	HalfDuplex,
	#[default]
	FullDuplex,
}

/// A decoded button tap.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tap {
	Play,
	Pause,
	Toggle,
}

#[derive(Default)]
struct Button {
	requested: bool,
}

pub struct Transmit {
	mode: Mode,
	buttons: BTreeMap<Address, Button>,
	queue: VecDeque<Address>,
	active: BTreeSet<Address>,
	controllers: Option<BTreeMap<Address, String>>,
	dropped: BTreeSet<Address>,
	pub pending: BTreeSet<Address>,
}

impl Transmit {
	pub fn new(mode: Mode, mapped: impl IntoIterator<Item = Address>) -> Self {
		Self {
			mode,
			buttons: mapped.into_iter().map(|a| (a, Button::default())).collect(),
			queue: VecDeque::new(),
			active: BTreeSet::new(),
			controllers: None,
			dropped: BTreeSet::new(),
			pending: BTreeSet::new(),
		}
	}

	/// Half-duplex requests from headsets without a known controller stay
	/// queued but cannot receive the floor.
	pub fn set_controllers(&mut self, controllers: &BTreeMap<Address, String>) {
		self.controllers = Some(controllers.clone());
		self.refresh();
	}

	/// Apply a tap; a repeated play or pause is not a transition.
	pub fn tap(&mut self, address: &Address, tap: Tap, at: Instant) -> bool {
		let Some(button) = self.buttons.get(address) else {
			return false;
		};
		let talking = match tap {
			Tap::Play => true,
			Tap::Pause => false,
			Tap::Toggle => !button.requested,
		};
		self.event(address, talking, at)
	}

	/// Set whether a headset requests to transmit. Returns whether this changed
	/// its request.
	pub fn event(&mut self, address: &Address, talking: bool, _at: Instant) -> bool {
		let Some(button) = self.buttons.get_mut(address) else {
			return false;
		};
		if button.requested == talking {
			return false;
		}
		button.requested = talking;
		if self.mode == Mode::HalfDuplex {
			if talking {
				self.queue.push_back(address.to_owned());
			} else {
				self.queue.retain(|queued| queued != address);
			}
		}
		self.refresh();
		true
	}

	/// Drop disconnected requests, including the floor. Reconnection alone must
	/// not restart a mapped microphone: require a new play tap.
	pub fn topology(&mut self, available: &BTreeSet<Address>) -> bool {
		let before = self.active.clone();
		for (address, button) in &mut self.buttons {
			if !available.contains(address) {
				if button.requested {
					self.dropped.insert(address.clone());
				}
				self.queue.retain(|queued| queued != address);
				button.requested = false;
			}
		}
		self.refresh();
		before != self.active
	}

	/// Headsets whose held or always-open request was discarded because the
	/// headset was unavailable, so callers can tell the user why nothing happened.
	pub fn take_dropped(&mut self) -> BTreeSet<Address> {
		std::mem::take(&mut self.dropped)
	}

	fn refresh(&mut self) {
		let next = match self.mode {
			Mode::HalfDuplex => self
				.queue
				.front()
				.filter(|address| {
					self.controllers
						.as_ref()
						.is_none_or(|controllers| controllers.contains_key(*address))
				})
				.cloned()
				.into_iter()
				.collect(),
			Mode::FullDuplex => self
				.buttons
				.iter()
				.filter(|(_, b)| b.requested)
				.map(|(a, _)| a.clone())
				.collect::<BTreeSet<_>>(),
		};
		self.pending.retain(|a| next.contains(a));
		self.pending.extend(next.difference(&self.active).cloned());
		self.active = next;
	}

	pub fn sources(&self) -> &BTreeSet<Address> {
		&self.active
	}

	pub(crate) fn dashboard_state(&self, address: &Address) -> String {
		if self.mode == Mode::FullDuplex {
			return match self.buttons.get(address) {
				None => "always open".into(),
				Some(button) if button.requested => "transmitting".into(),
				Some(_) => "muted".into(),
			};
		}
		let queued = self.queue.contains(address);
		match (self.active.contains(address), queued) {
			(true, _) => "has floor".into(),
			(false, true) => "queued".into(),
			(false, false) => "idle".into(),
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	fn address(value: &str) -> Address {
		Address::parse(value).unwrap_or_else(|_| {
			let suffix = value.bytes().fold(0u8, u8::wrapping_add);
			Address::parse(&format!("00:00:00:00:00:{suffix:02X}")).unwrap()
		})
	}

	fn setup(mode: Mode) -> (Transmit, Instant) {
		(Transmit::new(mode, set(&["a", "b", "c"])), Instant::now())
	}

	fn set(addresses: &[&str]) -> BTreeSet<Address> {
		addresses.iter().map(|value| address(value)).collect()
	}

	fn press(t: &mut Transmit, a: &str, base: Instant, ms: u64) {
		t.event(&address(a), true, base + Duration::from_millis(ms));
	}

	fn release(t: &mut Transmit, a: &str, base: Instant, ms: u64) {
		t.event(&address(a), false, base + Duration::from_millis(ms));
	}

	#[test]
	fn unavailable_headset_drops_request_and_reports_it_once() {
		let (mut t, now) = setup(Mode::FullDuplex);
		press(&mut t, "a", now, 0);
		t.topology(&set(&["b", "c"]));
		assert!(t.sources().is_empty());
		assert_eq!(t.take_dropped(), set(&["a"]));
		assert!(t.take_dropped().is_empty());
	}

	#[test]
	fn fifo_and_release_cancellation_only_confirm_the_granted_source() {
		let (mut t, now) = setup(Mode::HalfDuplex);
		press(&mut t, "b", now, 0);
		press(&mut t, "a", now, 10);
		press(&mut t, "c", now, 20);
		assert_eq!(t.sources(), &set(&["b"]));
		assert_eq!(t.pending, set(&["b"]));
		release(&mut t, "a", now, 30);
		release(&mut t, "b", now, 40);
		assert_eq!(t.sources(), &set(&["c"]));
		assert_eq!(t.pending, set(&["c"]));
		release(&mut t, "c", now, 50);
		assert!(t.sources().is_empty());
		assert!(t.pending.is_empty());
	}

	#[test]
	fn dashboard_state_shows_per_headset_ptt_floor_and_queue() {
		let (mut full, now) = setup(Mode::FullDuplex);
		assert_eq!(full.dashboard_state(&address("a")), "muted");
		press(&mut full, "a", now, 0);
		assert_eq!(full.dashboard_state(&address("a")), "transmitting");
		assert_eq!(full.dashboard_state(&address("b")), "muted");

		let (mut half, now) = setup(Mode::HalfDuplex);
		press(&mut half, "b", now, 0);
		press(&mut half, "a", now, 10);
		assert_eq!(half.dashboard_state(&address("b")), "has floor");
		assert_eq!(half.dashboard_state(&address("a")), "queued");
		release(&mut half, "b", now, 20);
		assert_eq!(half.dashboard_state(&address("a")), "has floor");
	}

	#[test]
	fn fifo_not_address_order_and_duplicates_do_not_requeue() {
		let (mut t, now) = setup(Mode::HalfDuplex);
		press(&mut t, "c", now, 0);
		press(&mut t, "b", now, 1);
		press(&mut t, "a", now, 2);
		assert!(!t.event(&address("b"), true, now));
		assert!(!t.event(&address("unknown"), true, now));
		release(&mut t, "c", now, 3);
		assert_eq!(t.sources(), &set(&["b"]));
		release(&mut t, "b", now, 4);
		assert_eq!(t.sources(), &set(&["a"]));
	}

	#[test]
	fn controller_mapping_gates_the_single_floor() {
		let (mut t, now) = setup(Mode::HalfDuplex);
		t.set_controllers(&BTreeMap::new());
		press(&mut t, "a", now, 0);
		press(&mut t, "b", now, 1);
		assert!(t.sources().is_empty());
		t.set_controllers(&BTreeMap::from([(address("a"), "hci0".into())]));
		assert_eq!(t.sources(), &set(&["a"]));
		t.set_controllers(&BTreeMap::from([(address("b"), "hci1".into())]));
		assert!(t.sources().is_empty());
		t.set_controllers(&BTreeMap::from([
			(address("a"), "hci0".into()),
			(address("b"), "hci1".into()),
		]));
		assert_eq!(t.sources(), &set(&["a"]));
		release(&mut t, "a", now, 2);
		assert_eq!(t.sources(), &set(&["b"]));
	}

	#[test]
	fn disconnect_releases_floor_and_reconnect_requires_new_press() {
		let (mut t, now) = setup(Mode::HalfDuplex);
		press(&mut t, "a", now, 0);
		press(&mut t, "b", now, 1);
		assert!(t.topology(&set(&["b", "c"])));
		assert_eq!(t.sources(), &set(&["b"]));
		assert_eq!(t.pending, set(&["b"]));
		t.topology(&set(&["a", "b", "c"]));
		assert_eq!(t.sources(), &set(&["b"]));
		assert!(!t.tap(&address("a"), Tap::Pause, now));
		release(&mut t, "b", now, 2);
		assert!(t.sources().is_empty());
		assert!(t.tap(&address("a"), Tap::Play, now));
		assert_eq!(t.sources(), &set(&["a"]));
	}

	#[test]
	fn play_opens_pause_releases_and_toggle_alternates() {
		for mode in [Mode::FullDuplex, Mode::HalfDuplex] {
			let (mut t, now) = setup(mode);
			assert!(t.tap(&address("a"), Tap::Play, now));
			assert!(!t.tap(&address("a"), Tap::Play, now));
			assert_eq!(t.sources(), &set(&["a"]));
			assert!(t.tap(&address("a"), Tap::Pause, now));
			assert!(!t.tap(&address("a"), Tap::Pause, now));
			assert!(t.sources().is_empty());
			assert!(t.tap(&address("a"), Tap::Toggle, now));
			assert_eq!(t.sources(), &set(&["a"]));
			assert!(t.tap(&address("a"), Tap::Toggle, now));
			assert!(t.sources().is_empty());
			assert!(t.pending.is_empty());
		}
	}
}
