// Copyright (C) 2026 Philip Eriksson. All rights reserved.

//! Atomic replacement of configuration files.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};

static NEXT_TEMP_FILE: AtomicUsize = AtomicUsize::new(0);

pub(crate) fn write(path: &Path, contents: &[u8]) -> io::Result<()> {
	let mut temporary = None;
	for _ in 0..100 {
		let mut name = path.as_os_str().to_os_string();
		name.push(format!(
			".{}.{}.tmp",
			std::process::id(),
			NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
		));
		let candidate = PathBuf::from(name);
		match OpenOptions::new()
			.write(true)
			.create_new(true)
			.open(&candidate)
		{
			Ok(file) => {
				temporary = Some((candidate, file));
				break;
			}
			Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
			Err(error) => return Err(error),
		}
	}
	let (temporary_path, mut file) = temporary.ok_or_else(|| {
		io::Error::new(
			io::ErrorKind::AlreadyExists,
			"could not create a unique temporary file",
		)
	})?;

	let result = (|| {
		file.write_all(contents)?;
		file.sync_all()?;
		fs::rename(&temporary_path, path)
	})();
	if result.is_err() {
		let _ = fs::remove_file(temporary_path);
	}
	result
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn atomically_replaces_file_contents() {
		let directory = std::env::temp_dir().join(format!(
			"bt-intercom-atomic-file-{}-{}",
			std::process::id(),
			NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
		));
		fs::create_dir_all(&directory).unwrap();
		let path = directory.join("config");
		fs::write(&path, b"old").unwrap();

		write(&path, b"new").unwrap();

		assert_eq!(fs::read(&path).unwrap(), b"new");
		assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);
		fs::remove_dir_all(directory).unwrap();
	}

	#[test]
	fn removes_temporary_file_when_replacement_fails() {
		let directory = std::env::temp_dir().join(format!(
			"bt-intercom-atomic-file-failure-{}-{}",
			std::process::id(),
			NEXT_TEMP_FILE.fetch_add(1, Ordering::Relaxed)
		));
		fs::create_dir_all(&directory).unwrap();
		let path = directory.join("config");
		fs::create_dir(&path).unwrap();

		assert!(write(&path, b"new").is_err());
		assert!(path.is_dir());
		assert_eq!(fs::read_dir(&directory).unwrap().count(), 1);

		fs::remove_dir_all(directory).unwrap();
	}
}
