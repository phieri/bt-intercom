// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Supervised, cancellable readers for mapped evdev play/pause buttons.

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::fs::{self, File, OpenOptions};
use std::io::{ErrorKind, Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender, TryRecvError};
use std::time::{Duration, Instant};

use crate::bluez::Address;
use crate::network::runtime_directory;
use crate::process::command_cancellable;
use crate::router::Headset;
use crate::routing_status::RoutingStatus;
use crate::transmit::{Mode, Tap, Transmit};
use crate::worker::Worker;

/// Upper bound for playing the short PTT confirmation tone.
const BEEP_PLAY_TIMEOUT: Duration = Duration::from_secs(2);

/// How long a headset may lack a button device before the user is warned, and
/// how often the warning repeats.
const PTT_WAIT_WARNING_DELAY: Duration = Duration::from_secs(10);
const PTT_WAIT_WARNING_REPEAT: Duration = Duration::from_secs(60);

pub(crate) const BEEP_WAV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ptt-beep.wav"));

pub(crate) const KEY_PLAYPAUSE: u16 = 164;
pub(crate) const KEY_PLAYCD: u16 = 200;
pub(crate) const KEY_PAUSECD: u16 = 201;

/// BlueZ maps AVRCP PLAY/PAUSE passthrough to KEY_PLAYCD/KEY_PAUSECD and never
/// emits KEY_PLAYPAUSE; no MPRIS player is needed for these events.
pub(crate) const PLAY_PAUSE_KEYS: [u16; 3] = [KEY_PLAYPAUSE, KEY_PLAYCD, KEY_PAUSECD];

#[derive(Clone, Debug)]
pub(crate) struct PttButton {
	pub address: Option<Address>,
	pub path: String,
}

pub(crate) type PttEvent = Result<(Address, Tap, Instant), String>;

pub(crate) struct PttBeep {
	path: PathBuf,
}

impl PttBeep {
	pub(crate) fn new() -> Result<Self, String> {
		let runtime_dir = runtime_directory(env::var_os("XDG_RUNTIME_DIR").as_deref());
		let nonce = std::time::SystemTime::now()
			.duration_since(std::time::UNIX_EPOCH)
			.unwrap_or_default()
			.as_nanos();
		let path = runtime_dir.join(format!(
			"bt-intercom-ptt-{}-{nonce}.wav",
			std::process::id()
		));
		let mut file = OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&path)
			.map_err(|error| format!("could not create PTT beep: {error}"))?;
		if let Err(error) = file.write_all(BEEP_WAV) {
			drop(file);
			let _ = fs::remove_file(&path);
			return Err(format!("could not write PTT beep: {error}"));
		}
		Ok(Self { path })
	}

	pub(crate) fn play(&self, speaker_node: u64, stopped: &AtomicBool) -> Result<(), String> {
		let target = format!("--target={speaker_node}");
		let path = self.path.to_string_lossy().into_owned();
		command_cancellable(
			&["pw-play", &target, &path],
			BEEP_PLAY_TIMEOUT,
			Some(stopped),
		)
		.map(|_| ())
	}
}

impl Drop for PttBeep {
	fn drop(&mut self) {
		let _ = fs::remove_file(&self.path);
	}
}

pub(crate) fn confirm_transmissions(
	mode: Mode,
	transmit: &mut Transmit,
	input: &Receiver<PttEvent>,
	headsets: &BTreeMap<Address, Headset>,
	links: &BTreeSet<(u64, u64)>,
	beep: Option<&PttBeep>,
	stopped: &AtomicBool,
) -> Result<bool, String> {
	let status = RoutingStatus::new(headsets, links);
	let ready = transmit
		.pending
		.iter()
		.filter_map(|address| {
			let headset = headsets.get(address)?;
			(transmit.sources().contains(address) && status.has_active_source_route(address))
				.then(|| (address.clone(), headset.speaker_node()))
		})
		.collect::<Vec<_>>();
	for (address, speaker_node) in ready {
		if drain_events(input, transmit)? || stopped.load(Ordering::SeqCst) {
			return Ok(true);
		}
		let Some(speaker_node) = speaker_node else {
			let error = format!("could not play PTT confirmation for {address}: no speaker node");
			if mode == Mode::HalfDuplex {
				return Err(error);
			}
			log::warn!("{error}");
			continue;
		};
		let result = beep
			.ok_or_else(|| "confirmation beep is unavailable".to_string())
			.and_then(|beep| beep.play(speaker_node, stopped));
		match result {
			Ok(()) => {
				transmit.pending.remove(&address);
			}
			Err(_error) if stopped.load(Ordering::SeqCst) => return Ok(true),
			Err(error) if mode == Mode::HalfDuplex => {
				return Err(format!(
					"could not play PTT confirmation for {address}: {error}"
				));
			}
			Err(error) => {
				log::warn!("Could not play PTT confirmation for {address}: {error}");
			}
		}
	}
	Ok(false)
}

