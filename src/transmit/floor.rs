use std::collections::{BTreeMap, BTreeSet, VecDeque};

use crate::bluez::Address;

pub(super) type GroupSources = BTreeMap<Option<String>, BTreeSet<Address>>;

pub(super) fn allocate_floors(
	candidates: &GroupSources,
	previous: &GroupSources,
	request_order: &VecDeque<Address>,
	controllers: Option<&BTreeMap<Address, String>>,
	sco_limit: usize,
) -> GroupSources {
	let Some(controllers) = controllers else {
		return candidates.clone();
	};

	let mut group_sources = GroupSources::new();
	let mut reserved: BTreeMap<&str, BTreeSet<&Address>> = BTreeMap::new();

	for address in request_order {
		let Some(controller) = controllers.get(address).map(String::as_str) else {
			continue;
		};
		let active_groups = candidates
			.iter()
			.filter_map(|(group, addresses)| {
				(addresses.contains(address)
					&& previous
						.get(group)
						.is_some_and(|sources| sources.contains(address)))
				.then_some(group)
			})
			.collect::<Vec<_>>();
		if active_groups.is_empty() {
			continue;
		}

		let controller_sources = reserved.entry(controller).or_default();
		if !controller_sources.contains(address) && controller_sources.len() >= sco_limit {
			continue;
		}
		controller_sources.insert(address);
		for group in active_groups {
			group_sources
				.entry(group.clone())
				.or_default()
				.insert(address.clone());
		}
	}

	for address in request_order {
		let Some(controller) = controllers.get(address).map(String::as_str) else {
			continue;
		};
		let candidate_groups = candidates
			.iter()
			.filter_map(|(group, addresses)| addresses.contains(address).then_some(group))
			.collect::<Vec<_>>();
		if candidate_groups.is_empty() {
			continue;
		}

		let controller_sources = reserved.entry(controller).or_default();
		if !controller_sources.contains(address) && controller_sources.len() >= sco_limit {
			continue;
		}
		for group in candidate_groups {
			group_sources
				.entry(group.clone())
				.or_default()
				.insert(address.clone());
		}
		controller_sources.insert(address);
	}

	group_sources
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

	fn set(addresses: &[&str]) -> BTreeSet<Address> {
		addresses.iter().map(|value| address(value)).collect()
	}

	#[test]
	fn preserves_reservations_before_fifo_candidates() {
		let candidates = BTreeMap::from([
			(Some("Zulu".into()), set(&["c"])),
			(Some("Alpha".into()), set(&["a"])),
			(Some("Middle".into()), set(&["b"])),
		]);
		let previous = BTreeMap::from([(Some("Zulu".into()), set(&["c"]))]);
		let request_order = VecDeque::from([address("a"), address("b"), address("c")]);
		let controllers = BTreeMap::from([
			(address("a"), "hci0".into()),
			(address("b"), "hci0".into()),
			(address("c"), "hci0".into()),
		]);

		assert_eq!(
			allocate_floors(
				&candidates,
				&previous,
				&request_order,
				Some(&controllers),
				2
			),
			BTreeMap::from([
				(Some("Alpha".into()), set(&["a"])),
				(Some("Zulu".into()), set(&["c"])),
			])
		);
		assert_eq!(
			allocate_floors(
				&candidates,
				&GroupSources::new(),
				&request_order,
				Some(&controllers),
				1
			),
			BTreeMap::from([(Some("Alpha".into()), set(&["a"]))])
		);
	}

	#[test]
	fn skips_unmapped_requests_without_using_capacity() {
		let candidates = BTreeMap::from([
			(Some("Missing".into()), set(&["m"])),
			(Some("Mapped".into()), set(&["a"])),
		]);
		let request_order = VecDeque::from([address("m"), address("a")]);
		let controllers = BTreeMap::from([(address("a"), "hci0".into())]);

		assert_eq!(
			allocate_floors(
				&candidates,
				&GroupSources::new(),
				&request_order,
				Some(&controllers),
				1
			),
			BTreeMap::from([(Some("Mapped".into()), set(&["a"]))])
		);
	}
}
