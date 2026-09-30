use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use serde_json::Value;

use crate::command;

type Link = (u64, u64);

#[derive(Debug, PartialEq, Eq)]
pub struct Port {
    id: u64,
    channel: String,
}

#[derive(Debug, Default)]
pub struct Headset {
    pub sources: Vec<Port>,
    pub sinks: Vec<Port>,
}

fn property<'a>(props: &'a Value, key: &str) -> Option<&'a str> {
    props.get(key)?.as_str()
}

fn id_string(value: &Value) -> Option<String> {
    match value {
        Value::String(id) => Some(id.clone()),
        Value::Number(id) => Some(id.to_string()),
        _ => None,
    }
}

pub fn topology(
    objects: &Value,
    allowed: &BTreeSet<String>,
) -> Result<(BTreeMap<String, Headset>, BTreeSet<Link>), String> {
    let objects = objects
        .as_array()
        .ok_or("pw-dump output must be an array")?;
    let mut devices = BTreeMap::new();
    let mut nodes = BTreeMap::new();
    let mut ports = Vec::new();
    let mut links = BTreeSet::new();
    for object in objects {
        let info = &object["info"];
        let props = &info["props"];
        let Some(id) = object.get("id").and_then(id_string) else {
            continue;
        };
        match object["type"]
            .as_str()
            .and_then(|kind| kind.rsplit(':').next())
        {
            Some("Device") => {
                if let Some(address) = property(props, "api.bluez5.address") {
                    let address = address.to_ascii_uppercase();
                    if allowed.contains(&address) {
                        devices.insert(id, address);
                    }
                }
            }
            Some("Node") => {
                if matches!(
                    property(props, "media.class"),
                    Some("Audio/Source" | "Audio/Sink")
                ) && property(props, "api.bluez5.profile") == Some("headset-head-unit")
                    && let (Some(device), Some(class)) = (
                        props.get("device.id").and_then(id_string),
                        property(props, "media.class"),
                    )
                {
                    nodes.insert(id, (device, class));
                }
            }
            Some("Port") => {
                if let (Some(port_id), Some(node)) = (
                    object["id"].as_u64(),
                    props.get("node.id").and_then(id_string),
                ) {
                    ports.push((
                        port_id,
                        node,
                        property(props, "port.direction").unwrap_or("").to_owned(),
                        property(props, "audio.channel")
                            .unwrap_or("MONO")
                            .to_owned(),
                    ));
                }
            }
            Some("Link") => {
                if let (Some(output), Some(input)) = (
                    info["output-port-id"].as_u64(),
                    info["input-port-id"].as_u64(),
                ) {
                    links.insert((output, input));
                }
            }
            _ => {}
        }
    }
    let mut headsets: BTreeMap<_, _> = devices
        .values()
        .map(|address| (address.clone(), Headset::default()))
        .collect();
    for (id, node_id, direction, channel) in ports {
        let Some((device_id, class)) = nodes.get(&node_id) else {
            continue;
        };
        let Some(address) = devices.get(device_id) else {
            continue;
        };
        let headset = headsets.get_mut(address).expect("known device");
        if *class == "Audio/Source" && direction == "out" {
            headset.sources.push(Port { id, channel });
        } else if *class == "Audio/Sink" && direction == "in" {
            headset.sinks.push(Port { id, channel });
        }
    }
    Ok((headsets, links))
}

pub fn desired_links(headsets: &BTreeMap<String, Headset>) -> BTreeSet<Link> {
    let mut desired = BTreeSet::new();
    for (source_address, source) in headsets {
        for (sink_address, sink) in headsets {
            if source_address == sink_address {
                continue;
            }
            for output in &source.sources {
                for input in &sink.sinks {
                    if output.channel == input.channel
                        || output.channel == "MONO"
                        || input.channel == "MONO"
                    {
                        desired.insert((output.id, input.id));
                    }
                }
            }
        }
    }
    desired
}

