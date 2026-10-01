//! PipeWire topology discovery and ownership-safe inter-headset audio routing.
//!
//! Routing is limited to allowlisted Bluetooth devices using the duplex HFP/HSP
//! profile. Each created link is owned by a monitored `pw-cli` client, so
//! dropping that client releases the link without relying on reusable object IDs.

use std::collections::{BTreeMap, BTreeSet};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::Value;

use crate::command;

type Link = (u64, u64);
const OWNER_PROPERTY: &str = "rpi-intercom.owner";
static NEXT_ROUTER: AtomicUsize = AtomicUsize::new(0);

/// A live `pw-cli` client that owns a PipeWire link.
trait LinkHandle {
    fn is_running(&mut self) -> Result<bool, String>;
}

/// The production link handle; dropping it terminates its owning `pw-cli`.
struct LinkProcess(Child);

impl LinkHandle for LinkProcess {
    fn is_running(&mut self) -> Result<bool, String> {
        self.0
            .try_wait()
            .map(|status| status.is_none())
            .map_err(|error| error.to_string())
    }
}

impl Drop for LinkProcess {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

type StartLink = Box<dyn FnMut(&[&str]) -> Result<Box<dyn LinkHandle>, String>>;

#[derive(Debug, PartialEq, Eq)]
/// A PipeWire audio port and the channel it carries.
pub struct Port {
    pub(crate) id: u64,
    channel: String,
}

#[derive(Debug, Default)]
/// Audio ports belonging to one allowlisted Bluetooth headset.
pub struct Headset {
    /// Microphone output ports exposed by the headset.
    pub sources: Vec<Port>,
    /// Speaker input ports exposed by the headset.
    pub sinks: Vec<Port>,
}

impl Headset {
    /// Returns whether PipeWire exposes both microphone and speaker ports.
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

/// Extract allowlisted headset ports and existing links from a `pw-dump` snapshot.
///
/// Nodes are included only when they belong to a listed Bluetooth device and
/// use PipeWire's `headset-head-unit` profile.
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

/// Build cross-headset source-to-sink links whose channels are compatible.
///
/// A headset is never linked to itself; mono ports are compatible with every
/// channel on the opposite endpoint.
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

/// Reconciles desired headset routes with PipeWire while tracking owned links.
pub struct Router<F = fn(&[&str]) -> Result<String, String>> {
    /// Bluetooth addresses that may be discovered or routed.
    pub allowed: BTreeSet<String>,
    owner: String,
    execute: F,
    start_link: StartLink,
    owned: BTreeMap<Link, (Box<dyn LinkHandle>, Instant)>,
}

fn default_command(args: &[&str]) -> Result<String, String> {
    command(args, Duration::from_secs(15))
}

impl Router {
    /// Creates a router using the system `pw-dump` and `pw-cli` commands.
    pub fn new(allowed: BTreeSet<String>) -> Self {
        Self::with_executor(allowed, default_command)
    }
}

impl<F: FnMut(&[&str]) -> Result<String, String>> Router<F> {
    /// Creates a router with a custom command executor.
    ///
    /// The executor is useful for tests and must return the command's stdout
    /// or an error. Link creation still uses the production `pw-cli` backend.
    pub fn with_executor(allowed: BTreeSet<String>, execute: F) -> Self {
        Self::with_backend(
            allowed,
            execute,
            Box::new(|args| {
                Command::new(args[0])
                    .args(&args[1..])
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::inherit())
                    .spawn()
                    .map(|child| Box::new(LinkProcess(child)) as Box<dyn LinkHandle>)
                    .map_err(|error| error.to_string())
            }),
        )
    }

    fn with_backend(allowed: BTreeSet<String>, execute: F, start_link: StartLink) -> Self {
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
            start_link,
            owned: BTreeMap::new(),
        }
    }

    /// Routes every allowlisted microphone when transmitting, or removes all
    /// owned routes when muted.
    pub fn update(&mut self, transmitting: bool) -> Result<BTreeMap<String, Headset>, String> {
        let sources = if transmitting {
            self.allowed.clone()
        } else {
            BTreeSet::new()
        };
        self.update_sources(&sources)
    }

