use std::collections::{BTreeMap, BTreeSet};
use std::io::{self, Write};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{self, Receiver};
use std::thread::{self, JoinHandle};
use std::time::Duration;

use crate::router::Headset;
use crate::{command_cancellable, device_flag};

#[derive(Debug, PartialEq, Eq)]
pub struct Bluetooth {
    paired: bool,
    connected: bool,
    rssi: Option<i32>,
}

fn bluetooth_info(info: &str) -> Bluetooth {
    let rssi = info.lines().find_map(|line| {
        let (key, value) = line.trim().split_once(':')?;
        (key == "RSSI").then(|| value.split_whitespace().next()?.parse().ok())?
    });
    Bluetooth {
        paired: device_flag(info, "Paired"),
        connected: device_flag(info, "Connected"),
        rssi,
    }
}

pub struct Dashboard {
    receiver: Receiver<(String, Option<Bluetooth>)>,
    state: BTreeMap<String, Option<Bluetooth>>,
    shutdown: mpsc::Sender<()>,
    worker: Option<JoinHandle<()>>,
}

impl Dashboard {
    pub fn start(allowed: BTreeSet<String>, stopped: Arc<AtomicBool>) -> Self {
        let (sender, receiver) = mpsc::channel();
        let (shutdown, wake) = mpsc::channel();
        let worker = thread::spawn(move || {
            loop {
                for address in &allowed {
                    if stopped.load(Ordering::SeqCst) {
                        return;
                    }
                    let info = command_cancellable(
                        &["bluetoothctl", "info", address],
                        Duration::from_secs(15),
                        Some(&stopped),
                    )
                    .ok()
                    .map(|output| bluetooth_info(&output));
                    if sender.send((address.clone(), info)).is_err() {
                        return;
                    }
                }
                if wake.recv_timeout(Duration::from_secs(10)).is_ok()
                    || stopped.load(Ordering::SeqCst)
                {
                    return;
                }
            }
        });
        Self {
            receiver,
            state: BTreeMap::new(),
            shutdown,
            worker: Some(worker),
        }
    }

    pub fn refresh(&mut self) -> bool {
        let mut changed = false;
        while let Ok((address, info)) = self.receiver.try_recv() {
            self.state.insert(address, info);
            changed = true;
        }
        changed
    }

    pub fn draw(
        &self,
        allowed: &BTreeSet<String>,
        headsets: &BTreeMap<String, Headset>,
        links: &BTreeSet<(u64, u64)>,
        transmitting: bool,
        error: Option<&str>,
        output: &mut impl Write,
    ) -> io::Result<()> {
        writeln!(
            output,
            "\x1b[H\x1b[2Jrpi-intercom | {} | {} owned active links",
            if transmitting {
                "transmitting"
            } else {
                "muted"
            },
            links.len()
        )?;
        writeln!(
            output,
            "ADDRESS             PAIRED  CONNECTED  DUPLEX  TX/RX LINKS  SIGNAL"
        )?;
        for address in allowed {
            let bluetooth = self.state.get(address).and_then(Option::as_ref);
            let status = |flag: Option<bool>| match flag {
                Some(true) => "yes",
                Some(false) => "no",
                None => "?",
            };
            let headset = headsets.get(address);
            let (tx, rx) = headset.map_or((0, 0), |headset| {
                (
                    links
                        .iter()
                        .filter(|(out, _)| headset.sources.iter().any(|port| port.id == *out))
                        .count(),
                    links
                        .iter()
                        .filter(|(_, input)| headset.sinks.iter().any(|port| port.id == *input))
                        .count(),
                )
            });
            let signal = bluetooth
                .and_then(|info| info.connected.then_some(info.rssi).flatten())
                .map_or_else(
                    || "unknown".to_string(),
                    |rssi| {
                        let quality = match rssi {
                            -60.. => "strong",
                            -70..=-61 => "good",
                            -80..=-71 => "fair",
                            _ => "weak",
                        };
                        format!("{rssi} dBm ({quality})")
                    },
                );
            writeln!(
                output,
                "{address}  {:<6}  {:<9}  {:<6}  {tx:>2}/{rx:<2}         {signal}",
                status(bluetooth.map(|info| info.paired)),
                status(bluetooth.map(|info| info.connected)),
                status(headset.map(Headset::has_duplex_audio))
            )?;
        }
        writeln!(
            output,
            "\nLinks represent routing, not measured speech; RSSI is shown only if BlueZ reports it."
        )?;
        if let Some(error) = error {
            writeln!(
                output,
                "Last routing error: {}",
                error
                    .chars()
                    .filter(|ch| !ch.is_control())
                    .collect::<String>()
            )?;
        }
        output.flush()
    }

    pub fn stop(&mut self) {
        let _ = self.shutdown.send(());
        if let Some(worker) = self.worker.take() {
            let _ = worker.join();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parses_bluetooth_status_and_optional_signal() {
        assert_eq!(
            bluetooth_info("Paired: yes\nConnected: yes\nRSSI: -67 (0xffffffbd)\n"),
            Bluetooth {
                paired: true,
                connected: true,
                rssi: Some(-67)
            }
        );
        assert_eq!(bluetooth_info("RSSI: unavailable\n").rssi, None);
    }

    #[test]
    fn renders_unknown_status_and_observed_owned_links() {
        let allowed = BTreeSet::from(["AA:BB:CC:DD:EE:01".to_string()]);
        let (sender, receiver) = mpsc::channel();
        drop(sender);
        let (shutdown, _) = mpsc::channel();
        let dashboard = Dashboard {
            receiver,
            state: BTreeMap::new(),
            shutdown,
            worker: None,
        };
        let objects = json!([
            {"type":"PipeWire:Interface:Device","id":1,"info":{"props":{"api.bluez5.address":"AA:BB:CC:DD:EE:01"}}},
            {"type":"PipeWire:Interface:Node","id":2,"info":{"props":{"device.id":"1","media.class":"Audio/Source","api.bluez5.profile":"headset-head-unit"}}},
            {"type":"PipeWire:Interface:Port","id":3,"info":{"props":{"node.id":"2","port.direction":"out"}}}
        ]);
        let (headsets, _) = crate::router::topology(&objects, &allowed).unwrap();
        let mut output = Vec::new();
        dashboard
            .draw(
                &allowed,
                &headsets,
                &BTreeSet::from([(3, 4)]),
                false,
                None,
                &mut output,
            )
            .unwrap();
        let text = String::from_utf8(output).unwrap();
        assert!(text.contains("muted | 1 owned active links"));
        assert!(text.contains("AA:BB:CC:DD:EE:01  ?"));
        assert!(text.contains("no"));
        assert!(text.contains("1/0"));
        assert!(text.contains("unknown"));
    }
}
