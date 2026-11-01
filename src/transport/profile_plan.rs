// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Pure profile-transition planning from one PipeWire observation.

use std::collections::{BTreeMap, BTreeSet};

use crate::bluez::Address;
use crate::pipewire::Snapshot;
use crate::transport::Device;
use crate::transport_model::ProfileFamily;

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ProfileAction {
	SelectProfile {
		address: Address,
		family: ProfileFamily,
	},
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum ProfilePlan {
	Wait,
	Apply(Vec<ProfileAction>),
	Ready,
}

/// Target profile of every connected headset. Talkers need HFP. Listeners
/// stream A2DP up to the controller's A2DP budget; further listeners keep HFP
/// (SCO) so a controller can serve `a2dp_limit + sco_limit` headsets. Listeners
/// already on A2DP keep it, which avoids needless profile switches.
pub(super) fn assign_profiles(
	sources: &BTreeSet<Address>,
	devices: &BTreeMap<Address, Device>,
	a2dp_limit: usize,
) -> BTreeMap<Address, ProfileFamily> {
	let mut targets = BTreeMap::new();
	let mut listeners: BTreeMap<&str, Vec<&Address>> = BTreeMap::new();
	for (address, device) in devices {
		if sources.contains(address) {
			targets.insert(address.clone(), ProfileFamily::Hfp);
		} else {
			listeners
				.entry(device.controller.as_str())
				.or_default()
				.push(address);
		}
	}
	for mut addresses in listeners.into_values() {
		addresses.sort_by_key(|address| !devices[*address].is_profile(ProfileFamily::A2dp));
		for (index, address) in addresses.into_iter().enumerate() {
			let family = if index < a2dp_limit.max(1) {
				ProfileFamily::A2dp
			} else {
				ProfileFamily::Hfp
			};
			targets.insert(address.clone(), family);
		}
	}
	targets
}

pub(super) fn plan_profile_changes(
	sources: &BTreeSet<Address>,
	snapshot: &Snapshot,
	devices: &BTreeMap<Address, Device>,
	a2dp_limit: usize,
) -> ProfilePlan {
	let targets = assign_profiles(sources, devices, a2dp_limit);
	let demotions: Vec<_> = devices
		.iter()
		.filter(|(address, device)| {
			targets[*address] == ProfileFamily::A2dp && !device.is_profile(ProfileFamily::A2dp)
		})
		.map(|(address, _)| ProfileAction::SelectProfile {
			address: address.clone(),
			family: ProfileFamily::A2dp,
		})
		.collect();
	if !demotions.is_empty() {
		return ProfilePlan::Apply(demotions);
	}
	// An A2DP listener's lingering HFP node holds the SCO link only of its own
	// controller, so it blocks promotions on that controller alone.
	let lingering: BTreeSet<_> = devices
		.iter()
		.filter(|(address, device)| {
			targets[*address] == ProfileFamily::A2dp && snapshot.has_hfp_node(device.id)
		})
		.map(|(_, device)| device.controller.as_str())
		.collect();
	let promotions: Vec<_> = devices
		.iter()
		.filter(|(address, device)| {
			targets[*address] == ProfileFamily::Hfp
				&& !device.is_profile(ProfileFamily::Hfp)
				&& !lingering.contains(device.controller.as_str())
		})
		.map(|(address, _)| ProfileAction::SelectProfile {
			address: address.clone(),
			family: ProfileFamily::Hfp,
		})
		.collect();
	if promotions.is_empty() && !lingering.is_empty() {
		return ProfilePlan::Wait;
	}
	if promotions.is_empty() {
		ProfilePlan::Ready
	} else {
		ProfilePlan::Apply(promotions)
	}
}
