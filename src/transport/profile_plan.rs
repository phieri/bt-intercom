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

pub(super) fn plan_profile_changes(
	sources: &BTreeSet<Address>,
	snapshot: &Snapshot,
	devices: &BTreeMap<Address, Device>,
) -> ProfilePlan {
	let demotions: Vec<_> = devices
		.iter()
		.filter(|(address, device)| {
			!sources.contains(*address) && !device.is_profile(ProfileFamily::A2dp)
		})
		.map(|(address, _)| ProfileAction::SelectProfile {
			address: address.clone(),
			family: ProfileFamily::A2dp,
		})
		.collect();
	if !demotions.is_empty() {
		return ProfilePlan::Apply(demotions);
	}
	if devices
		.iter()
		.any(|(address, device)| !sources.contains(address) && snapshot.has_hfp_node(device.id))
	{
		return ProfilePlan::Wait;
	}
	let promotions: Vec<_> = sources
		.iter()
		.filter_map(|address| {
			let device = &devices[address];
			(!device.is_profile(ProfileFamily::Hfp)).then(|| ProfileAction::SelectProfile {
				address: address.clone(),
				family: ProfileFamily::Hfp,
			})
		})
		.collect();
	if promotions.is_empty() {
		ProfilePlan::Ready
	} else {
		ProfilePlan::Apply(promotions)
	}
}
