// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Metrics and readiness derived only from observed, valid inter-headset links.

use std::collections::{BTreeMap, BTreeSet};

use crate::bluez::Address;
use crate::pipewire::Headset;
use crate::route_policy::channels_compatible;

/// One consistent view of what PipeWire currently shows for the intercom.
#[derive(Clone, Copy)]
pub struct Observed<'a> {
	pub headsets: &'a BTreeMap<Address, Headset>,
	pub host: Option<&'a Headset>,
	pub links: &'a BTreeSet<(u64, u64)>,
}

impl Observed<'_> {
	pub fn status(&self) -> RoutingStatus {
		RoutingStatus::with_host(self.headsets, self.host, self.links)
	}
}

pub struct RoutingStatus {
	counts: BTreeMap<Address, (usize, usize)>,
	host_counts: (usize, usize),
	peers: BTreeSet<(Address, Address)>,
	total: usize,
}

/// Counts the compatible, not yet observed links from `source` to `sink`.
fn observe(
	source: &Headset,
	sink: &Headset,
	links: &BTreeSet<(u64, u64)>,
	observed: &mut BTreeSet<(u64, u64)>,
) -> usize {
	let mut count = 0;
	for output in &source.sources {
		for input in &sink.sinks {
			let link = (output.id, input.id);
			if channels_compatible(output, input) && links.contains(&link) && observed.insert(link)
			{
				count += 1;
			}
		}
	}
	count
}

impl RoutingStatus {
	pub fn new(headsets: &BTreeMap<Address, Headset>, links: &BTreeSet<(u64, u64)>) -> Self {
		Self::with_host(headsets, None, links)
	}

	/// Includes links between the routing computer's wired headset and the
	/// Bluetooth headsets.
	pub fn with_host(
		headsets: &BTreeMap<Address, Headset>,
		host: Option<&Headset>,
		links: &BTreeSet<(u64, u64)>,
	) -> Self {
		let mut status = Self {
			counts: BTreeMap::new(),
			host_counts: (0, 0),
			peers: BTreeSet::new(),
			total: 0,
		};
		let mut observed = BTreeSet::new();
		for (source_address, source) in headsets {
			for (sink_address, sink) in headsets {
				if source_address == sink_address {
					continue;
				}
				let count = observe(source, sink, links, &mut observed);
				if count > 0 {
					status.counts.entry(source_address.clone()).or_default().0 += count;
					status.counts.entry(sink_address.clone()).or_default().1 += count;
					status
						.peers
						.insert((source_address.clone(), sink_address.clone()));
				}
			}
			if let Some(host) = host {
				let heard = observe(source, host, links, &mut observed);
				status.counts.entry(source_address.clone()).or_default().0 += heard;
				status.host_counts.1 += heard;
			}
		}
		if let Some(host) = host {
			for (sink_address, sink) in headsets {
				let spoken = observe(host, sink, links, &mut observed);
				status.counts.entry(sink_address.clone()).or_default().1 += spoken;
				status.host_counts.0 += spoken;
			}
		}
		status.total = observed.len();
		status
	}

	/// Observed outgoing and incoming channel-link counts of the host.
	pub fn host_counts(&self) -> (usize, usize) {
		self.host_counts
	}

	pub fn has_active_host_route(&self) -> bool {
		self.host_counts.0 > 0
	}

	/// Observed outgoing and incoming channel-link counts, respectively.
	pub fn counts(&self, address: &Address) -> (usize, usize) {
		self.counts.get(address).copied().unwrap_or_default()
	}

	pub fn total_links(&self) -> usize {
		self.total
	}

	pub fn has_active_source_route(&self, address: &Address) -> bool {
		self.counts(address).0 > 0
	}

