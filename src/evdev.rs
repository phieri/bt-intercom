// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Framing of Linux `input_event` records read from non-blocking evdev devices.

use std::fs::{File, OpenOptions};
use std::io::{self, ErrorKind, Read};
use std::os::unix::fs::OpenOptionsExt;

pub(crate) const EV_SYN: u16 = 0;
pub(crate) const EV_KEY: u16 = 1;
pub(crate) const SYN_REPORT: u16 = 0;
pub(crate) const SYN_DROPPED: u16 = 3;

/// Opens an event device for polling without blocking.
pub(crate) fn open_nonblocking(path: &str) -> io::Result<File> {
	OpenOptions::new()
		.read(true)
		.custom_flags(libc::O_NONBLOCK)
		.open(path)
}

/// How to fix the most common reason an event device cannot be opened.
pub(crate) fn access_hint(error: &io::Error) -> &'static str {
	if error.kind() == ErrorKind::PermissionDenied {
		" (add the user to the `input` group or fix udev permissions, then log in again)"
	} else {
		""
	}
}

/// The meaningful part of an `input_event`: its type, code and value.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct InputEvent {
	pub(crate) kind: u16,
	pub(crate) code: u16,
	pub(crate) value: i32,
}

/// Reassembles records across short reads and `WouldBlock`.
pub(crate) struct EventReader {
	record: Vec<u8>,
	filled: usize,
	offset: usize,
}

impl EventReader {
	pub(crate) fn new() -> Self {
		// The kernel's input_event timestamp is two C longs on every ABI, which
		// can differ from libc::timeval (64-bit time_t on 32-bit targets).
		let offset = 2 * std::mem::size_of::<libc::c_ulong>();
		Self {
			record: vec![0; offset + 8],
			filled: 0,
			offset,
		}
	}

	/// Returns the next complete event, or `None` when the device has no more
	/// data for now. A partially read record is kept for the next call.
	pub(crate) fn next<R: Read>(&mut self, input: &mut R) -> io::Result<Option<InputEvent>> {
		loop {
			match input.read(&mut self.record[self.filled..]) {
				Ok(0) => {
					return Err(io::Error::new(
						ErrorKind::UnexpectedEof,
						"unexpected end of input",
					));
				}
				Ok(count) => self.filled += count,
				Err(error) if error.kind() == ErrorKind::Interrupted => continue,
				Err(error) if error.kind() == ErrorKind::WouldBlock => return Ok(None),
				Err(error) => return Err(error),
			}
			if self.filled == self.record.len() {
				self.filled = 0;
				let at = self.offset;
				return Ok(Some(InputEvent {
					kind: u16::from_ne_bytes([self.record[at], self.record[at + 1]]),
					code: u16::from_ne_bytes([self.record[at + 2], self.record[at + 3]]),
					value: i32::from_ne_bytes(self.record[at + 4..at + 8].try_into().unwrap()),
				}));
			}
		}
	}
}

#[cfg(test)]
pub(crate) fn record(kind: u16, code: u16, value: i32) -> Vec<u8> {
	let mut record = vec![0; 2 * std::mem::size_of::<libc::c_ulong>()];
	record.extend(kind.to_ne_bytes());
	record.extend(code.to_ne_bytes());
	record.extend(value.to_ne_bytes());
	record
}

#[cfg(test)]
mod tests {
	use super::*;

	struct Chunks(Vec<io::Result<Vec<u8>>>);

	impl Read for Chunks {
		fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
			if self.0.is_empty() {
				return Err(ErrorKind::WouldBlock.into());
			}
			self.0.remove(0).map(|bytes| {
				buffer[..bytes.len()].copy_from_slice(&bytes);
				bytes.len()
			})
		}
	}

	#[test]
	fn records_survive_short_reads_and_would_block() {
		let bytes = record(EV_KEY, 57, 1);
		let mut input = Chunks(vec![
			Ok(bytes[..5].to_vec()),
			Err(ErrorKind::Interrupted.into()),
			Err(ErrorKind::WouldBlock.into()),
			Ok(bytes[5..].to_vec()),
		]);
		let mut reader = EventReader::new();
		assert_eq!(reader.next(&mut input).unwrap(), None);
		assert_eq!(
			reader.next(&mut input).unwrap(),
			Some(InputEvent {
				kind: EV_KEY,
				code: 57,
				value: 1
			})
		);
		assert_eq!(reader.next(&mut input).unwrap(), None);
	}

	#[test]
	fn end_of_input_is_an_error() {
		let mut input = Chunks(vec![Ok(vec![])]);
		let error = EventReader::new().next(&mut input).unwrap_err();
		assert_eq!(error.kind(), ErrorKind::UnexpectedEof);
	}
}
