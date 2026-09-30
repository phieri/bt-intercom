mod dashboard;
mod router;

use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::{BufRead, IsTerminal, Read};
use std::process::{Child, Command, ExitStatus, Stdio};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver, TryRecvError};
use std::thread;
use std::time::{Duration, Instant};

use dashboard::Dashboard;
use router::Router;

fn command(args: &[&str], timeout: Duration) -> Result<String, String> {
    command_cancellable(args, timeout, None)
}

fn wait_child(
    child: &mut Child,
    name: &str,
    timeout: Duration,
    stopped: Option<&AtomicBool>,
) -> Result<ExitStatus, String> {
    let start = Instant::now();
    loop {
        let cancelled = stopped.is_some_and(|flag| flag.load(Ordering::SeqCst));
        let error = match child.try_wait() {
            Ok(Some(status)) => return Ok(status),
            Ok(None) if cancelled => format!("{name} cancelled"),
            Ok(None) if start.elapsed() >= timeout => format!("{name} timed out"),
            Ok(None) => {
                thread::sleep(Duration::from_millis(20));
                continue;
            }
            Err(error) => error.to_string(),
        };
        let _ = child.kill();
        let _ = child.wait();
        return Err(error);
    }
}

fn command_cancellable(
    args: &[&str],
    timeout: Duration,
    stopped: Option<&AtomicBool>,
) -> Result<String, String> {
    if stopped.is_some_and(|flag| flag.load(Ordering::SeqCst)) {
        return Err(format!("{} cancelled", args[0]));
    }
    let mut child = Command::new(args[0])
        .args(&args[1..])
        .stdin(Stdio::null())
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
    let status = wait_child(&mut child, args[0], timeout, stopped)?;
    let stdout = output
        .join()
        .map_err(|_| "stdout reader panicked".to_string())?
        .map_err(|e| e.to_string())?;
    let stderr = error
        .join()
        .map_err(|_| "stderr reader panicked".to_string())?
        .map_err(|e| e.to_string())?;
    if !status.success() {
        return Err(format!(
            "{} exited with {status}: {}",
            args[0],
            String::from_utf8_lossy(&stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&stdout).into_owned())
}

fn pair_command(device: &str) -> Result<(), String> {
    eprintln!("At the Bluetooth prompt, enter: pair {device}");
    eprintln!("Answer any PIN/confirmation prompts, then enter: quit");
    let mut child = Command::new("bluetoothctl")
        .args(["--agent", "KeyboardDisplay"])
        .stdin(Stdio::inherit())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .spawn()
        .map_err(|error| format!("bluetoothctl: {error}"))?;
    let status = wait_child(&mut child, "bluetoothctl", Duration::from_secs(300), None)?;
    if status.success() {
        Ok(())
    } else {
        Err(format!("bluetoothctl pair exited with {status}"))
    }
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

fn device_flag(info: &str, flag: &str) -> bool {
    info.lines().any(|line| {
        line.trim()
            .split_once(':')
            .is_some_and(|(key, value)| key.trim() == flag && value.trim() == "yes")
    })
}

fn usage() -> &'static str {
    "Usage: rpi-intercom scan [--seconds 1..300]\n       rpi-intercom pair ADDRESS\n       rpi-intercom status ADDRESS [ADDRESS ...]\n       rpi-intercom run ADDRESS [ADDRESS ...] [--interval SECONDS] [--connect] [--ptt] [--dashboard]"
}

fn addresses(args: &[String]) -> Result<BTreeSet<String>, String> {
    if args.is_empty() {
        return Err(usage().into());
    }
    args.iter().map(|value| address(value)).collect()
}

fn run_options(args: &[String]) -> Result<(BTreeSet<String>, Duration, bool, bool, bool), String> {
    let mut allowed = BTreeSet::new();
    let mut interval = 2.0_f64;
    let mut connect = false;
    let mut ptt = false;
    let mut dashboard = false;
    let mut index = 1;
    while index < args.len() {
        match args[index].as_str() {
            "--connect" => connect = true,
            "--ptt" => ptt = true,
            "--dashboard" => dashboard = true,
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
    if !interval.is_finite() || interval <= 0.0 || interval >= u64::MAX as f64 {
        return Err("--interval must be positive and finite".into());
    }
    let interval = Duration::from_secs_f64(interval);
    if interval.is_zero() {
        return Err("--interval must be at least one nanosecond".into());
    }
    Ok((allowed, interval, connect, ptt, dashboard))
}

fn ptt_input_from<R: BufRead>(mut input: R, sender: mpsc::Sender<bool>) {
    let mut transmitting = false;
    loop {
        let mut line = String::new();
        match input.read_line(&mut line) {
            Ok(0) | Err(_) => break,
            Ok(_) => {
                transmitting = !transmitting;
                if sender.send(transmitting).is_err() {
                    break;
                }
            }
        }
    }
}

fn ptt_input() -> Receiver<bool> {
    let (sender, receiver) = mpsc::channel();
    thread::spawn(move || {
        let stdin = std::io::stdin();
        ptt_input_from(stdin.lock(), sender);
    });
    receiver
}

fn connect_disconnected(
    allowed: &BTreeSet<String>,
    stopped: &AtomicBool,
    mut execute: impl FnMut(&[&str], Duration) -> Result<String, String>,
) {
    for device in allowed {
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        let connected = execute(&["bluetoothctl", "info", device], Duration::from_secs(15))
            .is_ok_and(|info| device_flag(&info, "Connected"));
        if !connected
            && !stopped.load(Ordering::SeqCst)
            && let Err(error) = execute(
                &["bluetoothctl", "--timeout", "30", "connect", device],
                Duration::from_secs(35),
            )
        {
            eprintln!("WARNING: Could not connect {device}: {error}");
        }
    }
}

fn reconnect_worker(
    allowed: BTreeSet<String>,
    stopped: Arc<AtomicBool>,
    shutdown: Receiver<()>,
    retry: Duration,
    mut execute: impl FnMut(&[&str], Duration) -> Result<String, String>,
) {
    loop {
        connect_disconnected(&allowed, &stopped, &mut execute);
        if stopped.load(Ordering::SeqCst) {
            break;
        }
        if !matches!(
            shutdown.recv_timeout(retry),
            Err(mpsc::RecvTimeoutError::Timeout)
        ) {
            break;
        }
    }
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
            pair_command(&device)?;
            if !device_flag(
                &command(&["bluetoothctl", "info", &device], Duration::from_secs(15))?,
                "Paired",
            ) {
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
        "status" => {
            let allowed = addresses(&args[1..])?;
            let mut router = Router::new(allowed);
            let (headsets, _) = router.inspect()?;
            for address in &router.allowed {
                match headsets.get(address) {
                    Some(headset) => println!(
                        "{address}: {} ({} microphone ports, {} speaker ports)",
                        if headset.has_duplex_audio() {
                            "duplex ready"
                        } else {
                            "duplex unavailable"
                        },
                        headset.sources.len(),
                        headset.sinks.len()
                    ),
                    None => println!("{address}: not found in PipeWire"),
                }
            }
        }
        "run" => {
            let (allowed, interval, connect, ptt, show_dashboard) = run_options(args)?;
            if show_dashboard && !std::io::stderr().is_terminal() {
                return Err("--dashboard requires an interactive terminal on stderr".into());
            }
            let stopped = Arc::new(AtomicBool::new(false));
            let signal = Arc::clone(&stopped);
            ctrlc::set_handler(move || signal.store(true, Ordering::SeqCst))
                .map_err(|e| e.to_string())?;
            let mut router = Router::new(allowed);
            let mut dashboard = show_dashboard
                .then(|| Dashboard::start(router.allowed.clone(), Arc::clone(&stopped)));
            let mut headsets = BTreeMap::new();
            let mut links = BTreeSet::new();
            let mut last_error = None;
            let mut redraw = true;
            let (shutdown, shutdown_rx) = mpsc::channel();
            let connector = if connect {
                let devices = router.allowed.clone();
                let cancelled = Arc::clone(&stopped);
                Some(thread::spawn(move || {
                    reconnect_worker(
                        devices,
                        Arc::clone(&cancelled),
                        shutdown_rx,
                        Duration::from_secs(30),
                        |args, timeout| command_cancellable(args, timeout, Some(&cancelled)),
                    );
                }))
            } else {
                None
            };
            let input = if ptt {
                eprintln!("PTT: Press Enter to transmit; press Enter again to mute.");
                Some(ptt_input())
            } else {
                None
            };
            let mut transmitting = !ptt;
            let mut last_update: Option<Instant> = None;
            let result: Result<(), String> = (|| {
                while !stopped.load(Ordering::SeqCst) {
                    if let Some(ref input) = input {
                        loop {
                            match input.try_recv() {
                                Ok(state) => {
                                    transmitting = state;
                                    last_update = None;
                                    redraw = true;
                                    if dashboard.is_none() {
                                        eprintln!(
                                            "PTT: {}",
                                            if state { "transmitting" } else { "muted" }
                                        );
                                    }
                                }
                                Err(TryRecvError::Empty) => break,
                                Err(TryRecvError::Disconnected) => {
                                    return Err(
                                        "PTT input closed; run --ptt with stdin open".into()
                                    );
                                }
                            }
                        }
                    }
                    if last_update.is_none_or(|updated| updated.elapsed() >= interval) {
                        let update = router.update(transmitting);
                        if dashboard.is_some() {
                            let mut inspect_error = None;
                            match router.inspect_owned() {
                                Ok((current, active)) => {
                                    headsets = current;
                                    links = active;
                                }
                                Err(error) => {
                                    headsets.clear();
                                    links.clear();
                                    inspect_error = Some(error);
                                }
                            }
                            last_error = update.err().or(inspect_error);
                            redraw = true;
                        } else {
                            match update {
                                Ok(headsets) => {
                                    let active = headsets
                                        .values()
                                        .filter(|headset| headset.has_duplex_audio())
                                        .count();
                                    eprintln!(
                                        "INFO: {active}/{} headsets with duplex audio",
                                        router.allowed.len()
                                    );
                                }
                                Err(error) => eprintln!("WARNING: Routing update failed: {error}"),
                            }
                        }
                        last_update = Some(Instant::now());
                    }
                    if let Some(ref mut dashboard) = dashboard {
                        redraw |= dashboard.refresh();
                        if redraw {
                            dashboard
                                .draw(
                                    &router.allowed,
                                    &headsets,
                                    &links,
                                    transmitting,
                                    last_error.as_deref(),
                                    &mut std::io::stderr().lock(),
                                )
                                .map_err(|error| error.to_string())?;
                            redraw = false;
                        }
                    }
                    thread::sleep(Duration::from_millis(100).min(interval));
                }
                Ok(())
            })();
            stopped.store(true, Ordering::SeqCst);
            let _ = shutdown.send(());
            if let Some(ref mut dashboard) = dashboard {
                dashboard.stop();
            }
            if let Some(connector) = connector {
                let _ = connector.join();
            }
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
    fn confirms_bluetooth_device_flags() {
        assert!(!device_flag("  Paired: no\n", "Paired"));
        assert!(device_flag("  Paired: yes\n", "Paired"));
        assert!(!device_flag("Not Paired: yes\n", "Paired"));
        assert!(device_flag("  Connected: yes\n", "Connected"));
        assert!(!device_flag("  Paired: yes\n", "Connected"));
    }

    #[test]
    fn connects_only_when_disconnected_or_info_fails() {
        let device = "AA:BB:CC:DD:EE:01";
        let allowed = BTreeSet::from([device.to_string()]);
        for (info, should_connect) in [
            (Ok("Connected: yes\n"), false),
            (Ok("Connected: no\n"), true),
            (Err("bluetoothctl info failed"), true),
        ] {
            let mut calls = Vec::new();
            connect_disconnected(&allowed, &AtomicBool::new(false), |args, timeout| {
                calls.push((
                    args.iter().map(|arg| arg.to_string()).collect::<Vec<_>>(),
                    timeout,
                ));
                if args[1] == "info" {
                    info.map(str::to_string).map_err(str::to_string)
                } else {
                    Ok(String::new())
                }
            });
            let mut expected = vec![(
                vec![
                    "bluetoothctl".to_string(),
                    "info".to_string(),
                    device.to_string(),
                ],
                Duration::from_secs(15),
            )];
            if should_connect {
                expected.push((
                    vec![
                        "bluetoothctl".to_string(),
                        "--timeout".to_string(),
                        "30".to_string(),
                        "connect".to_string(),
                        device.to_string(),
                    ],
                    Duration::from_secs(35),
                ));
            }
            assert_eq!(calls, expected);
        }
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
        assert!(run(&["status".into()]).is_err());
        assert!(addresses(&["--connect".into()]).is_err());
        assert!(run_options(&["run".into(), "--ptt".into()]).is_err());
        assert!(
            run_options(&["run".into(), "AA:BB:CC:DD:EE:01".into(), "--unknown".into()]).is_err()
        );
    }

    #[test]
    fn parses_ptt_without_changing_default_mode() {
        let address = "AA:BB:CC:DD:EE:01".to_string();
        let (_, _, _, ptt, dashboard) = run_options(&["run".into(), address.clone()]).unwrap();
        assert!(!ptt);
        assert!(!dashboard);
        let (allowed, _, connect, ptt, dashboard) = run_options(&[
            "run".into(),
            "--ptt".into(),
            address,
            "--connect".into(),
            "--dashboard".into(),
        ])
        .unwrap();
        assert_eq!(allowed.len(), 1);
        assert!(connect && ptt && dashboard);
    }

    #[test]
    fn ptt_input_toggles_each_line_and_disconnects_at_eof() {
        let (sender, receiver) = mpsc::channel();
        ptt_input_from(std::io::Cursor::new(b"\n\n"), sender);

        assert!(receiver.recv().unwrap());
        assert!(!receiver.recv().unwrap());
        assert!(matches!(
            receiver.try_recv(),
            Err(TryRecvError::Disconnected)
        ));
    }

    #[test]
    fn rejects_intervals_that_round_to_zero() {
        assert!(
            run_options(&[
                "run".into(),
                "AA:BB:CC:DD:EE:01".into(),
                "--interval".into(),
                "0.00000000001".into(),
            ])
            .is_err()
        );
    }

    #[test]
    fn connection_cancellation_skips_remaining_devices() {
        let stopped = AtomicBool::new(false);
        let allowed = ["AA:BB:CC:DD:EE:01".into(), "AA:BB:CC:DD:EE:02".into()].into();
        let mut calls = 0;
        connect_disconnected(&allowed, &stopped, |args, _| {
            assert_eq!(args[1], "info");
            calls += 1;
            stopped.store(true, Ordering::SeqCst);
            Err("cancelled".into())
        });
        assert_eq!(calls, 1);
    }

    #[test]
    fn reconnect_worker_retries_and_wakes_on_shutdown() {
        let (shutdown, receiver) = mpsc::channel();
        let (attempt, attempts) = mpsc::channel();
        let worker = thread::spawn(move || {
            reconnect_worker(
                ["AA:BB:CC:DD:EE:01".into()].into(),
                Arc::new(AtomicBool::new(false)),
                receiver,
                Duration::from_millis(10),
                |_, _| {
                    attempt.send(()).unwrap();
                    Ok("Connected: yes\n".into())
                },
            );
        });
        attempts.recv_timeout(Duration::from_secs(5)).unwrap();
        attempts.recv_timeout(Duration::from_secs(5)).unwrap();
        shutdown.send(()).unwrap();
        worker.join().unwrap();
    }

    #[test]
    fn commands_capture_output_errors_and_do_not_read_ptt_input() {
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
            command(
                &["sh", "-c", "sleep 2 & wait"],
                Duration::from_millis(20)
            )
            .unwrap_err()
            .contains("timed out")
        );
        assert!(start.elapsed() < Duration::from_secs(1));
    }
}
