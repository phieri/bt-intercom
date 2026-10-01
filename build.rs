use std::env;
use std::fs;
use std::path::PathBuf;

fn main() {
    let out_dir = PathBuf::from(env::var_os("OUT_DIR").expect("Cargo did not set OUT_DIR"));
    fs::write(out_dir.join("ptt-beep.wav"), double_beep_wav())
        .expect("could not write PTT beep WAV");
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
