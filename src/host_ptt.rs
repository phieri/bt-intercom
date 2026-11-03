// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! The routing computer's spacebar as a true push-to-talk.
//!
//! Unlike a headset's play/pause taps, a keyboard reports both key-down and
//! key-up, so the host transmits exactly while a space key is held. Keyboards
//! are read through evdev and are not grabbed: the keystrokes still reach the
//! focused application.

use std::fs::File;
use std::io::Read;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender, SyncSender};
use std::time::{Duration, Instant};

use crate::evdev::{
	EV_KEY, EV_SYN, EventReader, InputEvent, SYN_DROPPED, SYN_REPORT, access_hint, open_nonblocking,
};
use crate::ptt::PttEvent;
use crate::transmit::{Talker, Tap};
use crate::worker::Worker;

pub(crate) const KEY_ENTER: u16 = 28;
pub(crate) const KEY_A: u16 = 30;
pub(crate) const KEY_SPACE: u16 = 57;

const POLL_INTERVAL: Duration = Duration::from_millis(10);
const RESCAN_INTERVAL: Duration = Duration::from_secs(2);
const WAIT_WARNING_DELAY: Duration = Duration::from_secs(10);
const WAIT_WARNING_REPEAT: Duration = Duration::from_secs(60);

/// Where the host's keyboards come from.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Keyboards {
	/// Every attached keyboard, including ones plugged in later.
	Auto,
	Path(String),
}

impl Keyboards {
	pub(crate) fn parse(value: &str) -> Result<Self, String> {
		match value {
			crate::ptt_discovery::AUTO => Ok(Self::Auto),
			"" => Err("--host-keyboard requires an input device path or `auto`".into()),
			path => Ok(Self::Path(path.into())),
		}
	}
}

/// Whether a space key is held on one keyboard.
///
/// After `SYN_DROPPED` the kernel's key state is unknown, so the key counts as
/// released and the rest of that report is ignored. Auto-repeat or the next
/// press then restores it; a missed release can therefore never latch the
/// microphone open.
#[derive(Debug, Default)]
struct SpaceKey {
	down: bool,
	discarding: bool,
}

impl SpaceKey {
	fn apply(&mut self, event: InputEvent) {
		match (event.kind, event.code) {
			(EV_SYN, SYN_DROPPED) => {
				self.down = false;
				self.discarding = true;
			}
			(EV_SYN, SYN_REPORT) => self.discarding = false,
			(EV_KEY, KEY_SPACE) if !self.discarding => self.down = event.value != 0,
			_ => {}
		}
	}
}

struct Keyboard<R> {
	path: String,
	input: R,
	reader: EventReader,
	space: SpaceKey,
}

impl<R: Read> Keyboard<R> {
	fn new(path: String, input: R) -> Self {
		Self {
			path,
			input,
			reader: EventReader::new(),
			space: SpaceKey::default(),
		}
	}

	/// Applies every pending event; an error means the device is gone.
	fn drain(&mut self) -> Result<(), String> {
		loop {
			match self.reader.next(&mut self.input) {
				Ok(Some(event)) => self.space.apply(event),
				Ok(None) => return Ok(()),
				Err(error) => return Err(error.to_string()),
			}
		}
	}
}

/// Drains all keyboards and reports whether any space key is held. Keyboards
/// that fail are dropped, which releases their key.
fn poll<R: Read>(keyboards: &mut Vec<Keyboard<R>>) -> bool {
	keyboards.retain_mut(|keyboard| match keyboard.drain() {
		Ok(()) => true,
		Err(error) => {
			log::warn!(
				"Host keyboard {} lost: {error}; waiting for it to reappear",
				keyboard.path
			);
			false
		}
	});
	keyboards.iter().any(|keyboard| keyboard.space.down)
}

/// Keyboards opened before any worker or persisted state exists.
pub(crate) struct PreparedKeyboards {
	source: Keyboards,
	opened: Vec<Keyboard<File>>,
}

impl PreparedKeyboards {
	/// An explicit device must open; automatic discovery only warns, since a
	/// keyboard may be attached later.
	pub(crate) fn open(source: &Keyboards) -> Result<Self, String> {
		let opened = match source {
			Keyboards::Path(path) => {
				vec![open(path).map_err(|error| format!("host keyboard {path}: {error}"))?]
			}
			Keyboards::Auto => attach_new(&mut Vec::new()),
		};
		Ok(Self {
			source: source.clone(),
			opened,
		})
	}

