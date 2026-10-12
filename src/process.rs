// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Bounded subprocess execution and process-group cleanup.

use std::io::{self, Read};
use std::os::unix::process::CommandExt;
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

pub(crate) const COMMAND_TIMEOUT: Duration = Duration::from_secs(15);
const CHILD_POLL_INTERVAL: Duration = Duration::from_millis(20);
const MAX_CAPTURED_OUTPUT: usize = 1024 * 1024;

fn read_bounded(mut reader: impl Read, limit: usize) -> io::Result<Vec<u8>> {
	let mut captured = Vec::with_capacity(limit.min(8192));
	let mut buffer = [0; 8192];
	let mut exceeded_limit = false;
	loop {
		let count = reader.read(&mut buffer)?;
		if count == 0 {
			break;
		}
		let remaining = limit.saturating_sub(captured.len());
		captured.extend_from_slice(&buffer[..count.min(remaining)]);
		exceeded_limit |= count > remaining;
	}
	if exceeded_limit {
		Err(io::Error::new(
			io::ErrorKind::InvalidData,
			format!("captured output exceeded the {limit}-byte limit"),
		))
	} else {
		Ok(captured)
	}
}

/// Runs a command with captured output and a deadline.
pub(crate) fn command(args: &[&str], timeout: Duration) -> Result<String, String> {
	command_cancellable(args, timeout, None)
}

/// Which process(es) `wait_child` should terminate when the command times
/// out, is cancelled, or errors while polling.
#[derive(Clone, Copy, PartialEq, Eq)]
enum KillMode {
	/// Kill only the direct child (used for interactive children that share
	/// the caller's controlling terminal, where grouping could trigger
	/// `SIGTTIN`/job-control issues).
	Process,
	/// Kill the child's entire process group, releasing pipes held open by
	/// descendants. Only valid for children spawned with `process_group(0)`.
	ProcessGroup,
}

/// Waits for a child process, enforcing its timeout and optional cancellation.
fn wait_child(
	child: &mut Child,
	name: &str,
	timeout: Duration,
	stopped: Option<&AtomicBool>,
	kill_mode: KillMode,
) -> Result<ExitStatus, String> {
	let start = Instant::now();
	loop {
		let cancelled = stopped.is_some_and(|flag| flag.load(Ordering::SeqCst));
		let error = match child.try_wait() {
			Ok(Some(status)) => return Ok(status),
			Ok(None) if cancelled => format!("{name} cancelled"),
			Ok(None) if start.elapsed() >= timeout => format!("{name} timed out"),
			Ok(None) => {
				thread::sleep(CHILD_POLL_INTERVAL);
				continue;
			}
			Err(error) => error.to_string(),
		};
		match kill_mode {
			KillMode::ProcessGroup => kill_process_group(child),
			KillMode::Process => {
				let _ = child.kill();
			}
		}
		let _ = child.wait();
		return Err(error);
	}
}

/// Kills the child's entire process group, so descendants that keep the
/// child's inherited pipes open are also terminated.
pub(crate) fn kill_process_group(child: &mut Child) {
	let pid = child.id() as libc::pid_t;
	if pid <= 0 {
		return;
	}
	// SAFETY: `pid` is the child's own process ID, which was placed in its
	// own process group via `process_group(0)` when spawned, so `-pid`
	// refers to a valid process group led by that child.
	unsafe {
		libc::kill(-pid, libc::SIGKILL);
	}
}

