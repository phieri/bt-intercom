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
	buttons: BTreeMap<String, Button>,
	groups: Vec<TalkGroup>,
	queues: BTreeMap<Option<String>, VecDeque<String>>,
	group_sources: BTreeMap<Option<String>, BTreeSet<String>>,
	active: BTreeSet<String>,
	controllers: Option<BTreeMap<String, String>>,
	sco_limit: usize,
	request_order: VecDeque<String>,
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
			controllers: None,
			sco_limit: 1,
			request_order: VecDeque::new(),
			pending: BTreeSet::new(),
		}
	}

	/// Apply a per-controller SCO source limit, independently of talk groups.
	/// Missing mappings cannot receive a grant, but held requests remain queued.
	pub fn set_controllers(&mut self, controllers: &BTreeMap<String, String>) {
		self.controllers = Some(controllers.clone());
		self.refresh();
	}

	pub fn set_sco_limit(&mut self, limit: usize) {
		self.sco_limit = limit;
		self.refresh();
	}

	/// Replace talk groups. Half-duplex requests are retired when membership
	/// changes so a held button must be released and pressed again.
	pub fn set_groups(&mut self, groups: &[TalkGroup]) -> bool {
		if self.groups == groups {
			return false;
		}
		self.groups = groups.to_vec();
		if self.mode == Mode::HalfDuplex {
			self.queues.clear();
			self.request_order.clear();
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
			Mode::HalfDuplex => {
				if pressed {
					self.request_order.push_back(address.to_owned());
					for membership in memberships {
						self.queues
							.entry(membership)
							.or_default()
							.push_back(address.to_owned());
					}
				} else {
					self.request_order.retain(|queued| queued != address);
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
				self.request_order.retain(|queued| queued != address);
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
			Mode::HalfDuplex => {
				let mut group_sources = self
					.queues
					.iter()
					.filter_map(|(group, queue)| {
						queue
							.front()
							.map(|address| (group.clone(), BTreeSet::from([address.clone()])))
					})
					.collect::<BTreeMap<_, _>>();
				if let Some(controllers) = &self.controllers {
					let candidates = group_sources;
					group_sources = BTreeMap::new();
					let mut reserved: BTreeMap<&String, BTreeSet<&String>> = BTreeMap::new();
					// Keep existing resource reservations before considering new
					// floors; lexical group names must not preempt a held source.
					for address in &self.request_order {
						let Some(controller) = controllers.get(address) else {
							continue;
						};
						let was_active = candidates.iter().any(|(group, addresses)| {
							addresses.contains(address)
								&& self
									.group_sources
									.get(group)
									.is_some_and(|sources| sources.contains(address))
						});
						if !was_active {
							continue;
						}
						let controller_sources = reserved.entry(controller).or_default();
						if !controller_sources.contains(address)
							&& controller_sources.len() >= self.sco_limit
						{
							continue;
						}
						controller_sources.insert(address);
						for (group, addresses) in &candidates {
							if addresses.contains(address)
								&& self
									.group_sources
									.get(group)
									.is_some_and(|sources| sources.contains(address))
							{
								group_sources
									.entry(group.clone())
									.or_default()
									.insert(address.clone());
							}
						}
					}
					for address in &self.request_order {
						let Some(controller) = controllers.get(address) else {
							continue;
						};
						let controller_sources = reserved.entry(controller).or_default();
						if !controller_sources.contains(address)
							&& controller_sources.len() >= self.sco_limit
						{
							continue;
						}
						let mut selected = false;
						for (group, addresses) in &candidates {
							if addresses.contains(address) {
								selected = true;
								group_sources
									.entry(group.clone())
									.or_default()
									.insert(address.clone());
							}
						}
						if selected {
							controller_sources.insert(address);
						}
					}
				}
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
	fn fifo_not_address_order_and_duplicates_do_not_requeue() {
		let (mut t, now) = setup(Mode::HalfDuplex);
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
	fn half_duplex_queues_are_independent_per_talk_group() {
		let (mut t, now) = setup(Mode::HalfDuplex);
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
	fn controller_floors_preserve_held_reservations_and_global_fifo() {
		let (mut t, now) = setup(Mode::HalfDuplex);
		t.set_groups(&[
			group("Zulu", &["a"]),
			group("Alpha", &["b"]),
			group("Middle", &["c"]),
		]);
		t.set_controllers(&BTreeMap::from([
			("a".into(), "hci0".into()),
			("b".into(), "hci0".into()),
			("c".into(), "hci0".into()),
		]));
		press(&mut t, "a", now, 0);
		press(&mut t, "c", now, 1);
		press(&mut t, "b", now, 2);
		assert_eq!(t.sources(), &set(&["a"]));
		release(&mut t, "a", now, 3);
		assert_eq!(t.sources(), &set(&["c"]));
		release(&mut t, "c", now, 4);
		assert_eq!(t.sources(), &set(&["b"]));
	}

	#[test]
	fn configured_controller_capacity_grants_multiple_queued_floors() {
		let (mut t, now) = setup(Mode::HalfDuplex);
		t.set_groups(&[
			group("Zulu", &["a"]),
			group("Alpha", &["b"]),
			group("Middle", &["c"]),
		]);
		t.set_sco_limit(2);
		t.set_controllers(&BTreeMap::from([
			("a".into(), "hci0".into()),
			("b".into(), "hci0".into()),
			("c".into(), "hci0".into()),
		]));
		press(&mut t, "a", now, 0);
		press(&mut t, "c", now, 1);
		press(&mut t, "b", now, 2);
		assert_eq!(t.sources(), &set(&["a", "c"]));
		release(&mut t, "a", now, 3);
		assert_eq!(t.sources(), &set(&["b", "c"]));
	}

	#[test]
	fn unmapped_controller_requests_are_retained_but_not_granted() {
		let (mut t, now) = setup(Mode::HalfDuplex);
		t.set_groups(&[group("Red", &["a"]), group("Blue", &["b"])]);
		t.set_controllers(&BTreeMap::new());
		press(&mut t, "a", now, 0);
		press(&mut t, "b", now, 1);
		assert!(t.sources().is_empty());
		t.set_controllers(&BTreeMap::from([
			("a".into(), "hci0".into()),
			("b".into(), "hci1".into()),
		]));
		assert_eq!(t.sources(), &set(&["a", "b"]));
		t.set_controllers(&BTreeMap::from([("b".into(), "hci1".into())]));
		assert_eq!(t.sources(), &set(&["b"]));
		t.set_controllers(&BTreeMap::from([
			("a".into(), "hci0".into()),
			("b".into(), "hci1".into()),
		]));
		assert_eq!(t.sources(), &set(&["a", "b"]));
	}

	#[test]
	fn enabling_constraints_keeps_the_oldest_existing_reservation() {
		let (mut t, now) = setup(Mode::HalfDuplex);
		t.set_groups(&[group("Zulu", &["a"]), group("Alpha", &["b"])]);
		press(&mut t, "a", now, 0);
		press(&mut t, "b", now, 1);
		assert_eq!(t.sources(), &set(&["a", "b"]));
		t.set_controllers(&BTreeMap::from([
			("a".into(), "hci0".into()),
			("b".into(), "hci0".into()),
		]));
		assert_eq!(t.sources(), &set(&["a"]));
	}

	#[test]
	fn half_duplex_ignores_requests_without_a_group_membership() {
		let (mut t, now) = setup(Mode::HalfDuplex);
		t.set_groups(&[group("Team", &["a", "b"])]);
		press(&mut t, "c", now, 0);
		press(&mut t, "a", now, 1);
		assert_eq!(t.sources(), &set(&["a"]));
	}

	#[test]
	fn changing_groups_retires_requests_until_buttons_are_repressed() {
		let (mut t, now) = setup(Mode::HalfDuplex);
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