	pub(crate) fn start(
		self,
		stopped: Arc<AtomicBool>,
		sender: Sender<PttEvent>,
		session_wake: SyncSender<()>,
	) -> Worker {
		let source = self.source;
		Worker::spawn(stopped, move |stopped, wake| {
			supervise(
				self.opened,
				|keyboards| attach(&source, keyboards),
				&sender,
				&stopped,
				&wake,
				&session_wake,
			);
		})
	}
}

fn open(path: &str) -> Result<Keyboard<File>, String> {
	open_nonblocking(path)
		.map(|file| Keyboard::new(path.to_owned(), file))
		.map_err(|error| format!("{error}{}", access_hint(&error)))
}

/// Opens every discovered keyboard that is not already attached.
fn attach_new(attached: &mut [Keyboard<File>]) -> Vec<Keyboard<File>> {
	let mut opened = Vec::new();
	for path in crate::ptt_discovery::find_keyboards() {
		if attached.iter().any(|keyboard| keyboard.path == path) {
			continue;
		}
		match open(&path) {
			Ok(keyboard) => {
				log::info!("Host push-to-talk keyboard attached at {path}");
				opened.push(keyboard);
			}
			Err(error) => log::warn!("Host keyboard {path}: {error}"),
		}
	}
	opened
}

fn attach(source: &Keyboards, keyboards: &mut Vec<Keyboard<File>>) {
	match source {
		Keyboards::Auto => {
			let new = attach_new(keyboards);
			keyboards.extend(new);
		}
		Keyboards::Path(path) if keyboards.is_empty() => {
			if let Ok(keyboard) = open(path) {
				log::info!("Host push-to-talk keyboard attached at {path}");
				keyboards.push(keyboard);
			}
		}
		Keyboards::Path(_) => {}
	}
}

