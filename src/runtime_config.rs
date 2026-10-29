// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Read-only resolution of run options, before persistence or worker startup.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::bluez::Address;
use crate::groups::{self, TalkGroup};
use crate::network;
use crate::ptt::PttButton;
use crate::transmit::Mode;
use crate::transport::{self, Transport};

pub(crate) fn parse_interval(value: &str) -> Result<f64, String> {
	let interval = value
		.parse::<f64>()
		.map_err(|_| "--interval must be a positive finite number")?;
	if !interval.is_finite() || interval <= 0.0 || interval >= u64::MAX as f64 {
		return Err("--interval must be a positive finite number".into());
	}
	if interval < 1.0 {
		return Err("--interval must be at least 1 second".into());
	}
	Ok(interval)
}

pub(crate) fn validate_ptt(
	buttons: &[PttButton],
	allowed: &BTreeSet<Address>,
) -> Result<(), String> {
	let mut configured = BTreeSet::new();
	let mut paths = BTreeSet::new();
	for button in buttons {
		let Some(address) = &button.address else {
			return Err("--ptt auto must be resolved before validating headset mappings".into());
		};
		if (!allowed.is_empty() && !allowed.contains(address))
			|| !configured.insert(address.clone())
		{
			return Err("--ptt requires one unique button mapping per listed headset".into());
		}
		if !paths.insert(&button.path) {
			return Err("--ptt requires a separate input device for each headset".into());
		}
	}
	if !buttons.is_empty() && !allowed.is_empty() && configured != *allowed {
		return Err("--ptt requires a button mapping for every listed headset".into());
	}
	Ok(())
}

pub(crate) fn validate_mode(
	mode: Mode,
	buttons: &[PttButton],
	allowed: &BTreeSet<Address>,
) -> Result<(), String> {
	validate_ptt(buttons, allowed)?;
	if mode == Mode::HalfDuplex && buttons.is_empty() {
		return Err("--mode half-duplex requires --ptt for every headset".into());
	}
	Ok(())
}

pub(crate) struct RunOptions {
	pub addresses: Vec<Address>,
	pub interval: f64,
	pub connect: bool,
	pub sco_limit: usize,
	pub buttons: Vec<PttButton>,
	pub mode: Option<Mode>,
	pub transport: Option<Transport>,
	pub dashboard: bool,
}

pub(crate) struct RunConfig {
	pub allowed: BTreeSet<Address>,
	pub interval: Duration,
	pub connect: bool,
	pub sco_limit: usize,
	pub buttons: Vec<PttButton>,
	pub mode: Mode,
	pub transport: Transport,
	pub dashboard: bool,
	pub network_path: PathBuf,
	pub explicit_network: bool,
	pub groups_path: PathBuf,
	pub groups: Vec<TalkGroup>,
}

impl RunConfig {
	pub fn resolve(options: RunOptions, interactive: bool) -> Result<Self, String> {
		Self::resolve_at(options, interactive, network::headset_path()?)
	}

	fn resolve_at(
		options: RunOptions,
		interactive: bool,
		network_path: PathBuf,
	) -> Result<Self, String> {
		Self::resolve_at_with_ptt(
			options,
			interactive,
			network_path,
			crate::ptt_discovery::resolve_pending,
		)
	}

