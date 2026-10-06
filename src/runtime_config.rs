// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Read-only resolution of run options, before persistence or worker startup.

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::groups::{self, TalkGroup};
use crate::network;
use crate::transmit::Mode;
use crate::transport::Transport;
use crate::{PttButton, address, parse_interval, validate_mode, validate_transport};

pub(crate) struct RunOptions {
	pub addresses: Vec<String>,
	pub interval: f64,
	pub connect: bool,
	pub pair_button: bool,
	pub buttons: Vec<PttButton>,
	pub mode: Mode,
	pub transport: Transport,
	pub dashboard: bool,
}

pub(crate) struct RunConfig {
	pub allowed: BTreeSet<String>,
	pub interval: Duration,
	pub connect: bool,
	pub pair_button: bool,
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
		mut options: RunOptions,
		interactive: bool,
		network_path: PathBuf,
	) -> Result<Self, String> {
		validate_transport(options.mode, options.transport)?;
		let interval = Duration::from_secs_f64(parse_interval(&options.interval.to_string())?);
		if options.dashboard && !interactive {
			return Err("--dashboard requires an interactive terminal on stderr".into());
		}
		if options.pair_button && (options.mode != Mode::FullDuplex || !options.buttons.is_empty())
		{
			return Err("--pair-button requires full-duplex without --ptt".into());
		}
		let explicit_network = !options.addresses.is_empty();
		let allowed = if explicit_network {
			options
				.addresses
				.iter()
				.map(|value| address(value))
				.collect::<Result<_, _>>()?
		} else {
			network::load_for_run(&network_path, options.pair_button)?
		};
		crate::ptt_discovery::resolve_pending(&mut options.buttons, &allowed)?;
		for button in &mut options.buttons {
			button.address = address(&button.address)?;
			if button.path.is_empty() {
				return Err("--ptt requires an input device path".into());
			}
		}
		validate_mode(options.mode, &options.buttons, &allowed)?;
		let groups_path = groups::config_path(&network_path)?;
		let groups = groups::load(&groups_path)?;
		Ok(Self {
			allowed,
			interval,
			connect: options.connect,
			pair_button: options.pair_button,
			buttons: options.buttons,
			mode: options.mode,
			transport: options.transport,
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
			pair_button: false,
			buttons: vec![],
			mode: Mode::FullDuplex,
			transport: Transport::Hfp,
			dashboard: false,
		}
	}

	#[test]
	fn invalid_combinations_never_replace_saved_network() {
		let fixture = Fixture::new();
		let path = fixture.network();
		network::save(&path, &[B.into()].into()).unwrap();
		for invalid in 0..8 {
			let mut options = options();
			match invalid {
				0 => options.transport = Transport::ScoA2dp,
				1 => options.mode = Mode::HalfDuplex,
				2 => {
					options.dashboard = true;
				}
				3 => options.interval = f64::NAN,
				4 => options.addresses = vec!["invalid".into()],
				5 => {
					options.pair_button = true;
					options.mode = Mode::HalfDuplex;
				}
				6 => {
					options.buttons = vec![PttButton {
						address: B.into(),
						path: "input".into(),
					}]
				}
				7 => {
					options.pair_button = true;
					options.buttons = vec![PttButton {
						address: A.into(),
						path: "input".into(),
					}];
				}
				_ => unreachable!(),
			}
			assert!(RunConfig::resolve_at(options, false, path.clone()).is_err());
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
		explicit.addresses = vec![A.to_lowercase(), A.into()];
		let config = RunConfig::resolve_at(explicit, true, path.clone()).unwrap();
		assert_eq!(config.allowed, [A.into()].into());
		assert!(config.explicit_network);
		assert!(!path.exists());
		network::save(&path, &[B.into()].into()).unwrap();
		let mut saved = options();
		saved.addresses.clear();
		saved.mode = Mode::HalfDuplex;
		saved.transport = Transport::ScoA2dp;
		saved.buttons = vec![PttButton {
			address: B.into(),
			path: "input".into(),
		}];
		let config = RunConfig::resolve_at(saved, true, path.clone()).unwrap();
		assert_eq!(config.allowed, [B.into()].into());
		assert!(!config.explicit_network);
		assert_eq!(network::load(&path).unwrap(), [B.into()].into());
	}

	#[test]
	fn enrollment_can_start_empty_but_saved_mapping_must_cover_network() {
		let fixture = Fixture::new();
		let path = fixture.network();
		let mut enrollment = options();
		enrollment.addresses.clear();
		enrollment.pair_button = true;
		let config = RunConfig::resolve_at(enrollment, true, path.clone()).unwrap();
		assert!(config.allowed.is_empty());
		assert!(!path.exists());
		network::save(&path, &[A.into(), B.into()].into()).unwrap();
		let mut saved = options();
		saved.addresses.clear();
		saved.buttons = vec![PttButton {
			address: A.into(),
			path: "input".into(),
		}];
		assert!(RunConfig::resolve_at(saved, true, path.clone()).is_err());
		assert_eq!(network::load(&path).unwrap().len(), 2);
	}
}