pub(crate) fn confirm_connections(
	routed_headsets: &mut BTreeSet<Address>,
	headsets: &BTreeMap<Address, Headset>,
	links: &BTreeSet<(u64, u64)>,
	beep: Option<&PttBeep>,
	stopped: &AtomicBool,
) {
	let status = RoutingStatus::new(headsets, links);
	let current: BTreeSet<_> = headsets
		.keys()
		.filter(|address| status.has_active_intercom_connection(address))
		.cloned()
		.collect();
	for address in current.difference(routed_headsets) {
		let Some(speaker_node) = headsets[address].speaker_node() else {
			log::warn!("Could not play intercom connection beep for {address}: no speaker node");
			continue;
		};
		if let Some(beep) = beep
			&& let Err(error) = beep.play(speaker_node, stopped)
		{
			log::warn!("Could not play intercom connection beep for {address}: {error}");
		}
	}
	*routed_headsets = current;
}

/// Drain timestamped transitions before selecting sources and again after slow
/// routing work, so a released/cancelled request never earns a stale beep.
pub(crate) fn drain_events(
	input: &Receiver<PttEvent>,
	transmit: &mut Transmit,
) -> Result<bool, String> {
	let mut changed = false;
	loop {
		match input.try_recv() {
			Ok(Ok((address, tap, at))) => {
				changed |= transmit.tap(&address, tap, at);
			}
			Ok(Err(error)) => return Err(error),
			Err(TryRecvError::Empty) => return Ok(changed),
			Err(TryRecvError::Disconnected) => return Err("PTT input closed".into()),
		}
	}
}

#[cfg(test)]
pub(crate) fn read_input_events<R: Read>(input: R, address: Address, sender: Sender<PttEvent>) {
	let (_wake, receiver) = mpsc::channel();
	read_events(
		input,
		address,
		sender,
		&AtomicBool::new(false),
		&receiver,
		None,
	);
}

/// A mapping whose device is `None` is still waiting for discovery to find it.
pub(crate) struct PreparedInputs(Vec<(PttButton, Option<File>)>);

pub(crate) struct PttInput {
	pub events: Receiver<PttEvent>,
	workers: Vec<Worker>,
}

impl PreparedInputs {
	pub fn open(buttons: &[PttButton]) -> Result<Self, String> {
		buttons
			.iter()
			.map(|button| {
				if button.path == crate::ptt_discovery::AUTO {
					return Ok((button.clone(), None));
				}
				OpenOptions::new()
					.read(true)
					.custom_flags(libc::O_NONBLOCK)
					.open(&button.path)
					.map(|file| (button.clone(), Some(file)))
					.map_err(|error| {
						let hint = if error.kind() == ErrorKind::PermissionDenied {
							" (add the user to the `input` group or fix udev permissions, then log in again)"
						} else {
							""
						};
						format!("PTT input {}: {error}{hint}", button.path)
					})
			})
			.collect::<Result<Vec<_>, _>>()
			.map(Self)
	}

	pub fn start(self, stopped: Arc<AtomicBool>, session_wake: SyncSender<()>) -> PttInput {
		let (sender, events) = mpsc::channel();
		let workers = self
			.0
			.into_iter()
			.map(|(button, file)| {
				let sender = sender.clone();
				let session_wake = session_wake.clone();
				Worker::spawn(Arc::clone(&stopped), move |stopped, wake| {
					let Some(address) = button.address else {
						return;
					};
					supervise_events(
						file,
						&button.path,
						address,
						sender,
						&stopped,
						&wake,
						&session_wake,
					);
				})
			})
			.collect();
		PttInput { events, workers }
	}
}

impl PttInput {
	#[cfg(test)]
	pub(crate) fn from_events(events: Receiver<PttEvent>) -> Self {
		Self {
			events,
			workers: vec![],
		}
	}

	pub fn stop(&mut self) {
		for worker in &mut self.workers {
			worker.stop();
		}
	}
}

