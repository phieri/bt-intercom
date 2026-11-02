// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Timestamped, hardware-independent transmit policy.
//!
//! Full-duplex mappings start in PTT. Three distinct play/pause presses whose
//! first-to-third span is at most one second toggle that headset's always-open
//! mode. Releases are required between presses; repeats never count.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use crate::bluez::Address;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
	HalfDuplex,
	#[default]
	FullDuplex,
}

#[derive(Default)]
struct Button {
	pressed: bool,
	requested: bool,
	open: bool,
	presses: VecDeque<Instant>,
}

pub struct Transmit {
	mode: Mode,
	buttons: BTreeMap<Address, Button>,
	queue: VecDeque<Address>,
	active: BTreeSet<Address>,
	controllers: Option<BTreeMap<Address, String>>,
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
			pending: BTreeSet::new(),
		}
	}

	/// Half-duplex requests from headsets without a known controller stay
	/// queued but cannot receive the floor.
	pub fn set_controllers(&mut self, controllers: &BTreeMap<Address, String>) {
		self.controllers = Some(controllers.clone());
		self.refresh();
	}

	/// Returns whether a genuine transition was processed.
	pub fn event(&mut self, address: &Address, pressed: bool, at: Instant) -> bool {
		let Some(button) = self.buttons.get_mut(address) else {
			return false;
		};
		if button.pressed == pressed {
			return false;
		}
		button.pressed = pressed;
		button.requested = pressed;
		match self.mode {
			Mode::HalfDuplex => {
				if pressed {
					self.queue.push_back(address.to_owned());
				} else {
					self.queue.retain(|queued| queued != address);
				}
			}
			Mode::FullDuplex if pressed => {
				button.presses.retain(|previous| {
					at.checked_duration_since(*previous)
						.is_some_and(|elapsed| elapsed <= Duration::from_secs(1))
				});
				button.presses.push_back(at);
				if button.presses.len() == 3 {
					button.open = !button.open;
					button.presses.clear();
				}
			}
			Mode::FullDuplex => {}
		}
		self.refresh();
		true
	}

	/// Drop disconnected requests, including the floor. Reconnection alone must
	/// not restart a held or always-open mapped microphone: require a new press.
	/// Preserve the physical pressed bit so duplicate presses cannot re-arm it.
	pub fn topology(&mut self, available: &BTreeSet<Address>) -> bool {
		let before = self.active.clone();
		for (address, button) in &mut self.buttons {
			if !available.contains(address) {
				self.queue.retain(|queued| queued != address);
				button.requested = false;
				button.open = false;
				button.presses.clear();
			}
		}
		self.refresh();
		before != self.active
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
				.filter(|(_, b)| b.requested || b.open)
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
				Some(button) if button.open => "always open".into(),
				Some(button) if button.requested => "PTT held".into(),
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
		assert_eq!(full.dashboard_state(&address("a")), "PTT held");
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
	fn independent_triples_toggle_open_and_back_to_normal_hold() {
		let (mut t, now) = setup(Mode::FullDuplex);
		assert!(t.sources().is_empty());
		for ms in [0, 300, 1000] {
			press(&mut t, "a", now, ms);
			release(&mut t, "a", now, ms + 1);
		}
		assert_eq!(t.sources(), &set(&["a"])); // inclusive 1s window
		press(&mut t, "b", now, 1100);
		assert_eq!(t.sources(), &set(&["a", "b"]));
		release(&mut t, "b", now, 1101);
		assert_eq!(t.sources(), &set(&["a"]));
		for ms in [1200, 1300] {
			press(&mut t, "b", now, ms);
			release(&mut t, "b", now, ms + 1);
		}
		assert_eq!(t.sources(), &set(&["a", "b"]));
		for ms in [1400, 1500, 1600] {
			press(&mut t, "a", now, ms);
			if ms != 1600 {
				release(&mut t, "a", now, ms + 1);
			}
		}
		assert_eq!(t.sources(), &set(&["a", "b"])); // third press remains held
		release(&mut t, "a", now, 1601);
		assert_eq!(t.sources(), &set(&["b"])); // B stays independently open
		for ms in [1700, 1800, 1900] {
			press(&mut t, "b", now, ms);
			release(&mut t, "b", now, ms + 1);
		}
		assert!(t.sources().is_empty());
	}

	#[test]
	fn expired_window_and_duplicate_presses_do_not_toggle() {
		let (mut t, now) = setup(Mode::FullDuplex);
		for ms in [0, 400, 1001] {
			press(&mut t, "a", now, ms);
			assert!(!t.event(&address("a"), true, now + Duration::from_millis(ms)));
			release(&mut t, "a", now, ms + 1);
			assert!(!t.event(&address("a"), false, now + Duration::from_millis(ms + 2)));
		}
		assert!(t.sources().is_empty());
		// Sliding window: the 400, 1001, 1200 presses now constitute a triple.
		press(&mut t, "a", now, 1200);
		release(&mut t, "a", now, 1201);
		assert_eq!(t.sources(), &set(&["a"]));
	}

	#[test]
	fn half_never_toggles_and_rapid_changes_retire_pending_beeps() {
		let (mut t, now) = setup(Mode::HalfDuplex);
		for ms in [0, 10, 20] {
			press(&mut t, "a", now, ms);
			release(&mut t, "a", now, ms + 1);
		}
		assert!(t.sources().is_empty());
		assert!(t.pending.is_empty());
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
		assert!(!t.event(&address("a"), true, now));
		release(&mut t, "b", now, 2);
		assert!(t.sources().is_empty());
		release(&mut t, "a", now, 3);
		press(&mut t, "a", now, 4);
		assert_eq!(t.sources(), &set(&["a"]));
	}

	#[test]
	fn disconnect_resets_open_mode_and_gesture_history() {
		let (mut t, now) = setup(Mode::FullDuplex);
		for ms in [0, 10, 20] {
			press(&mut t, "a", now, ms);
			release(&mut t, "a", now, ms + 1);
		}
		assert!(t.topology(&set(&["b"])));
		t.topology(&set(&["a", "b"]));
		assert!(t.sources().is_empty());
		press(&mut t, "a", now, 30);
		release(&mut t, "a", now, 31);
		assert!(t.sources().is_empty());
	}
}
