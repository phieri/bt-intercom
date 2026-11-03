// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Timestamped, hardware-independent transmit policy.
//!
//! Headsets cannot report a held button: each press is a tap that sends AVRCP
//! PLAY or PAUSE (or a toggling play/pause). A play tap opens the headset's
//! microphone request and a pause tap releases it; the request is latched in
//! between. Full-duplex lets every requesting headset transmit at once,
//! half-duplex grants one shared floor in FIFO order.
//!
//! The optional host (the routing computer's wired headset) joins the same
//! floor. Its spacebar is a true push-to-talk: key-down requests and key-up
//! releases, so it never latches.

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

/// Anyone who can hold the floor: the routing computer or a Bluetooth headset.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Talker {
	Host,
	Headset(Address),
}

impl From<Address> for Talker {
	fn from(address: Address) -> Self {
		Self::Headset(address)
	}
}

#[derive(Default)]
struct Button {
	requested: bool,
}

pub struct Transmit {
	mode: Mode,
	buttons: BTreeMap<Address, Button>,
	host: Option<Button>,
	queue: VecDeque<Talker>,
	active: BTreeSet<Address>,
	host_active: bool,
	controllers: Option<BTreeMap<Address, String>>,
	dropped: BTreeSet<Address>,
	host_dropped: bool,
	pub pending: BTreeSet<Address>,
	pub host_pending: bool,
}

impl Transmit {
	pub fn new(mode: Mode, mapped: impl IntoIterator<Item = Address>) -> Self {
		Self {
			mode,
			buttons: mapped.into_iter().map(|a| (a, Button::default())).collect(),
			host: None,
			queue: VecDeque::new(),
			active: BTreeSet::new(),
			host_active: false,
			controllers: None,
			dropped: BTreeSet::new(),
			host_dropped: false,
			pending: BTreeSet::new(),
			host_pending: false,
		}
	}

	/// Lets the routing computer take part through its own push-to-talk key.
	pub fn with_host(mut self) -> Self {
		self.host = Some(Button::default());
		self
	}

	pub fn has_host(&self) -> bool {
		self.host.is_some()
	}

	/// Half-duplex requests from headsets without a known controller stay
	/// queued but cannot receive the floor.
	pub fn set_controllers(&mut self, controllers: &BTreeMap<Address, String>) {
		self.controllers = Some(controllers.clone());
		self.refresh();
	}

	/// Apply a headset's tap; a repeated play or pause is not a transition.
	#[cfg(test)]
	pub fn tap(&mut self, address: &Address, tap: Tap, at: Instant) -> bool {
		self.talker_tap(&Talker::Headset(address.clone()), tap, at)
	}

	/// Apply a tap or key transition from any talker. The host reports `Play`
	/// while its key is down and `Pause` once it is released.
	pub fn talker_tap(&mut self, talker: &Talker, tap: Tap, at: Instant) -> bool {
		let Some(button) = self.button(talker) else {
			return false;
		};
		let talking = match tap {
			Tap::Play => true,
			Tap::Pause => false,
			Tap::Toggle => !button.requested,
		};
		self.request(talker, talking, at)
	}

	fn button(&self, talker: &Talker) -> Option<&Button> {
		match talker {
			Talker::Host => self.host.as_ref(),
			Talker::Headset(address) => self.buttons.get(address),
		}
	}

	/// Set whether a headset requests to transmit. Returns whether this changed
	/// its request.
	#[cfg(test)]
	pub fn event(&mut self, address: &Address, talking: bool, at: Instant) -> bool {
		self.request(&Talker::Headset(address.clone()), talking, at)
	}

	fn request(&mut self, talker: &Talker, talking: bool, _at: Instant) -> bool {
		let button = match talker {
			Talker::Host => self.host.as_mut(),
			Talker::Headset(address) => self.buttons.get_mut(address),
		};
		let Some(button) = button else {
			return false;
		};
		if button.requested == talking {
			return false;
		}
		button.requested = talking;
		if self.mode == Mode::HalfDuplex {
			if talking {
				self.queue.push_back(talker.clone());
			} else {
				self.queue.retain(|queued| queued != talker);
			}
		}
		self.refresh();
		true
	}

	/// Drop disconnected requests, including the floor. Reconnection alone must
	/// not restart a mapped microphone: require a new play tap.
	pub fn topology(&mut self, available: &BTreeSet<Address>) -> bool {
		let before = (self.active.clone(), self.host_active);
		for (address, button) in &mut self.buttons {
			if !available.contains(address) {
				if button.requested {
					self.dropped.insert(address.clone());
				}
				self.queue
					.retain(|queued| *queued != Talker::Headset(address.clone()));
				button.requested = false;
			}
		}
		self.refresh();
		before != (self.active.clone(), self.host_active)
	}

	/// Drop the host's request while its wired headset is unavailable.
	pub fn host_topology(&mut self, available: bool) -> bool {
		let before = (self.active.clone(), self.host_active);
		if !available && let Some(button) = &mut self.host {
			self.host_dropped |= button.requested;
			button.requested = false;
			self.queue.retain(|queued| *queued != Talker::Host);
		}
		self.refresh();
		before != (self.active.clone(), self.host_active)
	}

	/// Headsets whose held or always-open request was discarded because the
	/// headset was unavailable, so callers can tell the user why nothing happened.
	pub fn take_dropped(&mut self) -> BTreeSet<Address> {
		std::mem::take(&mut self.dropped)
	}

