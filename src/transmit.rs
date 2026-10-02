//! Timestamped, hardware-independent transmit policy.
//!
//! Full-duplex mappings start in PTT. Three distinct play/pause presses whose
//! first-to-third span is at most one second toggle that headset's always-open
//! mode. Releases are required between presses; repeats never count.

use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::time::{Duration, Instant};

use crate::groups::TalkGroup;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, clap::ValueEnum)]
pub enum Mode {
	SemiDuplex,
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
	buttons: BTreeMap<String, Button>,
	groups: Vec<TalkGroup>,
	queues: BTreeMap<Option<String>, VecDeque<String>>,
	group_sources: BTreeMap<Option<String>, BTreeSet<String>>,
	active: BTreeSet<String>,
	pub pending: BTreeSet<String>,
}

impl Transmit {
	pub fn new(mode: Mode, mapped: impl IntoIterator<Item = String>) -> Self {
		Self {
			mode,
			buttons: mapped.into_iter().map(|a| (a, Button::default())).collect(),
			groups: Vec::new(),
			queues: BTreeMap::new(),
			group_sources: BTreeMap::new(),
			active: BTreeSet::new(),
			pending: BTreeSet::new(),
		}
	}

	/// Replace talk groups. Semi-duplex requests are retired when membership
	/// changes so a held button must be released and pressed again.
	pub fn set_groups(&mut self, groups: &[TalkGroup]) -> bool {
		if self.groups == groups {
			return false;
		}
		self.groups = groups.to_vec();
		if self.mode == Mode::SemiDuplex {
			self.queues.clear();
			for button in self.buttons.values_mut() {
				button.requested = false;
			}
			self.pending.clear();
			self.refresh();
		}
		true
	}