/// Keep reading a headset's button across loss of its input device. BlueZ
/// destroys and recreates the AVRCP uinput device when the headset reconnects
/// or AVRCP restarts (e.g. around profile switches), which invalidates the
/// open descriptor. Rediscover the device by address instead of failing.
fn supervise_events(
	file: Option<File>,
	path: &str,
	address: Address,
	sender: Sender<PttEvent>,
	stopped: &AtomicBool,
	wake: &Receiver<()>,
	session_wake: &SyncSender<()>,
) {
	let mut file = file;
	let mut path = path.to_string();
	let mut waiting_since = Instant::now();
	let mut last_warning: Option<Instant> = None;
	while !stopped.load(Ordering::SeqCst) {
		let Some(input) = file.take() else {
			match reopen(&address) {
				Ok((new_path, input)) => {
					log::info!("PTT input for {address} attached at {new_path}");
					path = new_path;
					file = Some(input);
					last_warning = None;
				}
				Err(error) => {
					let overdue = last_warning.map_or_else(
						|| waiting_since.elapsed() >= PTT_WAIT_WARNING_DELAY,
						|at| at.elapsed() >= PTT_WAIT_WARNING_REPEAT,
					);
					if overdue {
						log::warn!("PTT button of {address} is unavailable: {error}");
						last_warning = Some(Instant::now());
					}
				}
			}
			if file.is_some() {
				continue;
			}
			if !matches!(
				wake.recv_timeout(Duration::from_millis(500)),
				Err(mpsc::RecvTimeoutError::Timeout)
			) {
				return;
			}
			continue;
		};
		let Err(reason) = read_loop(input, &address, &sender, stopped, wake, Some(session_wake))
		else {
			return;
		};
		if stopped.load(Ordering::SeqCst) {
			return;
		}
		log::warn!("{reason} ({path}); waiting for the device to reappear");
		waiting_since = Instant::now();
	}
}

fn reopen(address: &Address) -> Result<(String, File), String> {
	let path = crate::ptt_discovery::find(address)?;
	let file = OpenOptions::new()
		.read(true)
		.custom_flags(libc::O_NONBLOCK)
		.open(&path)
		.map_err(|error| format!("{path}: {error}"))?;
	Ok((path, file))
}

/// `input` must not block: real devices are opened with O_NONBLOCK.
#[cfg(test)]
pub(crate) fn read_events<R: Read>(
	input: R,
	address: Address,
	sender: Sender<PttEvent>,
	stopped: &AtomicBool,
	wake: &Receiver<()>,
	session_wake: Option<&SyncSender<()>>,
) {
	if let Err(error) = read_loop(input, &address, &sender, stopped, wake, session_wake)
		&& !stopped.load(Ordering::SeqCst)
	{
		send_event(&sender, session_wake, Err(error));
	}
}

/// Preserve the prefix of a record across short reads and WouldBlock. Returns
/// `Ok` when stopped or the session is gone, `Err` when the input was lost.
fn read_loop<R: Read>(
	mut input: R,
	address: &Address,
	sender: &Sender<PttEvent>,
	stopped: &AtomicBool,
	wake: &Receiver<()>,
	session_wake: Option<&SyncSender<()>>,
) -> Result<(), String> {
	// The kernel's input_event timestamp is two C longs on every ABI, which can
	// differ from libc::timeval (64-bit time_t on 32-bit targets).
	let offset = 2 * std::mem::size_of::<libc::c_ulong>();
	let mut event = vec![0; offset + 8];
	let mut filled = 0;
	while !stopped.load(Ordering::SeqCst) {
		match input.read(&mut event[filled..]) {
			Ok(0) => {
				return Err(format!(
					"PTT input for {address} closed: unexpected end of input"
				));
			}
			Ok(count) => filled += count,
			Err(error) if error.kind() == ErrorKind::Interrupted => continue,
			Err(error) if error.kind() == ErrorKind::WouldBlock => {
				if !matches!(
					wake.recv_timeout(Duration::from_millis(20)),
					Err(mpsc::RecvTimeoutError::Timeout)
				) {
					return Ok(());
				}
				continue;
			}
			Err(error) => return Err(format!("PTT input for {address} closed: {error}")),
		}
		if filled != event.len() {
			continue;
		}
		filled = 0;
		if stopped.load(Ordering::SeqCst) {
			return Ok(());
		}
		let at = Instant::now();
		let event_type = u16::from_ne_bytes([event[offset], event[offset + 1]]);
		let event_key = u16::from_ne_bytes([event[offset + 2], event[offset + 3]]);
		let value = i32::from_ne_bytes(event[offset + 4..offset + 8].try_into().unwrap());
		if event_type == 0 && event_key == 3 {
			// SYN_DROPPED: later taps still arrive intact, so keep reading.
			log::warn!("PTT input for {address} dropped events; continuing");
			continue;
		}
		// Headsets send each AVRCP command as an instant press and release, so
		// only the key-down carries meaning; releases and repeats are ignored.
		if event_type == 1
			&& value == 1
			&& let Some(tap) = tap_for_key(event_key)
			&& !send_event(sender, session_wake, Ok((address.clone(), tap, at)))
		{
			return Ok(());
		}
	}
	Ok(())
}

