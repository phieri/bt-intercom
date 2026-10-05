//! Metrics and readiness derived only from observed, valid inter-headset links.

use std::collections::{BTreeMap, BTreeSet};

use crate::router::{Headset, channels_compatible};

pub struct RoutingStatus {
	counts: BTreeMap<String, (usize, usize)>,
	peers: BTreeSet<(String, String)>,
	total: usize,
}

impl RoutingStatus {
	pub fn new(headsets: &BTreeMap<String, Headset>, links: &BTreeSet<(u64, u64)>) -> Self {
		let mut status = Self {
			counts: BTreeMap::new(),
			peers: BTreeSet::new(),
			total: 0,
		};
		let mut observed = BTreeSet::new();
		for (source_address, source) in headsets {
			for (sink_address, sink) in headsets {
				if source_address == sink_address {
					continue;
				}

				for output in &source.sources {
					for input in &sink.sinks {
						let link = (output.id, input.id);
						if channels_compatible(output, input)
							&& links.contains(&link)
							&& observed.insert(link)
						{
							status.counts.entry(source_address.clone()).or_default().0 += 1;
							status.counts.entry(sink_address.clone()).or_default().1 += 1;
							status
								.peers
								.insert((source_address.clone(), sink_address.clone()));
						}
					}
				}
			}
		}
		status.total = observed.len();
		status
	}

	/// Observed outgoing and incoming channel-link counts, respectively.
	pub fn counts(&self, address: &str) -> (usize, usize) {
		self.counts.get(address).copied().unwrap_or_default()
	}

	pub fn total_links(&self) -> usize {
		self.total
	}

	pub fn has_active_source_route(&self, address: &str) -> bool {
		self.counts(address).0 > 0
	}

	pub fn has_active_intercom_connection(&self, address: &str) -> bool {
		self.peers.iter().any(|(source, sink)| {
			source == address && self.peers.contains(&(sink.clone(), source.clone()))
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::router::Port;

	fn port(id: u64, channel: &str) -> Port {
		Port {
			id,
			node_id: None,
			channel: channel.into(),
		}
	}

	fn headsets() -> BTreeMap<String, Headset> {
		BTreeMap::from([
			(
				"A".into(),
				Headset {
					sources: vec![port(1, "FL")],
					sinks: vec![port(2, "FL")],
				},
			),
			(
				"B".into(),
				Headset {
					sources: vec![port(3, "MONO")],
					sinks: vec![port(4, "FL"), port(5, "FR")],
				},
			),
			(
				"C".into(),
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
		assert_eq!(status.counts("A"), (1, 1));
		assert_eq!(status.counts("B"), (1, 1));
		assert_eq!(status.counts("missing"), (0, 0));
		assert!(status.has_active_source_route("A"));
		assert!(status.has_active_intercom_connection("A"));
		assert!(!status.has_active_source_route("C"));
		assert!(!status.has_active_intercom_connection("missing"));
	}

	#[test]
	fn duplex_readiness_requires_a_common_peer_and_observed_links() {
		let status = RoutingStatus::new(&headsets(), &[(1, 4), (6, 2)].into());
		assert_eq!(status.counts("A"), (1, 1));
		assert!(status.has_active_source_route("A"));
		assert!(!status.has_active_intercom_connection("A"));
		assert_eq!(
			RoutingStatus::new(&headsets(), &BTreeSet::new()).total_links(),
			0
		);
	}

	#[test]
	fn repeated_port_metadata_does_not_inflate_counts() {
		let mut headsets = headsets();
		headsets.get_mut("A").unwrap().sources.push(port(1, "FL"));
		let status = RoutingStatus::new(&headsets, &[(1, 4)].into());
		assert_eq!(status.total_links(), 1);
		assert_eq!(status.counts("A"), (1, 0));
	}
}
