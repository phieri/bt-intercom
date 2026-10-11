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
use std::sync::mpsc::{self, Receiver, Sender, TryRecvError};
use std::time::{Duration, Instant};

use crate::network::runtime_directory;
use crate::process::command_cancellable;
use crate::router::Headset;
use crate::routing_status::RoutingStatus;
use crate::transmit::{Mode, Transmit};
use crate::worker::Worker;

pub(crate) const BEEP_WAV: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/ptt-beep.wav"));

pub(crate) const KEY_PLAYPAUSE: u16 = 164;

#[derive(Clone, Debug)]
pub(crate) struct PttButton {
	pub address: String,
	pub path: String,
}

pub(crate) type PttEvent = Result<(String, bool, Instant), String>;

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
			Duration::from_secs(2),
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
	headsets: &BTreeMap<String, Headset>,
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
	routed_headsets: &mut BTreeSet<String>,
	headsets: &BTreeMap<String, Headset>,
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
			Ok(Ok((address, pressed, at))) => {
				changed |= transmit.event(&address, pressed, at);
			}
			Ok(Err(error)) => return Err(error),
			Err(TryRecvError::Empty) => return Ok(changed),
			Err(TryRecvError::Disconnected) => return Err("PTT input closed".into()),
		}
	}
}

#[cfg(test)]
pub(crate) fn read_input_events<R: Read>(input: R, address: String, sender: Sender<PttEvent>) {
	let (_wake, receiver) = mpsc::channel();
	read_events(input, address, sender, &AtomicBool::new(false), &receiver);
}

pub(crate) struct PreparedInputs(Vec<(PttButton, File)>);

pub(crate) struct PttInput {
	pub events: Receiver<PttEvent>,
	workers: Vec<Worker>,
}

impl PreparedInputs {
	pub fn open(buttons: &[PttButton]) -> Result<Self, String> {
		buttons
			.iter()
			.map(|button| {
				OpenOptions::new()
					.read(true)
					.custom_flags(libc::O_NONBLOCK)
					.open(&button.path)
					.map(|file| (button.clone(), file))
					.map_err(|error| format!("PTT input {}: {error}", button.path))
			})
			.collect::<Result<Vec<_>, _>>()
			.map(Self)
	}

	pub fn start(self, stopped: Arc<AtomicBool>) -> PttInput {
		let (sender, events) = mpsc::channel();
		let workers = self
			.0
			.into_iter()
			.map(|(button, file)| {
				let sender = sender.clone();
				Worker::spawn(Arc::clone(&stopped), move |stopped, wake| {
					read_events(file, button.address, sender, &stopped, &wake);
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

/// `input` must not block: real devices are opened with O_NONBLOCK.
/// Preserve the prefix of a record across short reads and WouldBlock.
pub(crate) fn read_events<R: Read>(
	mut input: R,
	address: String,
	sender: Sender<PttEvent>,
	stopped: &AtomicBool,
	wake: &Receiver<()>,
) {
	let mut pressed = false;
	let offset = std::mem::size_of::<libc::timeval>();
	let mut event = vec![0; offset + 8];
	let mut filled = 0;
	while !stopped.load(Ordering::SeqCst) {
		match input.read(&mut event[filled..]) {
			Ok(0) => {
				if !stopped.load(Ordering::SeqCst) {
					let _ = sender.send(Err(format!(
						"PTT input for {address} closed: unexpected end of input"
					)));
				}
				return;
			}
			Ok(count) => filled += count,
			Err(error) if error.kind() == ErrorKind::Interrupted => continue,
			Err(error) if error.kind() == ErrorKind::WouldBlock => {
				if !matches!(
					wake.recv_timeout(Duration::from_millis(20)),
					Err(mpsc::RecvTimeoutError::Timeout)
				) {
					return;
				}
				continue;
			}
			Err(error) => {
				if !stopped.load(Ordering::SeqCst) {
					let _ = sender.send(Err(format!("PTT input for {address} closed: {error}")));
				}
				return;
			}
		}
		if filled != event.len() {
			continue;
		}
		filled = 0;
		if stopped.load(Ordering::SeqCst) {
			return;
		}
		let at = Instant::now();
		let event_type = u16::from_ne_bytes([event[offset], event[offset + 1]]);
		let event_key = u16::from_ne_bytes([event[offset + 2], event[offset + 3]]);
		let value = i32::from_ne_bytes(event[offset + 4..offset + 8].try_into().unwrap());
		if event_type == 0 && event_key == 3 {
			let _ = sender.send(Err(format!("PTT input for {address} lost button events")));
			return;
		}
		if event_type == 1 && event_key == KEY_PLAYPAUSE && (value == 0 || value == 1) {
			let next = value == 1;
			if next != pressed {
				pressed = next;
				if sender.send(Ok((address.clone(), pressed, at))).is_err() {
					return;
				}
			}
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::Write;
	use std::os::unix::net::UnixStream;

	fn reader() -> (UnixStream, Receiver<PttEvent>, Worker) {
		let (input, output) = UnixStream::pair().unwrap();
		input.set_nonblocking(true).unwrap();
		let (sender, events) = mpsc::channel();
		let worker = Worker::spawn(Arc::new(AtomicBool::new(false)), move |stopped, wake| {
			read_events(input, "headset".into(), sender, &stopped, &wake);
		});
		(output, events, worker)
	}

	fn record(pressed: bool) -> Vec<u8> {
		let mut record = vec![0; std::mem::size_of::<libc::timeval>()];
		record.extend(1u16.to_ne_bytes());
		record.extend(KEY_PLAYPAUSE.to_ne_bytes());
		record.extend(i32::from(pressed).to_ne_bytes());
		record
	}

	#[test]
	fn idle_and_partial_readers_are_joined_on_cancellation() {
		for prefix in [0, 1, record(true).len() - 1] {
			let (mut writer, events, mut worker) = reader();
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
		let (mut writer, events, worker) = reader();
		for pressed in [true, false] {
			let record = record(pressed);
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
			assert_eq!(address, "headset");
			assert_eq!(actual, pressed);
		}
		drop(worker);
		assert!(events.recv().is_err());
	}
}
