use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::command;

type Link = (u64, u64);
const OWNER_PROPERTY: &str = "rpi-intercom.owner";
static NEXT_ROUTER: AtomicUsize = AtomicUsize::new(0);

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

impl Headset {
    pub fn has_duplex_audio(&self) -> bool {
        !self.sources.is_empty() && !self.sinks.is_empty()
    }
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
    owner: String,
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
            owner: format!(
                "{}-{}-{}",
                std::process::id(),
                SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_nanos(),
                NEXT_ROUTER.fetch_add(1, Ordering::Relaxed)
            ),
            execute,
        }
    }

    pub fn update(&mut self, transmitting: bool) -> Result<BTreeMap<String, Headset>, String> {
        let snapshot = self.snapshot()?;
        let (headsets, existing) = topology(&snapshot, &self.allowed)?;
        let desired = if transmitting {
            desired_links(&headsets)
        } else {
            BTreeSet::new()
        };
        let mut failures = Vec::new();
        for (id, link) in self.owned_links(&snapshot) {
            if !desired.contains(&link)
                && let Err(error) = (self.execute)(&["pw-link", "-d", &id.to_string()])
            {
                failures.push(error);
            }
        }
        let properties = serde_json::json!({OWNER_PROPERTY: self.owner}).to_string();
        for &(output, input) in desired.difference(&existing) {
            if let Err(error) = (self.execute)(&[
                "pw-link",
                "-L",
                "-p",
                &properties,
                &output.to_string(),
                &input.to_string(),
            ]) {
                failures.push(format!("Could not link ports {output} -> {input}: {error}"));
            }
        }
        if failures.is_empty() {
            Ok(headsets)
        } else {
            Err(failures.join("; "))
        }
    }

    pub fn inspect(&mut self) -> Result<(BTreeMap<String, Headset>, BTreeSet<Link>), String> {
        let snapshot = self.snapshot()?;
        topology(&snapshot, &self.allowed)
    }

    fn snapshot(&mut self) -> Result<Value, String> {
        serde_json::from_str(&(self.execute)(&["pw-dump"])?)
            .map_err(|e| format!("invalid pw-dump JSON: {e}"))
    }

    fn owned_links(&self, snapshot: &Value) -> BTreeMap<u64, Link> {
        snapshot
            .as_array()
            .into_iter()
            .flatten()
            .filter(|object| {
                object["type"] == "PipeWire:Interface:Link"
                    && object["info"]["props"][OWNER_PROPERTY].as_str() == Some(&self.owner)
            })
            .filter_map(|object| {
                Some((
                    object["id"].as_u64()?,
                    (
                        object["info"]["output-port-id"].as_u64()?,
                        object["info"]["input-port-id"].as_u64()?,
                    ),
                ))
            })
            .collect()
    }

    pub fn close(&mut self) {
        match self.snapshot() {
            Ok(snapshot) => {
                for id in self.owned_links(&snapshot).keys() {
                    if let Err(error) = (self.execute)(&["pw-link", "-d", &id.to_string()]) {
                        eprintln!("WARNING: Could not unlink link {id}: {error}");
                    }
                }
            }
            Err(error) => eprintln!("WARNING: Could not inspect links for cleanup: {error}"),
        }
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
    fn inspection_does_not_create_or_remove_links() {
        let mut router = Router::with_executor(allowed(), |args: &[&str]| {
            assert_eq!(args, ["pw-dump"]);
            Ok(json!(fixture()).to_string())
        });
        let (headsets, links) = router.inspect().unwrap();
        assert!(links.is_empty());
        assert!(headsets[A].has_duplex_audio());
        assert!(headsets[B].has_duplex_audio());
        assert!(!Headset::default().has_duplex_audio());
    }

    #[derive(Clone)]
    struct PipeWire {
        objects: Rc<RefCell<Vec<Value>>>,
        calls: Rc<RefCell<Vec<Vec<String>>>>,
        fail_next: Rc<RefCell<bool>>,
    }

    impl PipeWire {
        fn new() -> Self {
            Self {
                objects: Rc::new(RefCell::new(fixture())),
                calls: Rc::default(),
                fail_next: Rc::default(),
            }
        }

        fn execute(&self, args: &[&str]) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            if args[0] == "pw-dump" {
                return Ok(json!(*self.objects.borrow()).to_string());
            }
            if self.fail_next.replace(false) {
                return Err("simulated pw-link failure".into());
            }
            let mut objects = self.objects.borrow_mut();
            if args[1] == "-d" {
                assert_eq!(args.len(), 3, "delete a link ID, never a port pair");
                let id = args[2].parse::<u64>().unwrap();
                objects.retain(|object| object["id"] != id);
            } else {
                assert_eq!(&args[..3], ["pw-link", "-L", "-p"]);
                let props: Value = serde_json::from_str(args[3]).unwrap();
                assert!(props[OWNER_PROPERTY].as_str().is_some());
                let output = args[4].parse::<u64>().unwrap();
                let input = args[5].parse::<u64>().unwrap();
                let id = objects
                    .iter()
                    .filter_map(|object| object["id"].as_u64())
                    .max()
                    .unwrap_or(0)
                    + 1;
                objects.push(json!({"type":"PipeWire:Interface:Link","id":id,"info":{"output-port-id":output,"input-port-id":input,"props":props}}));
            }
            Ok(String::new())
        }

        fn links(&self) -> usize {
            self.objects
                .borrow()
                .iter()
                .filter(|object| object["type"] == "PipeWire:Interface:Link")
                .count()
        }

        fn external_link(&self) {
            self.objects.borrow_mut().push(json!({"type":"PipeWire:Interface:Link","id":100,"info":{"output-port-id":13,"input-port-id":24}}));
        }
    }

    #[test]
    fn owns_only_created_links_and_cleans_up_on_disappearance() {
        let server = PipeWire::new();
        server.external_link();
        let mut router = Router::with_executor(allowed(), |args: &[&str]| server.execute(args));
        router.update(true).unwrap();
        assert_eq!(server.links(), 4);
        *server.objects.borrow_mut() = headset(10, A);
        router.update(true).unwrap();
        router.close();
        assert!(
            !server
                .calls
                .borrow()
                .iter()
                .any(|args| args.get(1).is_some_and(|arg| arg == "-d"))
        );
    }

    #[test]
    fn failed_link_retries_and_close_only_unlinks_owned() {
        let server = PipeWire::new();
        let mut router = Router::with_executor(allowed(), |args: &[&str]| server.execute(args));
        server.fail_next.replace(true);
        assert!(router.update(true).is_err());
        assert_eq!(server.links(), 3);
        router.update(true).unwrap();
        assert_eq!(server.links(), 4);
        server.external_link();
        router.close();
        assert_eq!(server.links(), 1);
    }

    #[test]
    fn ptt_mutes_and_restores_only_owned_links() {
        let server = PipeWire::new();
        server.external_link();
        let mut router = Router::with_executor(allowed(), |args: &[&str]| server.execute(args));
        router.update(false).unwrap();
        assert_eq!(server.links(), 1);
        router.update(true).unwrap();
        assert_eq!(server.links(), 4);
        router.update(false).unwrap();
        assert_eq!(server.links(), 1);
        router.update(true).unwrap();
        assert_eq!(server.links(), 4);
    }

    #[test]
    fn reused_ids_and_replacement_links_are_not_owned() {
        for close in [false, true] {
            let server = PipeWire::new();
            let mut router = Router::with_executor(allowed(), |args: &[&str]| server.execute(args));
            router.update(true).unwrap();
            for object in server.objects.borrow_mut().iter_mut() {
                if object["type"] == "PipeWire:Interface:Link" {
                    object["info"]["props"] = json!({});
                }
            }
            if close {
                router.close();
            } else {
                router.update(false).unwrap();
            }
            assert_eq!(server.links(), 4);
        }
    }

    #[test]
    fn failed_unlink_does_not_skip_other_links_and_retries() {
        let server = PipeWire::new();
        let mut router = Router::with_executor(allowed(), |args: &[&str]| server.execute(args));
        router.update(true).unwrap();
        server.fail_next.replace(true);
        assert!(router.update(false).is_err());
        assert_eq!(server.links(), 1);
        router.update(false).unwrap();
        assert_eq!(server.links(), 0);
    }

    #[test]
    fn separate_routers_do_not_own_each_others_links() {
        let server = PipeWire::new();
        let mut first = Router::with_executor(allowed(), |args: &[&str]| server.execute(args));
        let mut second = Router::with_executor(allowed(), |args: &[&str]| server.execute(args));
        first.update(true).unwrap();
        second.update(true).unwrap();
        second.close();
        assert_eq!(server.links(), 4);
        first.close();
        assert_eq!(server.links(), 0);
    }

    #[test]
    fn profile_changes_remove_owned_links_and_reconnect_restores_them() {
        let server = PipeWire::new();
        let mut router = Router::with_executor(allowed(), |args: &[&str]| server.execute(args));
        router.update(true).unwrap();
        for object in server.objects.borrow_mut().iter_mut() {
            if object["type"] == "PipeWire:Interface:Node" {
                object["info"]["props"]["api.bluez5.profile"] = json!("a2dp-sink");
            }
        }
        router.update(true).unwrap();
        assert_eq!(server.links(), 0);
        *server.objects.borrow_mut() = fixture();
        router.update(true).unwrap();
        assert_eq!(server.links(), 4);
    }
}