pub struct Router<F = fn(&[&str]) -> Result<String, String>> {
    pub allowed: BTreeSet<String>,
    owned: BTreeSet<Link>,
    execute: F,
}

fn default_command(args: &[&str]) -> Result<String, String> {
    command(args, Duration::from_secs(15))
}

impl Router {
    pub fn new(allowed: BTreeSet<String>) -> Self {
        Self::with_executor(allowed, default_command)
    }
}

impl<F: FnMut(&[&str]) -> Result<String, String>> Router<F> {
    pub fn with_executor(allowed: BTreeSet<String>, execute: F) -> Self {
        Self {
            allowed,
            owned: BTreeSet::new(),
            execute,
        }
    }

    pub fn update(&mut self) -> Result<BTreeMap<String, Headset>, String> {
        let snapshot = serde_json::from_str(&(self.execute)(&["pw-dump"])?)
            .map_err(|e| format!("invalid pw-dump JSON: {e}"))?;
        let (headsets, existing) = topology(&snapshot, &self.allowed)?;
        let desired = desired_links(&headsets);
        for (output, input) in self.owned.difference(&desired).copied().collect::<Vec<_>>() {
            if existing.contains(&(output, input)) {
                (self.execute)(&["pw-link", "-d", &output.to_string(), &input.to_string()])?;
            }
            self.owned.remove(&(output, input));
        }
        for &(output, input) in desired.difference(&existing) {
            match (self.execute)(&["pw-link", &output.to_string(), &input.to_string()]) {
                Ok(_) => {
                    self.owned.insert((output, input));
                }
                Err(error) => {
                    eprintln!("WARNING: Could not link ports {output} -> {input}: {error}")
                }
            }
        }
        Ok(headsets)
    }