	fn resolve_at_with_ptt(
		mut options: RunOptions,
		interactive: bool,
		network_path: PathBuf,
		resolve_ptt: impl FnOnce(&mut Vec<PttButton>, &BTreeSet<Address>) -> Result<(), String>,
	) -> Result<Self, String> {
		let mode = options.mode.unwrap_or(Mode::HalfDuplex);
		let transport = options.transport.unwrap_or(if mode == Mode::HalfDuplex {
			Transport::ScoA2dp
		} else {
			Transport::Hfp
		});
		transport::validate_transport(mode, transport)?;
		if options.sco_limit == 0 {
			return Err("--sco-limit must be at least 1".into());
		}
		let interval = Duration::from_secs_f64(parse_interval(&options.interval.to_string())?);
		if options.dashboard && !interactive {
			return Err("--dashboard requires an interactive terminal on stderr".into());
		}
		let explicit_network = !options.addresses.is_empty();
		let allowed = if explicit_network {
			options.addresses.iter().cloned().collect()
		} else {
			network::load(&network_path)?
		};
		if options.buttons.is_empty() {
			options.buttons.push(PttButton {
				address: None,
				path: crate::ptt_discovery::AUTO.into(),
			});
		}
		resolve_ptt(&mut options.buttons, &allowed)?;
		for button in &options.buttons {
			if button.path.is_empty() {
				return Err("--ptt requires an input device path".into());
			}
		}
		validate_mode(mode, &options.buttons, &allowed)?;
		let groups_path = groups::config_path(&network_path)?;
		let groups = groups::load(&groups_path)?;
		Ok(Self {
			allowed,
			interval,
			connect: options.connect,
			sco_limit: options.sco_limit,
			buttons: options.buttons,
			mode,
			transport,
			dashboard: options.dashboard,
			network_path,
			explicit_network,
			groups_path,
			groups,
		})
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::fs;
	use std::sync::atomic::{AtomicUsize, Ordering};

	const A: &str = "AA:BB:CC:DD:EE:01";
	const B: &str = "AA:BB:CC:DD:EE:02";

	struct Fixture(PathBuf);

	impl Fixture {
		fn new() -> Self {
			static NEXT: AtomicUsize = AtomicUsize::new(0);
			let path = std::env::temp_dir().join(format!(
				"run-config-{}-{}",
				std::process::id(),
				NEXT.fetch_add(1, Ordering::Relaxed)
			));
			fs::create_dir_all(&path).unwrap();
			Self(path)
		}

		fn network(&self) -> PathBuf {
			self.0.join("headsets")
		}
	}

	impl Drop for Fixture {
		fn drop(&mut self) {
			fs::remove_dir_all(&self.0).unwrap();
		}
	}

	fn options() -> RunOptions {
		RunOptions {
			addresses: vec![A.into()],
			interval: 2.0,
			connect: false,
			sco_limit: 1,
			buttons: vec![PttButton {
				address: Some(A.into()),
				path: "input".into(),
			}],
			mode: Some(Mode::FullDuplex),
			transport: Some(Transport::Hfp),
			dashboard: false,
		}
	}

	#[test]
	fn invalid_combinations_never_replace_saved_network() {
		let fixture = Fixture::new();
		let path = fixture.network();
		network::save(&path, &[B.into()].into()).unwrap();
		for invalid in 0..6 {
			let mut options = options();
			match invalid {
				0 => options.transport = Some(Transport::ScoA2dp),
				1 => options.dashboard = true,
				2 => options.interval = f64::NAN,
				3 => {
					options.addresses.clear();
					options.buttons.clear();
					options.mode = Some(Mode::HalfDuplex);
				}
				4 => {
					options.buttons = vec![PttButton {
						address: Some(B.into()),
						path: "input".into(),
					}]
				}
				5 => options.sco_limit = 0,
				_ => unreachable!(),
			}
			assert!(
				RunConfig::resolve_at(options, false, path.clone()).is_err(),
				"invalid combination {invalid} should fail"
			);
			assert_eq!(network::load(&path).unwrap(), [B.into()].into());
		}
	}

	#[test]
	fn malformed_groups_do_not_create_or_replace_network() {
		let fixture = Fixture::new();
		let path = fixture.network();
		fs::write(groups::config_path(&path).unwrap(), "{invalid").unwrap();
		assert!(RunConfig::resolve_at(options(), true, path.clone()).is_err());
		assert!(!path.exists());
		network::save(&path, &[B.into()].into()).unwrap();
		assert!(RunConfig::resolve_at(options(), true, path.clone()).is_err());
		assert_eq!(network::load(&path).unwrap(), [B.into()].into());
	}

	#[test]
	fn explicit_and_saved_networks_resolve_without_writes() {
		let fixture = Fixture::new();
		let path = fixture.network();
		let mut explicit = options();
		explicit.addresses = vec![
			Address::parse(&A.to_lowercase()).unwrap(),
			Address::parse(A).unwrap(),
		];
		let config = RunConfig::resolve_at(explicit, true, path.clone()).unwrap();
		assert_eq!(config.allowed, [A.into()].into());
		assert!(config.explicit_network);
		assert!(!path.exists());
		network::save(&path, &[B.into()].into()).unwrap();
		let mut saved = options();
		saved.addresses.clear();
		saved.mode = Some(Mode::HalfDuplex);
		saved.transport = Some(Transport::ScoA2dp);
		saved.buttons = vec![PttButton {
			address: Some(B.into()),
			path: "input".into(),
		}];
		let config = RunConfig::resolve_at(saved, true, path.clone()).unwrap();
		assert_eq!(config.allowed, [B.into()].into());
		assert!(!config.explicit_network);
		assert_eq!(network::load(&path).unwrap(), [B.into()].into());
	}

	#[test]
	fn saved_mapping_must_cover_network() {
		let fixture = Fixture::new();
		let path = fixture.network();
		network::save(&path, &[A.into(), B.into()].into()).unwrap();
		let mut saved = options();
		saved.addresses.clear();
		saved.buttons = vec![PttButton {
			address: Some(A.into()),
			path: "input".into(),
		}];
		assert!(RunConfig::resolve_at(saved, true, path.clone()).is_err());
		assert_eq!(network::load(&path).unwrap().len(), 2);
	}

	#[test]
	fn missing_ptt_mappings_default_to_automatic_discovery() {
		let fixture = Fixture::new();
		let mut options = options();
		options.addresses = vec![A.into(), B.into()];
		options.buttons.clear();
		let config =
			RunConfig::resolve_at_with_ptt(options, true, fixture.network(), |buttons, allowed| {
				assert_eq!(buttons.len(), 1);
				assert_eq!(buttons[0].address, None);
				assert_eq!(buttons[0].path, crate::ptt_discovery::AUTO);
				buttons.clear();
				buttons.extend(
					allowed
						.iter()
						.enumerate()
						.map(|(index, address)| PttButton {
							address: Some(address.clone()),
							path: format!("input{index}"),
						}),
				);
				Ok(())
			})
			.unwrap();
		assert_eq!(config.buttons.len(), 2);
		assert!(
			config
				.buttons
				.iter()
				.all(|button| button.path.starts_with("input"))
		);
	}

	#[test]
	fn mode_and_transport_default_to_half_duplex_sco_a2dp() {
		let fixture = Fixture::new();
		let mut unset = options();
		unset.mode = None;
		unset.transport = None;
		let config = RunConfig::resolve_at(unset, true, fixture.network()).unwrap();
		assert_eq!(config.mode, Mode::HalfDuplex);
		assert_eq!(config.transport, Transport::ScoA2dp);

		let mut full = options();
		full.mode = Some(Mode::FullDuplex);
		full.transport = None;
		let config = RunConfig::resolve_at(full, true, fixture.network()).unwrap();
		assert_eq!(config.transport, Transport::Hfp);
	}

	#[test]
	fn routing_interval_must_be_at_least_one_second() {
		assert!(parse_interval("0.999").is_err());
		assert_eq!(parse_interval("1").unwrap(), 1.0);
	}
}
