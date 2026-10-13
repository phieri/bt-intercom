//! Pure headset route selection, independent of PipeWire link ownership.

use std::collections::{BTreeMap, BTreeSet};

use crate::bluez::Address;
use crate::groups::TalkGroup;
use crate::pipewire::{Headset, Link, MONO_CHANNEL, Port};

pub(crate) fn channels_compatible(output: &Port, input: &Port) -> bool {
	output.channel == input.channel
		|| output.channel == MONO_CHANNEL
		|| input.channel == MONO_CHANNEL
}

/// Build cross-headset source-to-sink links whose channels are compatible.
///
/// A headset is never linked to itself; mono ports are compatible with every
/// channel on the opposite endpoint.
#[cfg(test)]
pub(crate) fn desired_links(headsets: &BTreeMap<Address, Headset>) -> BTreeSet<Link> {
	desired_links_with_policy(headsets, None, None)
}

fn desired_links_with_policy(
	headsets: &BTreeMap<Address, Headset>,
	allowed_pairs: Option<&BTreeMap<Address, BTreeSet<Address>>>,
	active_sources: Option<&BTreeSet<Address>>,
) -> BTreeSet<Link> {
	let mut desired = BTreeSet::new();
	for (source_address, source) in headsets {
		if active_sources.is_some_and(|sources| !sources.contains(source_address)) {
			continue;
		}
		for (sink_address, sink) in headsets {
			if source_address == sink_address
				|| allowed_pairs.is_some_and(|pairs| {
					!pairs
						.get(source_address)
						.is_some_and(|sinks| sinks.contains(sink_address))
				}) {
				continue;
			}
			for output in &source.sources {
				for input in &sink.sinks {
					if channels_compatible(output, input) {
						desired.insert((output.id, input.id));
					}
				}
			}
		}
	}
	desired
}

fn group_pairs(groups: &[TalkGroup]) -> BTreeMap<Address, BTreeSet<Address>> {
	let mut pairs = BTreeMap::new();
	for group in groups {
		for source in &group.members {
			pairs
				.entry(source.clone())
				.or_insert_with(BTreeSet::new)
				.extend(group.members.iter().filter(|sink| *sink != source).cloned());
		}
	}
	pairs
}

/// Build routes only between headset pairs that share a talk group.
///
/// An empty group list retains the original all-to-all behavior.
#[cfg(test)]
pub(crate) fn desired_links_in_groups(
	headsets: &BTreeMap<Address, Headset>,
	groups: &[TalkGroup],
) -> BTreeSet<Link> {
	if groups.is_empty() {
		return desired_links(headsets);
	}
	let allowed_pairs = group_pairs(groups);
	desired_links_with_policy(headsets, Some(&allowed_pairs), None)
}

/// Build routes only for sources granted a floor in the corresponding group.
///
/// With no configured groups, the `None` entry represents the shared floor.
pub(crate) fn desired_links_by_group(
	headsets: &BTreeMap<Address, Headset>,
	group_sources: &BTreeMap<Option<String>, BTreeSet<Address>>,
	groups: &[TalkGroup],
) -> BTreeSet<Link> {
	if groups.is_empty() {
		let sources = group_sources.get(&None).cloned().unwrap_or_default();
		return desired_links_with_policy(headsets, None, Some(&sources));
	}

	groups
		.iter()
		.flat_map(|group| {
			let Some(sources) = group_sources.get(&Some(group.name.clone())) else {
				return BTreeSet::new();
			};
			let allowed_pairs = group_pairs(std::slice::from_ref(group));
			desired_links_with_policy(headsets, Some(&allowed_pairs), Some(sources))
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	fn address(value: &str) -> Address {
		Address::parse(value).unwrap()
	}

	fn headset(source: u64, sink: u64) -> Headset {
		Headset {
			sources: vec![Port {
				id: source,
				node_id: None,
				channel: MONO_CHANNEL.into(),
			}],
			sinks: vec![Port {
				id: sink,
				node_id: None,
				channel: MONO_CHANNEL.into(),
			}],
		}
	}

	fn headsets() -> BTreeMap<Address, Headset> {
		BTreeMap::from([
			(address("AA:BB:CC:DD:EE:01"), headset(1, 2)),
			(address("AA:BB:CC:DD:EE:02"), headset(3, 4)),
			(address("AA:BB:CC:DD:EE:03"), headset(5, 6)),
		])
	}

	fn group(name: &str, members: &[&str]) -> TalkGroup {
		TalkGroup {
			name: name.into(),
			members: members
				.iter()
				.map(|member| crate::bluez::parse_address(member).unwrap())
				.collect(),
		}
	}

	#[test]
	fn all_to_all_policy_links_only_other_headsets_and_honors_channels() {
		let headsets = headsets();
		assert_eq!(
			desired_links(&headsets),
			[(1, 4), (1, 6), (3, 2), (3, 6), (5, 2), (5, 4)].into()
		);

		let stereo = Port {
			id: 7,
			node_id: None,
			channel: "FL".into(),
		};
		let other_stereo = Port {
			id: 8,
			node_id: None,
			channel: "FR".into(),
		};
		let mono = Port {
			id: 9,
			node_id: None,
			channel: MONO_CHANNEL.into(),
		};
		assert!(!channels_compatible(&stereo, &other_stereo));
		assert!(channels_compatible(&stereo, &stereo));
		assert!(channels_compatible(&stereo, &mono));
	}

	#[test]
	fn group_policy_limits_peers_and_active_sources_per_group() {
		let headsets = headsets();
		let groups = [
			group("North", &["AA:BB:CC:DD:EE:01", "AA:BB:CC:DD:EE:02"]),
			group("South", &["AA:BB:CC:DD:EE:02", "AA:BB:CC:DD:EE:03"]),
		];
		assert_eq!(
			desired_links_in_groups(&headsets, &groups),
			[(1, 4), (3, 2), (3, 6), (5, 4)].into()
		);

		let floors = BTreeMap::from([
			(
				Some("North".into()),
				[address("AA:BB:CC:DD:EE:01")].into(),
			),
			(
				Some("South".into()),
				[address("AA:BB:CC:DD:EE:03")].into(),
			),
		]);
		assert_eq!(
			desired_links_by_group(&headsets, &floors, &groups),
			[(1, 4), (5, 4)].into()
		);
		assert_eq!(
			desired_links_by_group(
				&headsets,
				& BTreeMap::from([(None, [address("AA:BB:CC:DD:EE:01")].into())]),
				&[]
			),
			[(1, 4), (1, 6)].into()
		);
	}
}