	/// Returns whether a genuine transition was processed.
	pub fn event(&mut self, address: &str, pressed: bool, at: Instant) -> bool {
		let memberships = self.memberships(address);
		let Some(button) = self.buttons.get_mut(address) else {
			return false;
		};
		if button.pressed == pressed {
			return false;
		}
		button.pressed = pressed;
		button.requested = pressed;
		match self.mode {
			Mode::SemiDuplex => {
				if pressed {
					for membership in memberships {
						self.queues
							.entry(membership)
							.or_default()
							.push_back(address.to_owned());
					}
				} else {
					for queue in self.queues.values_mut() {
						queue.retain(|queued| queued != address);
					}
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
	pub fn topology(&mut self, available: &BTreeSet<String>) -> bool {
		let before = self.active.clone();
		for (address, button) in &mut self.buttons {
			if !available.contains(address) {
				button.requested = false;
				button.open = false;
				button.presses.clear();
				for queue in self.queues.values_mut() {
					queue.retain(|queued| queued != address);
				}
			}
		}
		self.refresh();
		before != self.active
	}

	fn refresh(&mut self) {
		let (next, group_sources) = match self.mode {
			Mode::SemiDuplex => {
				let group_sources = self
					.queues
					.iter()
					.filter_map(|(group, queue)| {
						queue
							.front()
							.map(|address| (group.clone(), BTreeSet::from([address.clone()])))
					})
					.collect::<BTreeMap<_, _>>();
				let active = group_sources.values().flatten().cloned().collect();
				(active, group_sources)
			}
			Mode::FullDuplex => (
				self.buttons
					.iter()
					.filter(|(_, b)| b.requested || b.open)
					.map(|(a, _)| a.clone())
					.collect::<BTreeSet<_>>(),
				BTreeMap::new(),
			),
		};
		self.pending.retain(|a| next.contains(a));
		self.pending.extend(next.difference(&self.active).cloned());
		self.active = next;
		self.group_sources = group_sources;
	}

	fn memberships(&self, address: &str) -> Vec<Option<String>> {
		if self.groups.is_empty() {
			return vec![None];
		}
		self.groups
			.iter()
			.filter(|group| group.members.contains(address))
			.map(|group| Some(group.name.clone()))
			.collect()
	}

	pub fn sources(&self) -> &BTreeSet<String> {
		&self.active
	}

	pub fn group_sources(&self) -> &BTreeMap<Option<String>, BTreeSet<String>> {
		&self.group_sources
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::groups::TalkGroup;

	fn setup(mode: Mode) -> (Transmit, Instant) {
		(
			Transmit::new(mode, ["a".into(), "b".into(), "c".into()]),
			Instant::now(),
		)
	}

	fn set(addresses: &[&str]) -> BTreeSet<String> {
		addresses.iter().map(|a| (*a).into()).collect()
	}

	fn group(name: &str, members: &[&str]) -> TalkGroup {
		TalkGroup {
			name: name.into(),
			members: set(members),
		}
	}

	fn press(t: &mut Transmit, a: &str, base: Instant, ms: u64) {
		t.event(a, true, base + Duration::from_millis(ms));
	}

	fn release(t: &mut Transmit, a: &str, base: Instant, ms: u64) {
		t.event(a, false, base + Duration::from_millis(ms));
	}

	#[test]
	fn fifo_and_release_cancellation_only_confirm_the_granted_source() {
		let (mut t, now) = setup(Mode::SemiDuplex);
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
	fn fifo_not_address_order_and_duplicates_do_not_requeue() {
		let (mut t, now) = setup(Mode::SemiDuplex);
		press(&mut t, "c", now, 0);
		press(&mut t, "b", now, 1);
		press(&mut t, "a", now, 2);
		assert!(!t.event("b", true, now));
		assert!(!t.event("unknown", true, now));
		release(&mut t, "c", now, 3);
		assert_eq!(t.sources(), &set(&["b"]));
		release(&mut t, "b", now, 4);
		assert_eq!(t.sources(), &set(&["a"]));
	}

	#[test]
	fn semi_duplex_queues_are_independent_per_talk_group() {
		let (mut t, now) = setup(Mode::SemiDuplex);
		t.set_groups(&[group("Red", &["a", "b"]), group("Blue", &["c", "d"])]);
		press(&mut t, "a", now, 0);
		press(&mut t, "b", now, 1);
		press(&mut t, "c", now, 2);
		press(&mut t, "d", now, 3);
		assert_eq!(t.sources(), &set(&["a", "c"]));
		assert_eq!(
			t.group_sources(),
			&BTreeMap::from([
				(Some("Blue".into()), set(&["c"])),
				(Some("Red".into()), set(&["a"])),
			])
		);
		release(&mut t, "a", now, 4);
		assert_eq!(t.sources(), &set(&["b", "c"]));
	}

	#[test]
	fn semi_duplex_ignores_requests_without_a_group_membership() {
		let (mut t, now) = setup(Mode::SemiDuplex);
		t.set_groups(&[group("Team", &["a", "b"])]);
		press(&mut t, "c", now, 0);
		press(&mut t, "a", now, 1);
		assert_eq!(t.sources(), &set(&["a"]));
	}

	#[test]
	fn changing_groups_retires_requests_until_buttons_are_repressed() {
		let (mut t, now) = setup(Mode::SemiDuplex);
		t.set_groups(&[group("Team", &["a", "b"])]);
		press(&mut t, "a", now, 0);
		press(&mut t, "b", now, 1);
		assert!(t.set_groups(&[group("Team", &["b", "c"])]));
		assert!(t.sources().is_empty());
		assert!(!t.event("a", true, now + Duration::from_millis(2)));
		release(&mut t, "b", now, 3);
		press(&mut t, "b", now, 4);
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
			assert!(!t.event("a", true, now + Duration::from_millis(ms)));
			release(&mut t, "a", now, ms + 1);
			assert!(!t.event("a", false, now + Duration::from_millis(ms + 2)));
		}
		assert!(t.sources().is_empty());
		// Sliding window: the 400, 1001, 1200 presses now constitute a triple.
		press(&mut t, "a", now, 1200);
		release(&mut t, "a", now, 1201);
		assert_eq!(t.sources(), &set(&["a"]));
	}

	#[test]
	fn semi_never_toggles_and_rapid_changes_retire_pending_beeps() {
		let (mut t, now) = setup(Mode::SemiDuplex);
		for ms in [0, 10, 20] {
			press(&mut t, "a", now, ms);
			release(&mut t, "a", now, ms + 1);
		}
		assert!(t.sources().is_empty());
		assert!(t.pending.is_empty());
	}

	#[test]
	fn disconnect_releases_floor_and_reconnect_requires_new_press() {
		let (mut t, now) = setup(Mode::SemiDuplex);
		press(&mut t, "a", now, 0);
		press(&mut t, "b", now, 1);
		assert!(t.topology(&set(&["b", "c"])));
		assert_eq!(t.sources(), &set(&["b"]));
		assert_eq!(t.pending, set(&["b"]));
		t.topology(&set(&["a", "b", "c"]));
		assert!(!t.event("a", true, now));
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