/// Runs a cancellable command, draining both output streams while it executes.
///
/// Commands run in their own process group so timeout or cancellation also
/// terminates descendants that might otherwise keep the captured pipes open.
pub(crate) fn command_cancellable(
	args: &[&str],
	timeout: Duration,
	stopped: Option<&AtomicBool>,
) -> Result<String, String> {
	let Some(name) = args.first().copied().filter(|name| !name.is_empty()) else {
		return Err("command arguments must include a program".into());
	};
	if stopped.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
		return Err(format!("{name} cancelled"));
	}
	log::debug!(
		"Starting {name} ({} arguments, {timeout:?} timeout)",
		args.len() - 1
	);
	let started = Instant::now();
	let mut child = Command::new(name)
		.args(&args[1..])
		.stdin(Stdio::null())
		.stdout(Stdio::piped())
		.stderr(Stdio::piped())
		.process_group(0)
		.spawn()
		.map_err(|e| format!("{name}: {e}"))?;
	let (Some(mut stdout), Some(mut stderr)) = (child.stdout.take(), child.stderr.take()) else {
		kill_process_group(&mut child);
		let _ = child.wait();
		return Err(format!("{name}: failed to capture command output"));
	};
	let output = thread::spawn(move || read_bounded(&mut stdout, MAX_CAPTURED_OUTPUT));
	let error = thread::spawn(move || read_bounded(&mut stderr, MAX_CAPTURED_OUTPUT));
	let status = wait_child(&mut child, name, timeout, stopped, KillMode::ProcessGroup)
		.inspect_err(|error| {
			log::debug!("{name} stopped after {:?}: {error}", started.elapsed())
		})?;
	let stdout = output
		.join()
		.map_err(|_| "stdout reader panicked".to_string())?
		.map_err(|e| format!("stdout: {e}"))?;
	let stderr = error
		.join()
		.map_err(|_| "stderr reader panicked".to_string())?
		.map_err(|e| format!("stderr: {e}"))?;
	log::debug!(
		"{name} exited with {status} after {:?} ({} stdout bytes, {} stderr bytes)",
		started.elapsed(),
		stdout.len(),
		stderr.len()
	);
	if !status.success() {
		return Err(format!(
			"{} exited with {status}: {}",
			name,
			String::from_utf8_lossy(&stderr).trim()
		));
	}
	Ok(String::from_utf8_lossy(&stdout).into_owned())
}

pub(crate) fn wait_interactive(
	child: &mut Child,
	name: &str,
	timeout: Duration,
	stopped: Option<&AtomicBool>,
) -> Result<ExitStatus, String> {
	wait_child(child, name, timeout, stopped, KillMode::Process)
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::sync::Arc;

	#[test]
	fn commands_capture_output_errors_and_do_not_read_stdin() {
		let timeout = Duration::from_secs(5);
		assert_eq!(
			command(&["sh", "-c", "cat; printf output"], timeout).unwrap(),
			"output"
		);
		assert!(
			command(&["sh", "-c", "printf failure >&2; exit 7"], timeout)
				.unwrap_err()
				.contains("failure")
		);
		assert_eq!(
			command(&["head", "-c", "131072", "/dev/zero"], timeout)
				.unwrap()
				.len(),
			131072
		);
	}

	#[test]
	fn output_readers_drain_beyond_the_capture_limit() {
		let bytes = vec![0; 1025];
		let mut reader = std::io::Cursor::new(bytes);
		assert!(read_bounded(&mut reader, 1024).is_err());
		assert_eq!(reader.position(), 1025);
	}

	#[test]
	fn commands_reject_stdout_and_stderr_over_the_capture_limit() {
		let size = (MAX_CAPTURED_OUTPUT + 1).to_string();
		let stdout_error =
			command(&["head", "-c", &size, "/dev/zero"], Duration::from_secs(5)).unwrap_err();
		assert!(stdout_error.contains("stdout: captured output exceeded"));

		let stderr_error = command(
			&["sh", "-c", &format!("head -c {size} /dev/zero >&2")],
			Duration::from_secs(5),
		)
		.unwrap_err();
		assert!(stderr_error.contains("stderr: captured output exceeded"));
	}

	#[test]
	fn commands_reject_missing_program_names() {
		assert!(command(&[], Duration::from_secs(1)).is_err());
		assert!(command(&[""], Duration::from_secs(1)).is_err());
	}

	#[test]
	fn commands_time_out_and_can_be_cancelled() {
		assert!(
			command(&["sleep", "30"], Duration::from_millis(20))
				.unwrap_err()
				.contains("timed out")
		);
		let stopped = Arc::new(AtomicBool::new(false));
		let cancelled = Arc::clone(&stopped);
		let worker = thread::spawn(move || {
			command_cancellable(&["sleep", "30"], Duration::from_secs(60), Some(&cancelled))
		});
		thread::sleep(Duration::from_millis(50));
		stopped.store(true, Ordering::SeqCst);
		assert!(worker.join().unwrap().unwrap_err().contains("cancelled"));
	}

	#[test]
	fn timeout_does_not_wait_for_descendants_holding_output_pipes() {
		let start = Instant::now();
		assert!(
			command(&["sh", "-c", "sleep 2 & wait"], Duration::from_millis(20))
				.unwrap_err()
				.contains("timed out")
		);
		assert!(start.elapsed() < Duration::from_secs(1));
	}
}
