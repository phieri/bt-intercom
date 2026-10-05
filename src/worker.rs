// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Owned workers with cancellation, interruptible waits and joined teardown.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, Sender};
use std::thread::{self, JoinHandle};

pub(crate) struct Worker {
	stopped: Arc<AtomicBool>,
	wake: Sender<()>,
	handle: Option<JoinHandle<()>>,
}

impl Worker {
	pub(crate) fn spawn(
		stopped: Arc<AtomicBool>,
		task: impl FnOnce(Arc<AtomicBool>, Receiver<()>) + Send + 'static,
	) -> Self {
		let (wake, receiver) = mpsc::channel();
		let cancelled = Arc::clone(&stopped);
		Self {
			stopped,
			wake,
			handle: Some(thread::spawn(move || task(cancelled, receiver))),
		}
	}

	pub(crate) fn stop(&mut self) {
		self.stopped.store(true, Ordering::SeqCst);
		let _ = self.wake.send(());
		if let Some(handle) = self.handle.take()
			&& handle.join().is_err()
		{
			log::warn!("Background worker panicked");
		}
	}
}

impl Drop for Worker {
	fn drop(&mut self) {
		self.stop();
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::time::Duration;

	#[test]
	fn stop_cancels_wakes_and_joins_idempotently() {
		let stopped = Arc::new(AtomicBool::new(false));
		let (ready, started) = mpsc::channel();
		let (done, completed) = mpsc::channel();
		let mut worker = Worker::spawn(Arc::clone(&stopped), move |cancelled, wake| {
			ready.send(()).unwrap();
			wake.recv_timeout(Duration::from_secs(30)).unwrap();
			assert!(cancelled.load(Ordering::SeqCst));
			done.send(()).unwrap();
		});
		started.recv_timeout(Duration::from_secs(2)).unwrap();
		worker.stop();
		assert!(stopped.load(Ordering::SeqCst));
		completed.try_recv().unwrap();
		worker.stop();
	}

	#[test]
	fn drop_joins_on_early_return() {
		let (done, completed) = mpsc::channel();
		let fail = || -> Result<(), ()> {
			let _worker = Worker::spawn(Arc::new(AtomicBool::new(false)), move |_, wake| {
				let _ = wake.recv();
				done.send(()).unwrap();
			});
			Err(())
		};
		assert!(fail().is_err());
		completed.try_recv().unwrap();
	}
}