	/// Whether the discarded host request was due to its wired headset vanishing.
	pub fn take_host_dropped(&mut self) -> bool {
		std::mem::take(&mut self.host_dropped)
	}

	fn refresh(&mut self) {
		let next: BTreeSet<Talker> = match self.mode {
			Mode::HalfDuplex => self
				.queue
				.front()
				.filter(|talker| match talker {
					Talker::Host => true,
					Talker::Headset(address) => self
						.controllers
						.as_ref()
						.is_none_or(|controllers| controllers.contains_key(address)),
				})
				.cloned()
				.into_iter()
				.collect(),
			Mode::FullDuplex => self
				.buttons
				.iter()
				.filter(|(_, b)| b.requested)
				.map(|(a, _)| Talker::Headset(a.clone()))
				.chain(
					self.host
						.as_ref()
						.filter(|button| button.requested)
						.map(|_| Talker::Host),
				)
				.collect(),
		};
		let next_host = next.contains(&Talker::Host);
		let next: BTreeSet<Address> = next
			.into_iter()
			.filter_map(|talker| match talker {
				Talker::Headset(address) => Some(address),
				Talker::Host => None,
			})
			.collect();
		self.pending.retain(|a| next.contains(a));
		self.pending.extend(next.difference(&self.active).cloned());
		self.active = next;
		self.host_pending = next_host && (self.host_pending || !self.host_active);
		self.host_active = next_host;
	}

	/// Whether any transmission still awaits its routing confirmation.
	pub fn has_pending(&self) -> bool {
		self.host_pending || !self.pending.is_empty()
	}

	/// Bluetooth headsets whose microphones currently hold the floor.
	pub fn sources(&self) -> &BTreeSet<Address> {
		&self.active
	}

	/// Whether the host's microphone currently holds the floor.
	pub fn host_active(&self) -> bool {
		self.host_active
	}

	pub(crate) fn dashboard_state(&self, address: &Address) -> String {
		if self.mode == Mode::FullDuplex {
			return match self.buttons.get(address) {
				None => "always open".into(),
				Some(button) if button.requested => "transmitting".into(),
				Some(_) => "muted".into(),
			};
		}
		let queued = self.queue.contains(&Talker::Headset(address.clone()));
		match (self.active.contains(address), queued) {
			(true, _) => "has floor".into(),
			(false, true) => "queued".into(),
			(false, false) => "idle".into(),
		}
	}

	pub(crate) fn dashboard_host_state(&self) -> String {
		let Some(button) = &self.host else {
			return "off".into();
		};
		if self.mode == Mode::FullDuplex {
			return if button.requested {
				"transmitting".into()
			} else {
				"muted".into()
			};
		}
		match (self.host_active, self.queue.contains(&Talker::Host)) {
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

	#[test]
	fn host_key_is_a_true_push_to_talk_sharing_the_fifo_floor() {
		let now = Instant::now();
		let mut t = Transmit::new(Mode::HalfDuplex, set(&["a"])).with_host();
		assert!(t.talker_tap(&Talker::Host, Tap::Play, now));
		assert!(t.host_active() && t.host_pending);
		assert!(t.sources().is_empty());
		assert_eq!(t.dashboard_host_state(), "has floor");
		press(&mut t, "a", now, 1);
		assert!(t.sources().is_empty());
		assert_eq!(t.dashboard_state(&address("a")), "queued");
		assert!(t.talker_tap(&Talker::Host, Tap::Pause, now));
		assert!(!t.host_active() && !t.host_pending);
		assert_eq!(t.sources(), &set(&["a"]));
		assert_eq!(t.dashboard_host_state(), "idle");
	}

	#[test]
	fn host_requests_are_ignored_without_a_host_and_dropped_when_unavailable() {
		let now = Instant::now();
		let mut none = Transmit::new(Mode::FullDuplex, set(&["a"]));
		assert!(!none.talker_tap(&Talker::Host, Tap::Play, now));
		assert_eq!(none.dashboard_host_state(), "off");

		let mut t = Transmit::new(Mode::FullDuplex, set(&["a"])).with_host();
		assert_eq!(t.dashboard_host_state(), "muted");
		t.talker_tap(&Talker::Host, Tap::Play, now);
		press(&mut t, "a", now, 0);
		assert_eq!(t.dashboard_host_state(), "transmitting");
		assert!(t.host_topology(false));
		assert!(!t.host_active() && !t.host_pending);
		assert!(t.take_host_dropped());
		assert!(!t.take_host_dropped());
		assert_eq!(t.sources(), &set(&["a"]));
		assert!(!t.host_topology(true));
	}

	#[test]
	fn host_in_a_queue_leaves_it_when_released_before_getting_the_floor() {
		let now = Instant::now();
		let mut t = Transmit::new(Mode::HalfDuplex, set(&["a"])).with_host();
		press(&mut t, "a", now, 0);
		t.talker_tap(&Talker::Host, Tap::Play, now);
		assert_eq!(t.dashboard_host_state(), "queued");
		t.talker_tap(&Talker::Host, Tap::Pause, now);
		release(&mut t, "a", now, 1);
		assert!(!t.host_active());
		assert_eq!(t.dashboard_host_state(), "idle");
	}
}
