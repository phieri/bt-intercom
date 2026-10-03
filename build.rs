// Copyright (C) 2026 Philip Eriksson. All rights reserved.

use std::env;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn main() {
	println!("cargo:rerun-if-env-changed=SOURCE_DATE_EPOCH");
	println!("cargo:rerun-if-changed=Cargo.toml");
	println!("cargo:rerun-if-changed=Cargo.lock");
	println!("cargo:rerun-if-changed=src");
	let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"));
	fs::write(out_dir.join("ptt-beep.wav"), double_beep_wav())
		.expect("could not write PTT beep WAV");
	println!(
		"cargo:rustc-env=BT_INTERCOM_BUILD_DATETIME={}",
		build_datetime()
	);
}

fn build_datetime() -> String {
	let timestamp = match env::var("SOURCE_DATE_EPOCH") {
		Ok(value) => value
			.parse::<u64>()
			.expect("SOURCE_DATE_EPOCH must be a non-negative integer"),
		Err(_) => SystemTime::now()
			.duration_since(UNIX_EPOCH)
			.expect("system clock is before the Unix epoch")
			.as_secs(),
	};
	let days = i64::try_from(timestamp / 86_400).expect("build timestamp is out of range");
	let seconds = timestamp % 86_400;
	let z = days + 719_468;
	let era = if z >= 0 { z } else { z - 146_096 } / 146_097;
	let day_of_era = z - era * 146_097;
	let year_of_era =
		(day_of_era - day_of_era / 1_460 + day_of_era / 36_524 - day_of_era / 146_096) / 365;
	let mut year = year_of_era + era * 400;
	let day_of_year = day_of_era - (365 * year_of_era + year_of_era / 4 - year_of_era / 100);
	let month_prime = (5 * day_of_year + 2) / 153;
	let day = day_of_year - (153 * month_prime + 2) / 5 + 1;
	let month = month_prime + if month_prime < 10 { 3 } else { -9 };
	year += i64::from(month <= 2);
	format!(
		"{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}Z",
		seconds / 3_600,
		seconds % 3_600 / 60,
		seconds % 60
	)
}

fn double_beep_wav() -> Vec<u8> {
	let sample_rate = 48_000_u32;
	let beep_samples = sample_rate * 90 / 1_000;
	let gap_samples = sample_rate * 75 / 1_000;
	let sample_count = beep_samples * 2 + gap_samples;
	let mut samples = Vec::with_capacity(sample_count as usize * 2);
	let fade_samples = sample_rate / 200;
	for index in 0..sample_count {
		let position = index % (beep_samples + gap_samples);
		let sample = if position < beep_samples {
			let fade_in = position as f32 / fade_samples as f32;
			let fade_out = (beep_samples - position - 1) as f32 / fade_samples as f32;
			let envelope = fade_in.min(fade_out).min(1.0);
			let phase = std::f32::consts::TAU * 880.0 * position as f32 / sample_rate as f32;
			(phase.sin() * envelope * 0.2 * i16::MAX as f32) as i16
		} else {
			0
		};
		samples.extend_from_slice(&sample.to_le_bytes());
	}

	let mut wav = Vec::with_capacity(44 + samples.len());
	wav.extend_from_slice(b"RIFF");
	wav.extend_from_slice(&(36 + samples.len() as u32).to_le_bytes());
	wav.extend_from_slice(b"WAVEfmt ");
	wav.extend_from_slice(&16_u32.to_le_bytes());
	wav.extend_from_slice(&1_u16.to_le_bytes());
	wav.extend_from_slice(&1_u16.to_le_bytes());
	wav.extend_from_slice(&sample_rate.to_le_bytes());
	wav.extend_from_slice(&(sample_rate * 2).to_le_bytes());
	wav.extend_from_slice(&2_u16.to_le_bytes());
	wav.extend_from_slice(&16_u16.to_le_bytes());
	wav.extend_from_slice(b"data");
	wav.extend_from_slice(&(samples.len() as u32).to_le_bytes());
	wav.extend_from_slice(&samples);
	wav
}