    pub fn close(&mut self) {
        for &(output, input) in &self.owned {
            if let Err(error) =
                (self.execute)(&["pw-link", "-d", &output.to_string(), &input.to_string()])
            {
                eprintln!("WARNING: Could not unlink ports {output} -> {input}: {error}");
            }
        }
        self.owned.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::cell::RefCell;
    use std::rc::Rc;

    const A: &str = "AA:BB:CC:DD:EE:01";
    const B: &str = "AA:BB:CC:DD:EE:02";

    fn headset(base: u64, address: &str) -> Vec<Value> {
        vec![
            json!({"type":"PipeWire:Interface:Device","id":base,"info":{"props":{"api.bluez5.address":address}}}),
            json!({"type":"PipeWire:Interface:Node","id":base+1,"info":{"props":{"device.id":base.to_string(),"media.class":"Audio/Source","api.bluez5.profile":"headset-head-unit"}}}),
            json!({"type":"PipeWire:Interface:Node","id":base+2,"info":{"props":{"device.id":base.to_string(),"media.class":"Audio/Sink","api.bluez5.profile":"headset-head-unit"}}}),
            json!({"type":"PipeWire:Interface:Port","id":base+3,"info":{"props":{"node.id":(base+1).to_string(),"port.direction":"out","audio.channel":"MONO"}}}),
            json!({"type":"PipeWire:Interface:Port","id":base+4,"info":{"props":{"node.id":(base+2).to_string(),"port.direction":"in","audio.channel":"FL"}}}),
            json!({"type":"PipeWire:Interface:Port","id":base+5,"info":{"props":{"node.id":(base+2).to_string(),"port.direction":"in","audio.channel":"FR"}}}),
        ]
    }

    fn fixture() -> Vec<Value> {
        [headset(10, A), headset(20, B)].concat()
    }

    fn allowed() -> BTreeSet<String> {
        [A.to_string(), B.to_string()].into()
    }

    #[test]
    fn routes_only_other_allowlisted_headsets() {
        let (headsets, links) = topology(&json!(fixture()), &allowed()).unwrap();
        assert!(links.is_empty());
        assert_eq!(
            desired_links(&headsets),
            [(13, 24), (13, 25), (23, 14), (23, 15)].into()
        );
        let (headsets, _) = topology(&json!(fixture()), &[A.to_string()].into()).unwrap();
        assert!(desired_links(&headsets).is_empty());
    }

    #[test]
    fn filters_profiles_and_matches_channels() {
        let mut objects = fixture();
        objects[1]["info"]["props"]["api.bluez5.profile"] = json!("bap-duplex");
        objects[2]["info"]["props"]["api.bluez5.profile"] = json!("a2dp-sink");
        let (headsets, _) = topology(&json!(objects), &allowed()).unwrap();
        assert!(desired_links(&headsets).is_empty());
        let mut objects = fixture();
        objects[3]["info"]["props"]["audio.channel"] = json!("FL");
        let (headsets, _) = topology(&json!(objects), &allowed()).unwrap();
        assert!(!desired_links(&headsets).contains(&(13, 25)));
        objects[1]["info"]["props"]
            .as_object_mut()
            .unwrap()
            .remove("api.bluez5.profile");
        let (headsets, _) = topology(&json!(objects), &allowed()).unwrap();
        assert_eq!(desired_links(&headsets), [(23, 14), (23, 15)].into());
    }

    #[test]
    fn ignores_unrelated_ports_and_detects_existing_links() {
        let mut objects = fixture();
        objects.push(json!({"type":"PipeWire:Interface:Node","id":31,"info":{"props":{"device.id":"10","media.class":"Stream/Output/Audio"}}}));
        objects.push(json!({"type":"PipeWire:Interface:Port","id":32,"info":{"props":{"node.id":"31","port.direction":"out"}}}));
        objects.push(json!({"type":"PipeWire:Interface:Link","id":100,"info":{"output-port-id":13,"input-port-id":24}}));
        let (headsets, links) = topology(&json!(objects), &allowed()).unwrap();
        assert_eq!(headsets[A].sources.len(), 1);
        assert_eq!(links, [(13, 24)].into());
    }

    #[test]
    fn owns_only_created_links_and_cleans_up_on_disappearance() {
        let objects = Rc::new(RefCell::new(fixture()));
        objects.borrow_mut().push(json!({"type":"PipeWire:Interface:Link","id":100,"info":{"output-port-id":13,"input-port-id":24}}));
        let calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
        let snapshot = Rc::clone(&objects);
        let recorded = Rc::clone(&calls);
        let mut router = Router::with_executor(allowed(), move |args: &[&str]| {
            recorded
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            if args[0] == "pw-dump" {
                Ok(json!(*snapshot.borrow()).to_string())
            } else {
                Ok(String::new())
            }
        });
        router.update().unwrap();
        assert_eq!(router.owned.len(), 3);
        assert!(
            !calls
                .borrow()
                .contains(&vec!["pw-link".into(), "13".into(), "24".into()])
        );
        *objects.borrow_mut() = headset(10, A);
        router.update().unwrap();
        assert!(router.owned.is_empty());
        router.close();
        assert!(!calls.borrow().contains(&vec![
            "pw-link".into(),
            "-d".into(),
            "13".into(),
            "24".into()
        ]));
    }

    #[test]
    fn failed_link_retries_and_close_only_unlinks_owned() {
        let calls = Rc::new(RefCell::new(Vec::<Vec<String>>::new()));
        let recorded = Rc::clone(&calls);
        let mut failed = false;
        let mut router = Router::with_executor(allowed(), move |args: &[&str]| {
            recorded
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            if args[0] == "pw-dump" {
                return Ok(json!(fixture()).to_string());
            }
            if !failed {
                failed = true;
                return Err("failed".into());
            }
            Ok(String::new())
        });
        router.update().unwrap();
        assert_eq!(router.owned.len(), 3);
        router.update().unwrap();
        assert_eq!(router.owned.len(), 4);
        router.close();
        assert!(router.owned.is_empty());
        assert_eq!(
            calls
                .borrow()
                .iter()
                .filter(|args| args.get(1).is_some_and(|s| s == "-d"))
                .count(),
            4
        );
    }
}