	pub fn has_active_intercom_connection(&self, address: &Address) -> bool {
		self.peers.iter().any(|(source, sink)| {
			source == address && self.peers.contains(&(sink.clone(), source.clone()))
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::pipewire::Port;

	fn address(value: &str) -> Address {
		Address::parse(value).unwrap()
	}

	fn port(id: u64, channel: &str) -> Port {
		Port {
			id,
			node_id: None,
			channel: channel.into(),
		}
	}

	fn headsets() -> BTreeMap<Address, Headset> {
		BTreeMap::from([
			(
				address("AA:BB:CC:DD:EE:01"),
				Headset {
					sources: vec![port(1, "FL")],
					sinks: vec![port(2, "FL")],
				},
			),
			(
				address("AA:BB:CC:DD:EE:02"),
				Headset {
					sources: vec![port(3, "MONO")],
					sinks: vec![port(4, "FL"), port(5, "FR")],
				},
			),
			(
				address("AA:BB:CC:DD:EE:03"),
				Headset {
					sources: vec![port(6, "MONO")],
					sinks: vec![port(7, "MONO")],
				},
			),
		])
	}

	#[test]
	fn counts_and_readiness_ignore_self_incompatible_and_unknown_endpoints() {
		let status = RoutingStatus::new(
			&headsets(),
			&[(1, 2), (1, 5), (1, 99), (99, 2), (1, 4), (3, 2)].into(),
		);
		assert_eq!(status.total_links(), 2);
		assert_eq!(status.counts(&address("AA:BB:CC:DD:EE:01")), (1, 1));
		assert_eq!(status.counts(&address("AA:BB:CC:DD:EE:02")), (1, 1));
		assert_eq!(status.counts(&address("AA:BB:CC:DD:EE:04")), (0, 0));
		assert!(status.has_active_source_route(&address("AA:BB:CC:DD:EE:01")));
		assert!(status.has_active_intercom_connection(&address("AA:BB:CC:DD:EE:01")));
		assert!(!status.has_active_source_route(&address("AA:BB:CC:DD:EE:03")));
		assert!(!status.has_active_intercom_connection(&address("AA:BB:CC:DD:EE:04")));
	}

	#[test]
	fn duplex_readiness_requires_a_common_peer_and_observed_links() {
		let status = RoutingStatus::new(&headsets(), &[(1, 4), (6, 2)].into());
		assert_eq!(status.counts(&address("AA:BB:CC:DD:EE:01")), (1, 1));
		assert!(status.has_active_source_route(&address("AA:BB:CC:DD:EE:01")));
		assert!(!status.has_active_intercom_connection(&address("AA:BB:CC:DD:EE:01")));
		assert_eq!(
			RoutingStatus::new(&headsets(), &BTreeSet::new()).total_links(),
			0
		);
	}

	#[test]
	fn repeated_port_metadata_does_not_inflate_counts() {
		let mut headsets = headsets();
		headsets
			.get_mut(&address("AA:BB:CC:DD:EE:01"))
			.unwrap()
			.sources
			.push(port(1, "FL"));
		let status = RoutingStatus::new(&headsets, &[(1, 4)].into());
		assert_eq!(status.total_links(), 1);
		assert_eq!(status.counts(&address("AA:BB:CC:DD:EE:01")), (1, 0));
	}

	#[test]
	fn host_links_are_counted_for_both_sides_without_self_routes() {
		let host = Headset {
			sources: vec![port(10, "FL")],
			sinks: vec![port(11, "FL"), port(12, "FR")],
		};
		let status = RoutingStatus::with_host(
			&headsets(),
			Some(&host),
			&[(10, 2), (10, 7), (1, 11), (3, 11), (3, 12), (10, 11)].into(),
		);
		assert_eq!(status.host_counts(), (2, 3));
		assert!(status.has_active_host_route());
		assert_eq!(status.total_links(), 5);
		assert_eq!(status.counts(&address("AA:BB:CC:DD:EE:01")), (1, 1));
		assert_eq!(status.counts(&address("AA:BB:CC:DD:EE:02")), (2, 0));
		assert!(!status.has_active_intercom_connection(&address("AA:BB:CC:DD:EE:01")));
		let silent = RoutingStatus::with_host(&headsets(), Some(&host), &[(1, 11)].into());
		assert!(!silent.has_active_host_route());
	}
}
