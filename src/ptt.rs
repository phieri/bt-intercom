// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Supervised, cancellable readers for mapped evdev play/pause buttons.

use std::fs::{File, OpenOptions};
use std::io::{ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::time::{Duration, Instant};

use crate::worker::Worker;
use crate::{KEY_PLAYPAUSE, PttButton, PttEvent};

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
