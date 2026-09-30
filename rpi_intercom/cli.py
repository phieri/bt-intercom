"""Command-line interface for the Bluetooth intercom."""

import argparse
import logging
import re
import subprocess
import time

from .router import Router, command

ADDRESS = re.compile(r"[0-9A-Fa-f]{2}(?::[0-9A-Fa-f]{2}){5}\Z")


def address(value):
    if not ADDRESS.fullmatch(value):
        raise argparse.ArgumentTypeError("expected a Bluetooth MAC address (XX:XX:XX:XX:XX:XX)")
    return value.upper()


def parser():
    cli = argparse.ArgumentParser(description="Route audio between trusted Bluetooth headsets")
    actions = cli.add_subparsers(dest="action", required=True)
    scan = actions.add_parser("scan", help="discover nearby Bluetooth devices")
    scan.add_argument("--seconds", type=int, default=15)
    pair = actions.add_parser("pair", help="pair, trust and connect a headset")
    pair.add_argument("address", type=address)
    run = actions.add_parser("run", help="connect and route configured headsets")
    run.add_argument("addresses", nargs="+", type=address, help="paired headset MAC addresses")
    run.add_argument("--interval", type=float, default=2.0, help="seconds between topology checks")
    run.add_argument("--connect", action="store_true", help="connect headsets at startup")
    return cli


def main(argv=None):
    args = parser().parse_args(argv)
    logging.basicConfig(level=logging.INFO, format="%(levelname)s: %(message)s")
    try:
        if args.action == "scan":
            if not 1 <= args.seconds <= 300:
                parser().error("--seconds must be between 1 and 300")
            print(command("bluetoothctl", "--timeout", str(args.seconds), "scan", "on",
                          timeout=args.seconds + 5))
        elif args.action == "pair":
            for action in ("pair", "trust", "connect"):
                print(command("bluetoothctl", "--timeout", "60", action, args.address, timeout=65))
        else:
            if args.interval <= 0:
                parser().error("--interval must be positive")
            router = Router(args.addresses)
            if args.connect:
                for device in router.allowed:
                    try:
                        command("bluetoothctl", "--timeout", "30", "connect", device, timeout=35)
                    except (subprocess.CalledProcessError, subprocess.TimeoutExpired) as error:
                        logging.warning("Could not connect %s: %s", device, error)
            try:
                while True:
                    headsets = router.update()
                    active = sum(bool(h.sources and h.sinks) for h in headsets.values())
                    logging.info("%s/%s headsets with duplex audio", active, len(router.allowed))
                    time.sleep(args.interval)
            finally:
                router.close()
    except (OSError, subprocess.CalledProcessError, subprocess.TimeoutExpired, ValueError) as error:
        parser().exit(1, f"rpi-intercom: {error}\n")


if __name__ == "__main__":
    main()