/// Keeps the host's keyboards attached through `attach` (called periodically)
/// and turns the held state of the space key into press/release events.
fn supervise<R: Read>(
	opened: Vec<Keyboard<R>>,
	mut attach: impl FnMut(&mut Vec<Keyboard<R>>),
	sender: &Sender<PttEvent>,
	stopped: &AtomicBool,
	wake: &Receiver<()>,
	session_wake: &SyncSender<()>,
) {
	let mut keyboards = opened;
	let mut talking = false;
	let started = Instant::now();
	let mut last_scan: Option<Instant> = None;
	let mut last_warning: Option<Instant> = None;
	while !stopped.load(Ordering::SeqCst) {
		if last_scan.is_none_or(|at| at.elapsed() >= RESCAN_INTERVAL) {
			last_scan = Some(Instant::now());
			attach(&mut keyboards);
			if keyboards.is_empty() {
				let overdue = last_warning.map_or_else(
					|| started.elapsed() >= WAIT_WARNING_DELAY,
					|at| at.elapsed() >= WAIT_WARNING_REPEAT,
				);
				if overdue {
					log::warn!(
						"Host push-to-talk is unavailable: no keyboard found; attach one or pass --host-keyboard /dev/input/eventX"
					);
					last_warning = Some(Instant::now());
				}
			}
		}
		let pressed = poll(&mut keyboards);
		if pressed != talking {
			talking = pressed;
			let tap = if pressed { Tap::Play } else { Tap::Pause };
			if sender
				.send(Ok((Talker::Host, tap, Instant::now())))
				.is_err()
			{
				return;
			}
			let _ = session_wake.try_send(());
		}
		if !matches!(
			wake.recv_timeout(POLL_INTERVAL),
			Err(mpsc::RecvTimeoutError::Timeout)
		) {
			return;
		}
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::evdev::record;
	use std::io::Write;
	use std::os::unix::net::UnixStream;

	fn key(value: i32) -> Vec<u8> {
		record(EV_KEY, KEY_SPACE, value)
	}

	fn device() -> (UnixStream, Keyboard<UnixStream>) {
		let (input, output) = UnixStream::pair().unwrap();
		input.set_nonblocking(true).unwrap();
		(output, Keyboard::new("test".into(), input))
	}

	#[test]
	fn space_is_held_from_press_to_release_across_repeats() {
		let (mut writer, keyboard) = device();
		let mut keyboards = vec![keyboard];
		assert!(!poll(&mut keyboards));
		writer.write_all(&key(1)).unwrap();
		assert!(poll(&mut keyboards));
		writer.write_all(&key(2)).unwrap();
		assert!(poll(&mut keyboards));
		writer.write_all(&key(0)).unwrap();
		assert!(!poll(&mut keyboards));
	}

	#[test]
	fn other_keys_never_press_the_host() {
		let (mut writer, keyboard) = device();
		let mut keyboards = vec![keyboard];
		for code in [
			KEY_ENTER,
			KEY_A,
			crate::ptt::KEY_PLAYCD,
			crate::ptt::KEY_PLAYPAUSE,
		] {
			writer.write_all(&record(EV_KEY, code, 1)).unwrap();
		}
		assert!(!poll(&mut keyboards));
	}

	#[test]
	fn any_held_space_key_keeps_the_host_transmitting() {
		let (mut first_writer, first) = device();
		let (mut second_writer, second) = device();
		let mut keyboards = vec![first, second];
		first_writer.write_all(&key(1)).unwrap();
		second_writer.write_all(&key(1)).unwrap();
		first_writer.write_all(&key(0)).unwrap();
		assert!(poll(&mut keyboards));
		second_writer.write_all(&key(0)).unwrap();
		assert!(!poll(&mut keyboards));
	}

	#[test]
	fn dropped_events_release_the_key_until_it_is_seen_down_again() {
		let (mut writer, keyboard) = device();
		let mut keyboards = vec![keyboard];
		writer.write_all(&key(1)).unwrap();
		assert!(poll(&mut keyboards));
		writer.write_all(&record(EV_SYN, SYN_DROPPED, 0)).unwrap();
		writer.write_all(&key(1)).unwrap();
		assert!(!poll(&mut keyboards));
		writer.write_all(&record(EV_SYN, SYN_REPORT, 0)).unwrap();
		writer.write_all(&key(2)).unwrap();
		assert!(poll(&mut keyboards));
	}

	#[test]
	fn a_lost_keyboard_releases_its_key() {
		let (mut writer, keyboard) = device();
		let mut keyboards = vec![keyboard];
		writer.write_all(&key(1)).unwrap();
		assert!(poll(&mut keyboards));
		drop(writer);
		assert!(!poll(&mut keyboards));
		assert!(keyboards.is_empty());
	}

	#[test]
	fn worker_reports_press_and_release_and_stops_promptly() {
		let (mut writer, keyboard) = device();
		let (sender, events) = mpsc::channel();
		let (session_wake, session_events) = mpsc::sync_channel(1);
		let mut pending = Some(keyboard);
		let mut worker = Worker::spawn(Arc::new(AtomicBool::new(false)), move |stopped, wake| {
			supervise(
				Vec::new(),
				|keyboards| keyboards.extend(pending.take()),
				&sender,
				&stopped,
				&wake,
				&session_wake,
			);
		});
		assert!(events.recv_timeout(Duration::from_millis(50)).is_err());
		for (value, expected) in [(1, Tap::Play), (2, Tap::Play), (0, Tap::Pause)] {
			writer.write_all(&key(value)).unwrap();
			if value == 2 {
				continue;
			}
			let (talker, tap, _) = events
				.recv_timeout(Duration::from_secs(1))
				.unwrap()
				.unwrap();
			assert_eq!((talker, tap), (Talker::Host, expected));
			session_events.recv_timeout(Duration::from_secs(1)).unwrap();
		}
		let started = Instant::now();
		worker.stop();
		assert!(started.elapsed() < Duration::from_secs(1));
		assert!(events.try_recv().is_err());
	}

	#[test]
	fn parses_keyboard_selection() {
		assert_eq!(Keyboards::parse("auto"), Ok(Keyboards::Auto));
		assert_eq!(
			Keyboards::parse("/dev/input/event3"),
			Ok(Keyboards::Path("/dev/input/event3".into()))
		);
		assert!(Keyboards::parse("").is_err());
	}
}
