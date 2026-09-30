mod router;

use std::collections::BTreeSet;
use std::env;
use std::io::Read;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use router::Router;

fn command(args: &[&str], timeout: Duration) -> Result<String, String> {
    let mut child = Command::new(args[0])
        .args(&args[1..])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("{}: {e}", args[0]))?;
    let mut stdout = child.stdout.take().unwrap();
    let mut stderr = child.stderr.take().unwrap();
    let output = thread::spawn(move || {
        let mut bytes = Vec::new();
        stdout.read_to_end(&mut bytes).map(|_| bytes)
    });
    let error = thread::spawn(move || {
        let mut bytes = Vec::new();
        stderr.read_to_end(&mut bytes).map(|_| bytes)
    });
    let start = Instant::now();
    let status = loop {
        match child.try_wait() {
            Ok(Some(status)) => break Ok(status),
            Ok(None) if start.elapsed() >= timeout => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(format!("{} timed out", args[0]));
            }
            Ok(None) => thread::sleep(Duration::from_millis(20)),
            Err(e) => {
                let _ = child.kill();
                let _ = child.wait();
                break Err(e.to_string());
            }
        }
    };
    let stdout = output
        .join()
        .map_err(|_| "stdout reader panicked".to_string())?
        .map_err(|e| e.to_string())?;
    let stderr = error
        .join()
        .map_err(|_| "stderr reader panicked".to_string())?
        .map_err(|e| e.to_string())?;
    let status = status?;
    if !status.success() {
        return Err(format!(
            "{} exited with {status}: {}",
            args[0],
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

fn address(value: &str) -> Result<String, String> {
    if value.len() != 17
        || value.split(':').count() != 6
        || !value
            .split(':')
            .all(|part| part.len() == 2 && part.bytes().all(|b| b.is_ascii_hexdigit()))
    {
        return Err(format!(
            "expected a Bluetooth MAC address (XX:XX:XX:XX:XX:XX): {value}"
        ));
    }
    Ok(value.to_ascii_uppercase())
}

fn paired(info: &str) -> bool {
    info.lines().any(|line| {
        line.trim()
            .split_once(':')
            .is_some_and(|(key, value)| key.trim() == "Paired" && value.trim() == "yes")
    })
}

fn usage() -> &'static str {
    "Usage: rpi-intercom scan [--seconds 1..300]\n       rpi-intercom pair ADDRESS\n       rpi-intercom run ADDRESS [ADDRESS ...] [--interval SECONDS] [--connect]"
}

fn run(args: &[String]) -> Result<(), String> {
    let Some(action) = args.first().map(String::as_str) else {
        return Err(usage().into());
    };
    if matches!(action, "-h" | "--help") {
        println!("{}", usage());
        return Ok(());
    }
    match action {
        "scan" => {
            let seconds = match &args[1..] {
                [] => 15,
                [flag, value] if flag == "--seconds" => {
                    value.parse::<u64>().map_err(|_| usage())?
                }
                _ => return Err(usage().into()),
            };
            if !(1..=300).contains(&seconds) {
                return Err("--seconds must be between 1 and 300".into());
            }
            print!(
                "{}",
                command(
                    &[
                        "bluetoothctl",
                        "--timeout",
                        &seconds.to_string(),
                        "scan",
                        "on"
                    ],
                    Duration::from_secs(seconds + 5)
                )?
            );
        }
        "pair" => {
            if args.len() != 2 {
                return Err(usage().into());
            }
            let device = address(&args[1])?;
            print!(
                "{}",
                command(
                    &["bluetoothctl", "--timeout", "60", "pair", &device],
                    Duration::from_secs(65)
                )?
            );
            if !paired(&command(
                &["bluetoothctl", "info", &device],
                Duration::from_secs(15),
            )?) {
                return Err(format!("pairing did not succeed for {device}"));
            }
            for action in ["trust", "connect"] {
                print!(
                    "{}",
                    command(
                        &["bluetoothctl", "--timeout", "60", action, &device],
                        Duration::from_secs(65)
                    )?
                );
            }
        }
        "run" => {
            let mut allowed = BTreeSet::new();
            let mut interval = 2.0_f64;
            let mut connect = false;
            let mut index = 1;
            while index < args.len() {
                match args[index].as_str() {
                    "--connect" => connect = true,
                    "--interval" => {
                        index += 1;
                        interval = args
                            .get(index)
                            .ok_or_else(usage)?
                            .parse()
                            .map_err(|_| "--interval must be positive")?;
                    }
                    value if value.starts_with('-') => return Err(usage().into()),
                    value => {
                        allowed.insert(address(value)?);
                    }
                }
                index += 1;
            }
            if allowed.is_empty() {
                return Err(usage().into());
            }
            if !interval.is_finite() || interval <= 0.0 || interval > u64::MAX as f64 {
                return Err("--interval must be positive and finite".into());
            }
            let interval = Duration::from_secs_f64(interval);
            let stopped = Arc::new(AtomicBool::new(false));
            let signal = Arc::clone(&stopped);
            ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
                .map_err(|e| e.to_string())?;
            let mut router = Router::new(allowed);
            if connect {
                for device in &router.allowed {
                    if let Err(error) = command(
                        &["bluetoothctl", "--timeout", "30", "connect", device],
                        Duration::from_secs(35),
                    ) {
                        eprintln!("WARNING: Could not connect {device}: {error}");
                    }
                }
            }
            let result: Result<(), String> = (|| {
                while !stopped.load(Ordering::SeqCst) {
                    let headsets = router.update()?;
                    let active = headsets
                        .values()
                        .filter(|headset| !headset.sources.is_empty() && !headset.sinks.is_empty())
                        .count();
                    eprintln!(
                        "INFO: {active}/{} headsets with duplex audio",
                        router.allowed.len()
                    );
                    let deadline = Instant::now() + interval;
                    while !stopped.load(Ordering::SeqCst) && Instant::now() < deadline {
                        thread::sleep(
                            deadline
                                .saturating_duration_since(Instant::now())
                                .min(Duration::from_millis(100)),
                        );
                    }
                }
                Ok(())
            })();
            router.close();
            result?;
        }
        _ => return Err(usage().into()),
    }
    Ok(())
}

fn main() {
    if let Err(error) = run(&env::args().skip(1).collect::<Vec<_>>()) {
        eprintln!("rpi-intercom: {error}");
        std::process::exit(1);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_address_before_invoking_commands() {
        assert!(address("invalid; rm -rf /").is_err());
        assert_eq!(address("aa:bb:cc:dd:ee:ff").unwrap(), "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn confirms_pairing_status() {
        assert!(!paired("  Paired: no\n"));
        assert!(paired("  Paired: yes\n"));
        assert!(!paired("Not Paired: yes\n"));
    }

    #[test]
    fn rejects_invalid_cli_arguments() {
        assert!(run(&["scan".into(), "--seconds".into(), "301".into()]).is_err());
        assert!(
            run(&[
                "run".into(),
                "AA:BB:CC:DD:EE:01".into(),
                "--interval".into(),
                "NaN".into()
            ])
            .is_err()
        );
        assert!(run(&["run".into()]).is_err());
    }
}