fn tap_for_key(key: u16) -> Option<Tap> {
	match key {
		KEY_PLAYCD => Some(Tap::Play),
		KEY_PAUSECD => Some(Tap::Pause),
		KEY_PLAYPAUSE => Some(Tap::Toggle),
		_ => None,
	}
}

fn send_event(sender: &Sender<PttEvent>, wake: Option<&SyncSender<()>>, event: PttEvent) -> bool {
	if sender.send(event).is_err() {
		return false;
	}
	if let Some(wake) = wake {
		let _ = wake.try_send(());
	}
	true
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Write;
	use std::os::unix::net::UnixStream;

	fn reader() -> (
		UnixStream,
		Receiver<PttEvent>,
		Receiver<()>,
		SyncSender<()>,
		Worker,
	) {
		let (input, output) = UnixStream::pair().unwrap();
		input.set_nonblocking(true).unwrap();
		let (sender, events) = mpsc::channel();
		let (session_wake, session_events) = mpsc::sync_channel(1);
		let worker_wake = session_wake.clone();
		let worker = Worker::spawn(Arc::new(AtomicBool::new(false)), move |stopped, wake| {
			read_events(
				input,
				Address::parse("AA:BB:CC:DD:EE:01").unwrap(),
				sender,
				&stopped,
				&wake,
				Some(&worker_wake),
			);
		});
		(output, events, session_events, session_wake, worker)
	}

	fn key_record(key: u16, value: i32) -> Vec<u8> {
		let mut record = vec![0; std::mem::size_of::<libc::timeval>()];
		record.extend(1u16.to_ne_bytes());
		record.extend(key.to_ne_bytes());
		record.extend(value.to_ne_bytes());
		record
	}

	fn record(pressed: bool) -> Vec<u8> {
		key_record(KEY_PLAYPAUSE, i32::from(pressed))
	}

	#[test]
	fn idle_and_partial_readers_are_joined_on_cancellation() {
		for prefix in [0, 1, record(true).len() - 1] {
			let (mut writer, events, _, _, mut worker) = reader();
			writer.write_all(&record(true)[..prefix]).unwrap();
			assert!(matches!(
				events.recv_timeout(Duration::from_millis(50)),
				Err(mpsc::RecvTimeoutError::Timeout)
			));
			let started = Instant::now();
			worker.stop();
			assert!(started.elapsed() < Duration::from_secs(1));
			assert!(matches!(
				events.try_recv(),
				Err(mpsc::TryRecvError::Disconnected)
			));
		}
	}

	#[test]
	fn split_records_survive_would_block_without_losing_transitions() {
		let (mut writer, events, session_events, _, worker) = reader();
		for _ in 0..2 {
			let record = record(true);
			writer.write_all(&record[..3]).unwrap();
			assert!(matches!(
				events.recv_timeout(Duration::from_millis(50)),
				Err(mpsc::RecvTimeoutError::Timeout)
			));
			writer.write_all(&record[3..]).unwrap();
			let (address, actual, _) = events
				.recv_timeout(Duration::from_secs(1))
				.unwrap()
				.unwrap();
			assert_eq!(address.as_str(), "AA:BB:CC:DD:EE:01");
			assert_eq!(actual, Tap::Toggle);
			session_events.recv_timeout(Duration::from_secs(1)).unwrap();
		}
		drop(worker);
		assert!(events.recv().is_err());
	}

	#[test]
	fn ptt_wake_notifications_coalesce_while_events_remain_queued() {
		let (mut writer, events, session_events, wake_sender, worker) = reader();
		wake_sender.try_send(()).unwrap();
		for (key, value) in [(KEY_PLAYCD, 1), (KEY_PLAYCD, 0), (KEY_PAUSECD, 1)] {
			writer.write_all(&key_record(key, value)).unwrap();
		}

		for expected in [Tap::Play, Tap::Pause] {
			let (_, tap, _) = events
				.recv_timeout(Duration::from_secs(1))
				.unwrap()
				.unwrap();
			assert_eq!(tap, expected);
		}
		drop(worker);
		assert!(session_events.try_recv().is_ok());
		assert!(matches!(
			session_events.try_recv(),
			Err(mpsc::TryRecvError::Empty)
		));
	}
}
