import json
import subprocess
import unittest

from rpi_intercom.cli import address, parser
from rpi_intercom.router import Router, desired_links, topology


def obj(kind, id_, props=None, **info):
    return {"type": f"PipeWire:Interface:{kind}", "id": id_,
            "info": {"props": props or {}, **info}}


def headset(base, address):
    return [
        obj("Device", base, {"api.bluez5.address": address}),
        obj("Node", base + 1, {"device.id": str(base), "media.class": "Audio/Source"}),
        obj("Node", base + 2, {"device.id": str(base), "media.class": "Audio/Sink"}),
        obj("Port", base + 3, {"node.id": str(base + 1), "port.direction": "out",
                               "audio.channel": "MONO"}),
        obj("Port", base + 4, {"node.id": str(base + 2), "port.direction": "in",
                               "audio.channel": "FL"}),
        obj("Port", base + 5, {"node.id": str(base + 2), "port.direction": "in",
                               "audio.channel": "FR"}),
    ]


class TopologyTests(unittest.TestCase):
    def setUp(self):
        self.a = "AA:BB:CC:DD:EE:01"
        self.b = "AA:BB:CC:DD:EE:02"
        self.objects = headset(10, self.a) + headset(20, self.b)

    def test_full_duplex_routes_without_self_links(self):
        headsets, links = topology(self.objects, {self.a, self.b})
        self.assertEqual(links, set())
        self.assertEqual(desired_links(headsets),
                         {(13, 24), (13, 25), (23, 14), (23, 15)})

    def test_only_allowlisted_devices_route(self):
        headsets, _ = topology(self.objects, {self.a})
        self.assertEqual(set(headsets), {self.a})
        self.assertEqual(desired_links(headsets), set())

    def test_ignores_non_bluetooth_and_non_audio_ports(self):
        self.objects += [
            obj("Node", 31, {"device.id": "10", "media.class": "Stream/Output/Audio"}),
            obj("Port", 32, {"node.id": "31", "port.direction": "out"}),
            obj("Device", 40, {"device.name": "alsa_card"}),
        ]
        headsets, _ = topology(self.objects, {self.a, self.b})
        self.assertEqual(len(headsets[self.a].sources), 1)

    def test_matches_stereo_channels(self):
        self.objects[3]["info"]["props"]["audio.channel"] = "FL"
        headsets, _ = topology(self.objects, {self.a, self.b})
        self.assertNotIn((13, 25), desired_links(headsets))

    def test_existing_links(self):
        self.objects.append(obj("Link", 100, **{"output-port-id": 13, "input-port-id": 24}))
        _, links = topology(self.objects, {self.a, self.b})
        self.assertEqual(links, {(13, 24)})


class RouterTests(unittest.TestCase):
    def setUp(self):
        self.a = "AA:BB:CC:DD:EE:01"
        self.b = "AA:BB:CC:DD:EE:02"
        self.objects = headset(10, self.a) + headset(20, self.b)
        self.calls = []

        def execute(*args):
            self.calls.append(args)
            if args[0] == "pw-dump":
                return json.dumps(self.objects)
            return ""

        self.router = Router({self.a, self.b}, execute)

    def test_creates_and_cleans_up_only_owned_links(self):
        self.objects.append(obj("Link", 100, **{"output-port-id": 13, "input-port-id": 24}))
        self.router.update()
        self.assertNotIn(("pw-link", "13", "24"), self.calls)
        self.assertEqual(len(self.router.owned), 3)
        self.router.close()
        self.assertNotIn(("pw-link", "-d", "13", "24"), self.calls)
        self.assertEqual(self.router.owned, set())

    def test_disappearance_clears_owned_links(self):
        self.router.update()
        self.objects = headset(10, self.a)
        self.router.update()
        self.assertEqual(self.router.owned, set())

    def test_link_failure_can_retry(self):
        attempts = 0

        def failing(*args):
            nonlocal attempts
            if args[0] == "pw-dump":
                return json.dumps(self.objects)
            attempts += 1
            if attempts == 1:
                raise subprocess.CalledProcessError(1, args)
            return ""

        router = Router({self.a, self.b}, failing)
        router.update()
        self.assertEqual(len(router.owned), 3)
        router.update()
        self.assertEqual(len(router.owned), 4)


class CliTests(unittest.TestCase):
    def test_address_rejected_before_invoking_bluetoothctl(self):
        with self.assertRaises(SystemExit):
            parser().parse_args(["pair", "invalid; rm -rf /"])
        self.assertEqual(address("aa:bb:cc:dd:ee:ff"), "AA:BB:CC:DD:EE:FF")


if __name__ == "__main__":
    unittest.main()