    /// Reconciles owned routes so only the specified headsets' microphones transmit.
    ///
    /// Returns the currently discovered headset topology, or combined errors
    /// from link ownership checks and link creation.
    pub fn update_sources(
        &mut self,
        sources: &BTreeSet<String>,
    ) -> Result<BTreeMap<String, Headset>, String> {
        let snapshot = self.snapshot()?;
        let (headsets, existing) = topology(&snapshot, &self.allowed)?;
        let desired = desired_links(&headsets)
            .into_iter()
            .filter(|(output, _)| {
                headsets.iter().any(|(address, headset)| {
                    sources.contains(address)
                        && headset.sources.iter().any(|port| port.id == *output)
                })
            })
            .collect::<BTreeSet<_>>();
        let mut failures = Vec::new();
        let live = self.owned_links(&snapshot);
        self.owned.retain(|link, (handle, started)| {
            if !desired.contains(link) {
                return false;
            }
            match handle.is_running() {
                Ok(true) if live.contains(link) || started.elapsed() < Duration::from_secs(15) => {
                    true
                }
                Ok(true) => {
                    failures.push(format!("Link did not appear for {} -> {}", link.0, link.1));
                    false
                }
                Ok(false) => {
                    failures.push(format!("Link process exited for {} -> {}", link.0, link.1));
                    false
                }
                Err(error) => {
                    failures.push(error);
                    false
                }
            }
        });
        let properties = serde_json::json!({
            OWNER_PROPERTY: self.owner,
            "object.linger": false,
        })
        .to_string();
        for &(output, input) in desired.difference(&existing) {
            if self.owned.contains_key(&(output, input)) {
                continue;
            }
            match (self.start_link)(&[
                "pw-cli",
                "-m",
                "create-link",
                "-",
                &output.to_string(),
                "-",
                &input.to_string(),
                &properties,
            ]) {
                Ok(handle) => {
                    self.owned.insert((output, input), (handle, Instant::now()));
                }
                Err(error) => {
                    failures.push(format!("Could not link ports {output} -> {input}: {error}"))
                }
            }
        }
        if failures.is_empty() {
            Ok(headsets)
        } else {
            Err(failures.join("; "))
        }
    }

    /// Reads the current allowlisted headset topology and all observed links.
    pub fn inspect(&mut self) -> Result<(BTreeMap<String, Headset>, BTreeSet<Link>), String> {
        let snapshot = self.snapshot()?;
        topology(&snapshot, &self.allowed)
    }

    /// Reads headset topology and only links owned by this router instance.
    pub fn inspect_owned(&mut self) -> Result<(BTreeMap<String, Headset>, BTreeSet<Link>), String> {
        let snapshot = self.snapshot()?;
        let (headsets, _) = topology(&snapshot, &self.allowed)?;
        Ok((headsets, self.owned_links(&snapshot)))
    }

    fn snapshot(&mut self) -> Result<Value, String> {
        serde_json::from_str(&(self.execute)(&["pw-dump"])?)
            .map_err(|e| format!("invalid pw-dump JSON: {e}"))
    }

