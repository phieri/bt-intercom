// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Pure headset route selection, independent of PipeWire link ownership.

use std::collections::{BTreeMap, BTreeSet};

use crate::bluez::Address;
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
	desired_links_for_sources(headsets, &headsets.keys().cloned().collect::<BTreeSet<_>>())
}

/// Build routes only from the headsets whose microphones hold the floor.
#[cfg(test)]
pub(crate) fn desired_links_for_sources(
	headsets: &BTreeMap<Address, Headset>,
	active_sources: &BTreeSet<Address>,
) -> BTreeSet<Link> {
	desired_links_with_host(headsets, active_sources, None, false)
}

/// Like [`desired_links_for_sources`], with the routing computer's wired
/// headset as one more participant: its speakers hear every active headset,
/// and its microphone feeds every headset only while `host_talking`. The host
/// is never linked to itself.
pub(crate) fn desired_links_with_host(
	headsets: &BTreeMap<Address, Headset>,
	active_sources: &BTreeSet<Address>,
	host: Option<&Headset>,
	host_talking: bool,
) -> BTreeSet<Link> {
	let mut desired = BTreeSet::new();
	for (source_address, source) in headsets {
		if !active_sources.contains(source_address) {
			continue;
		}
		for (sink_address, sink) in headsets {
			if source_address != sink_address {
				link_ports(source, sink, &mut desired);
			}
		}
		if let Some(host) = host {
			link_ports(source, host, &mut desired);
		}
	}
	if let Some(host) = host
		&& host_talking
	{
		for sink in headsets.values() {
			link_ports(host, sink, &mut desired);
		}
	}
	desired
}

fn link_ports(source: &Headset, sink: &Headset, desired: &mut BTreeSet<Link>) {
	for output in &source.sources {
		for input in &sink.sinks {
			if channels_compatible(output, input) {
				desired.insert((output.id, input.id));
			}
		}
	}
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
	fn only_active_sources_are_routed() {
		let headsets = headsets();
		assert_eq!(
			desired_links_for_sources(&headsets, &[address("AA:BB:CC:DD:EE:01")].into()),
			[(1, 4), (1, 6)].into()
		);
		assert!(desired_links_for_sources(&headsets, &BTreeSet::new()).is_empty());
	}

	#[test]
	fn host_hears_active_headsets_and_feeds_all_only_while_talking() {
		let headsets = headsets();
		let host = Headset {
			sources: vec![Port {
				id: 7,
				node_id: None,
				channel: "FL".into(),
			}],
			sinks: vec![Port {
				id: 8,
				node_id: None,
				channel: "FL".into(),
			}],
		};
		let first: BTreeSet<_> = [address("AA:BB:CC:DD:EE:01")].into();
		assert_eq!(
			desired_links_with_host(&headsets, &first, Some(&host), false),
			[(1, 4), (1, 6), (1, 8)].into()
		);
		assert_eq!(
			desired_links_with_host(&headsets, &BTreeSet::new(), Some(&host), true),
			[(7, 2), (7, 4), (7, 6)].into()
		);
		assert!(
			desired_links_with_host(&headsets, &BTreeSet::new(), Some(&host), false).is_empty()
		);
		assert!(!desired_links_with_host(&headsets, &first, Some(&host), true).contains(&(7, 8)));
		assert_eq!(
			desired_links_with_host(&headsets, &first, None, true),
			desired_links_for_sources(&headsets, &first)
		);
	}
}
