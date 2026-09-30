"""Discover Bluetooth audio ports and manage intercom links in PipeWire."""

import json
import logging
import subprocess
from dataclasses import dataclass

LOG = logging.getLogger(__name__)


def command(*args, timeout=15):
    return subprocess.run(args, check=True, capture_output=True, text=True,
                          timeout=timeout).stdout


@dataclass(frozen=True)
class Port:
    id: int
    node: int
    channel: str


@dataclass
class Headset:
    address: str
    sources: list[Port]
    sinks: list[Port]


def topology(objects, allowed):
    """Return headsets and current port pairs from a pw-dump snapshot."""
    devices = {}
    nodes = {}
    ports = {}
    links = set()
    for obj in objects:
        info = obj.get("info") or {}
        props = info.get("props") or {}
        kind = obj.get("type", "").rsplit(":", 1)[-1]
        if kind == "Device":
            address = props.get("api.bluez5.address", "").upper()
            if address in allowed:
                devices[obj["id"]] = address
        elif kind == "Node":
            if props.get("media.class") in ("Audio/Source", "Audio/Sink"):
                nodes[obj["id"]] = (props.get("device.id"), props["media.class"])
        elif kind == "Port":
            ports[obj["id"]] = (props.get("node.id"), props.get("port.direction"),
                                 props.get("audio.channel", "MONO"))
        elif kind == "Link":
            output = info.get("output-port-id")
            input_ = info.get("input-port-id")
            if output is not None and input_ is not None:
                links.add((output, input_))

    headsets = {address: Headset(address, [], []) for address in devices.values()}
    for id_, (node_id, direction, channel) in ports.items():
        node = nodes.get(node_id)
        if not node:
            continue
        device_id, media_class = node
        address = devices.get(device_id)
        if not address:
            continue
        port = Port(id_, node_id, channel)
        if media_class == "Audio/Source" and direction == "out":
            headsets[address].sources.append(port)
        elif media_class == "Audio/Sink" and direction == "in":
            headsets[address].sinks.append(port)
    return headsets, links


def desired_links(headsets):
    """Connect each microphone to every other headset's speakers."""
    desired = set()
    for source in headsets.values():
        for sink in headsets.values():
            if source.address == sink.address:
                continue
            for output in source.sources:
                for input_ in sink.sinks:
                    if output.channel == input_.channel or output.channel == "MONO" or input_.channel == "MONO":
                        desired.add((output.id, input_.id))
    return desired


class Router:
    def __init__(self, allowed, execute=command):
        self.allowed = {address.upper() for address in allowed}
        self.execute = execute
        self.owned = set()

    def update(self):
        headsets, existing = topology(json.loads(self.execute("pw-dump")), self.allowed)
        desired = desired_links(headsets)
        for output, input_ in sorted(self.owned - desired):
            if (output, input_) in existing:
                self.execute("pw-link", "-d", str(output), str(input_))
            self.owned.discard((output, input_))
        for output, input_ in sorted(desired - existing):
            try:
                self.execute("pw-link", str(output), str(input_))
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
                LOG.warning("Could not link ports %s -> %s: %s", output, input_, error)
            else:
                self.owned.add((output, input_))
        return headsets

    def close(self):
        for output, input_ in sorted(self.owned):
            try:
                self.execute("pw-link", "-d", str(output), str(input_))
            except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
                LOG.warning("Could not unlink ports %s -> %s: %s", output, input_, error)
        self.owned.clear()