    fn owned_links(&self, snapshot: &Value) -> BTreeSet<Link> {
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
                    object["info"]["output-port-id"].as_u64()?,
                    object["info"]["input-port-id"].as_u64()?,
                ))
            })
            .collect()
    }

    /// Releases every link created by this router.
    pub fn close(&mut self) {
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
    fn headset_button_controls_only_its_own_microphone() {
        let server = PipeWire::new();
        let mut router = server.router();
        let mut sources = BTreeSet::new();
        router.update_sources(&sources).unwrap();
        assert_eq!(server.links(), 0);
        sources.insert(A.to_string());
        router.update_sources(&sources).unwrap();
        let (_, links) = router.inspect_owned().unwrap();
        assert_eq!(links, [(13, 24), (13, 25)].into());
        sources.clear();
        router.update_sources(&sources).unwrap();
        assert_eq!(server.links(), 0);
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

    #[test]
    fn dashboard_inspection_excludes_external_links() {
        let server = PipeWire::new();
        server.external_link();
        let mut router = server.router();
        router.update(true).unwrap();
        let (headsets, owned) = router.inspect_owned().unwrap();
        assert!(headsets[A].has_duplex_audio());
        assert!(!owned.contains(&(13, 24)));
        assert_eq!(owned.len(), 3);
        router.close();
    }

    #[derive(Clone)]
    struct PipeWire {
        objects: Rc<RefCell<Vec<Value>>>,
        calls: Rc<RefCell<Vec<Vec<String>>>>,
        fail_next: Rc<RefCell<bool>>,
        fail_snapshot: Rc<RefCell<bool>>,
        replace_after_snapshot: Rc<RefCell<bool>>,
    }

    impl PipeWire {
        fn new() -> Self {
            Self {
                objects: Rc::new(RefCell::new(fixture())),
                calls: Rc::default(),
                fail_next: Rc::default(),
                fail_snapshot: Rc::default(),
                replace_after_snapshot: Rc::default(),
            }
        }

        fn execute(&self, args: &[&str]) -> Result<String, String> {
            self.calls
                .borrow_mut()
                .push(args.iter().map(|s| s.to_string()).collect());
            if args[0] == "pw-dump" {
                if *self.fail_snapshot.borrow() {
                    return Err("PipeWire unavailable".into());
                }
                let snapshot = json!(*self.objects.borrow()).to_string();
                if self.replace_after_snapshot.replace(false) {
                    for object in self.objects.borrow_mut().iter_mut() {
                        if object["type"] == "PipeWire:Interface:Link" {
                            object["info"]["props"] = json!({});
                        }
                    }
                }
                return Ok(snapshot);
            }
            if self.fail_next.replace(false) {
                return Err("simulated pw-cli failure".into());
            }
            let mut objects = self.objects.borrow_mut();
            assert_eq!(&args[..4], ["pw-cli", "-m", "create-link", "-"]);
            assert_eq!(args[5], "-");
            let props: Value = serde_json::from_str(args[7]).unwrap();
            assert!(props[OWNER_PROPERTY].as_str().is_some());
            assert_eq!(props["object.linger"], false);
            let output = args[4].parse::<u64>().unwrap();
            let input = args[6].parse::<u64>().unwrap();
            let id = objects
                .iter()
                .filter_map(|object| object["id"].as_u64())
                .max()
                .unwrap_or(0)
                + 1;
            objects.push(json!({"type":"PipeWire:Interface:Link","id":id,"info":{"output-port-id":output,"input-port-id":input,"props":props}}));
            Ok(id.to_string())
        }

        fn router(&self) -> Router<impl FnMut(&[&str]) -> Result<String, String> + use<>> {
            let snapshot = self.clone();
            let links = self.clone();
            Router::with_backend(
                allowed(),
                move |args| snapshot.execute(args),
                Box::new(move |args| {
                    let id = links.execute(args)?.parse::<u64>().unwrap();
                    let object = links
                        .objects
                        .borrow()
                        .iter()
                        .find(|object| object["id"] == id)
                        .unwrap()
                        .clone();
                    Ok(Box::new(TestLink {
                        server: links.clone(),
                        object,
                    }))
                }),
            )
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

    struct TestLink {
        server: PipeWire,
        object: Value,
    }

    impl LinkHandle for TestLink {
        fn is_running(&mut self) -> Result<bool, String> {
            Ok(self.server.objects.borrow().contains(&self.object))
        }
    }

    impl Drop for TestLink {
        fn drop(&mut self) {
            self.server
                .objects
                .borrow_mut()
                .retain(|object| object != &self.object);
        }
    }

    #[test]
    fn owns_only_created_links_and_cleans_up_on_disappearance() {
        let server = PipeWire::new();
        server.external_link();
        let mut router = server.router();
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
        let mut router = server.router();
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
        let mut router = server.router();
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
            let mut router = server.router();
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
    fn replacement_after_snapshot_is_not_deleted_when_muting() {
        let server = PipeWire::new();
        let mut router = server.router();
        router.update(true).unwrap();
        server.replace_after_snapshot.replace(true);
        router.update(false).unwrap();
        assert_eq!(server.links(), 4);
        assert!(router.owned.is_empty());
    }

    #[test]
    fn dropping_router_releases_links_without_delete_commands() {
        let server = PipeWire::new();
        let mut router = server.router();
        router.update(true).unwrap();
        drop(router);
        assert_eq!(server.links(), 0);
        assert!(
            !server
                .calls
                .borrow()
                .iter()
                .any(|args| args.get(1) == Some(&"-d".into()))
        );
    }

    #[test]
    fn separate_routers_do_not_own_each_others_links() {
        let server = PipeWire::new();
        let mut first = server.router();
        let mut second = server.router();
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
        let mut router = server.router();
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

    #[test]
    fn cleanup_does_not_need_a_working_snapshot() {
        let server = PipeWire::new();
        let mut router = server.router();
        router.update(true).unwrap();
        server.fail_snapshot.replace(true);
        assert!(router.update(false).is_err());
        assert_eq!(server.links(), 4);
        router.close();
        assert_eq!(server.links(), 0);
        server.fail_snapshot.replace(false);
        router.update(true).unwrap();
        assert_eq!(server.links(), 4);
    }

    #[test]
    fn exited_link_processes_are_retried() {
        let server = PipeWire::new();
        let mut router = server.router();
        router.update(true).unwrap();
        *server.objects.borrow_mut() = fixture();
        assert!(router.update(true).is_err());
        assert_eq!(server.links(), 4);
        router.close();
        assert_eq!(server.links(), 0);
    }
}
