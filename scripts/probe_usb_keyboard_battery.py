#!/usr/bin/env python3
"""Probe ASUS Zenbook Duo keyboard (0b05:1bf2) for battery / power information over USB.

The daemon reads battery % from BlueZ org.bluez.Battery1 when Bluetooth is connected.
This script checks every other plausible Linux path while the keyboard is on USB:

  - /sys/class/power_supply/*
  - USB device sysfs attributes
  - HID feature reports on vendor interface 4 (report ID 0x5a, same as zenbook-duo-daemon)
  - Interrupt IN endpoint 0x85 traffic (vendor hotkeys — may include unknown report types)
  - BlueZ D-Bus (for comparison when BT is up)

Requires: pyusb (`pip install pyusb`). Run as root or with udev access to the device.

Usage:
  sudo python3 scripts/probe_usb_keyboard_battery.py
  sudo python3 scripts/probe_usb_keyboard_battery.py --listen 15
"""

from __future__ import annotations

import argparse
import glob
import os
import struct
import subprocess
import sys
import time
from pathlib import Path

VENDOR_ID = 0x0B05
PRODUCT_ID = 0x1BF2
VENDOR_INTERFACE = 4
INTERRUPT_EP = 0x85
REPORT_ID = 0x5A

# Feature report prefixes used by zenbook-duo-daemon (keyboard_usb.rs)
FEATURE_PROBES: list[tuple[str, bytes]] = [
    ("fn_lock_off", bytes.fromhex("5ad04e00000000000000000000000000")),
    ("fn_lock_on", bytes.fromhex("5ad04e01000000000000000000000000")),
    ("backlight_off", bytes.fromhex("5abac5c4000000000000000000000000")),
    ("backlight_low", bytes.fromhex("5abac5c4010000000000000000000000")),
    ("mic_mute_off", bytes.fromhex("5ad07c00000000000000000000000000")),
    ("mic_mute_on", bytes.fromhex("5ad07c01000000000000000000000000")),
    ("battery_query_guess_a", bytes.fromhex("5ab10000000000000000000000000000")),
    ("battery_query_guess_b", bytes.fromhex("5a00000000000000000000000000000000")),
    ("battery_query_guess_c", bytes.fromhex("5a01000000000000000000000000000000")),
]


def require_pyusb():
    try:
        import usb.core  # noqa: F401
        import usb.util  # noqa: F401
    except ImportError:
        print("pyusb is required: pip install pyusb", file=sys.stderr)
        sys.exit(1)


def hex_line(data: bytes) -> str:
    return data.hex(" ")


def scan_power_supply() -> None:
    print("\n=== /sys/class/power_supply (ASUS / keyboard / Primax) ===")
    found = False
    for path in sorted(glob.glob("/sys/class/power_supply/*")):
        name_path = os.path.join(path, "name")
        try:
            name = Path(name_path).read_text().strip()
        except OSError:
            continue
        blob = name.lower()
        if not any(k in blob for k in ("asus", "keyboard", "primax", "zenbook")):
            continue
        found = True
        print(f"  {path} name={name!r}")
        for attr in (
            "type",
            "status",
            "capacity",
            "capacity_level",
            "present",
            "online",
            "scope",
            "manufacturer",
            "model_name",
        ):
            ap = os.path.join(path, attr)
            if os.path.isfile(ap):
                try:
                    print(f"    {attr}: {Path(ap).read_text().strip()}")
                except OSError as e:
                    print(f"    {attr}: <read error {e}>")
    if not found:
        print("  (no matching power_supply entries)")


def find_keyboard_sysfs_nodes() -> list[str]:
    nodes: list[str] = []
    for path in glob.glob("/sys/bus/usb/devices/*"):
        if not os.path.isdir(path):
            continue
        vid_path = os.path.join(path, "idVendor")
        pid_path = os.path.join(path, "idProduct")
        if not os.path.isfile(vid_path) or not os.path.isfile(pid_path):
            continue
        try:
            vid = int(Path(vid_path).read_text().strip(), 16)
            pid = int(Path(pid_path).read_text().strip(), 16)
        except (OSError, ValueError):
            continue
        if vid == VENDOR_ID and pid == PRODUCT_ID:
            nodes.append(path)
    return sorted(nodes)


def scan_usb_sysfs() -> None:
    print("\n=== USB sysfs (0b05:1bf2) ===")
    nodes = find_keyboard_sysfs_nodes()
    if not nodes:
        print("  keyboard not present on USB")
        return
    for node in nodes:
        print(f"  node: {node}")
        for attr in (
            "devpath",
            "busnum",
            "devnum",
            "manufacturer",
            "product",
            "bNumConfigurations",
            "bDeviceClass",
        ):
            ap = os.path.join(node, attr)
            if os.path.isfile(ap):
                print(f"    {attr}: {Path(ap).read_text().strip()}")


def scan_bluez_battery() -> None:
    print("\n=== BlueZ Battery1 (system bus) ===")
    try:
        out = subprocess.run(
            [
                "busctl",
                "tree",
                "org.bluez",
                "--list",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
    except FileNotFoundError:
        print("  busctl not found")
        return
    keyboard_paths = [
        line.split()[0]
        for line in out.stdout.splitlines()
        if "dev_" in line and ("ASUS" in line.upper() or "KEYBOARD" in line.upper())
    ]
    if not keyboard_paths:
        print("  no ASUS keyboard device paths in busctl tree")
        return
    for path in keyboard_paths:
        print(f"  device: {path}")
        for prop in ("Connected", "Name"):
            r = subprocess.run(
                [
                    "busctl",
                    "get-property",
                    "org.bluez",
                    path,
                    "org.bluez.Device1",
                    prop,
                ],
                capture_output=True,
                text=True,
                check=False,
            )
            if r.returncode == 0:
                print(f"    Device1.{prop}: {r.stdout.strip()}")
        r = subprocess.run(
            [
                "busctl",
                "get-property",
                "org.bluez",
                path,
                "org.bluez.Battery1",
                "Percentage",
            ],
            capture_output=True,
            text=True,
            check=False,
        )
        if r.returncode == 0:
            print(f"    Battery1.Percentage: {r.stdout.strip()}")
        else:
            print("    Battery1: not present (typical when USB-only / BT disconnected)")


def hid_get_feature(dev, report_id: int, length: int = 32) -> bytes | None:
    import usb.util

    # HID class GET_REPORT: bmRequestType=0xA1, bRequest=0x01, wValue=(report_type<<8)|report_id
    for report_type, label in ((0x03, "feature"), (0x02, "input"), (0x01, "output")):
        try:
            data = dev.ctrl_transfer(
                0xA1,
                0x01,
                (report_type << 8) | report_id,
                VENDOR_INTERFACE,
                length,
                timeout=1000,
            )
            if data:
                print(f"    GET_REPORT {label} id=0x{report_id:02x}: {hex_line(bytes(data))}")
                return bytes(data)
        except usb.util.USBError as e:
            print(f"    GET_REPORT {label} id=0x{report_id:02x}: {e}")
    return None


def hid_set_feature(dev, payload: bytes) -> bytes | None:
    import usb.util

    if len(payload) < 2:
        return None
    report_id = payload[0]
    try:
        # SET_REPORT feature
        dev.ctrl_transfer(
            0x21,
            0x09,
            (0x03 << 8) | report_id,
            VENDOR_INTERFACE,
            payload,
            timeout=1000,
        )
        print(f"    SET_REPORT feature sent: {hex_line(payload[:16])}…")
    except usb.util.USBError as e:
        print(f"    SET_REPORT failed: {e}")
        return None
    time.sleep(0.05)
    return hid_get_feature(dev, report_id)


def looks_like_battery_pct(data: bytes) -> list[str]:
    hints: list[str] = []
    for i, b in enumerate(data):
        if 0 < b <= 100:
            hints.append(f"byte[{i}]={b} (possible %)")
    return hints


def probe_usb_hid(listen_secs: float) -> None:
    import usb.core
    import usb.util

    print("\n=== USB HID vendor interface (pyusb) ===")
    dev = usb.core.find(idVendor=VENDOR_ID, idProduct=PRODUCT_ID)
    if dev is None:
        print("  device not found (is the keyboard on USB?)")
        return

    try:
        if dev.is_kernel_driver_active(VENDOR_INTERFACE):
            dev.detach_kernel_driver(VENDOR_INTERFACE)
            print(f"  detached kernel driver on interface {VENDOR_INTERFACE}")
    except (usb.core.USBError, NotImplementedError) as e:
        print(f"  kernel driver detach: {e}")

    try:
        usb.util.claim_interface(dev, VENDOR_INTERFACE)
    except usb.core.USBError as e:
        if getattr(e, "errno", None) == 16:
            print(
                "  interface busy (zenbook-duo-daemon or kernel holds it).\n"
                "  Stop the daemon for a full HID probe:\n"
                "    sudo systemctl stop zenbook-duo-daemon.service\n"
                "  then re-run this script."
            )
            return
        raise
    print(f"  claimed interface {VENDOR_INTERFACE}")

    print("  GET_REPORT sweep (report ids 0x00–0xff, first hits only):")
    for rid in range(0x00, 0x100):
        data = hid_get_feature(dev, rid, length=64)
        if data:
            hints = looks_like_battery_pct(data)
            for h in hints:
                print(f"      hint: {h}")

    print("  SET_REPORT probes (daemon-known + battery guesses):")
    for label, payload in FEATURE_PROBES:
        print(f"  -- {label}")
        reply = hid_set_feature(dev, payload)
        if reply:
            hints = looks_like_battery_pct(reply)
            for h in hints:
                print(f"      hint: {h}")

    cfg = dev.get_active_configuration()
    intf = cfg[(VENDOR_INTERFACE, 0)]
    ep = usb.util.find_descriptor(intf, custom_match=lambda e: e.bEndpointAddress == INTERRUPT_EP)
    if ep is None:
        print(f"  endpoint 0x{INTERRUPT_EP:02x} not found")
        usb.util.release_interface(dev, VENDOR_INTERFACE)
        return

    print(f"  listening on interrupt IN 0x{INTERRUPT_EP:02x} for {listen_secs:.0f}s …")
    deadline = time.monotonic() + listen_secs
    seen: set[str] = set()
    while time.monotonic() < deadline:
        try:
            chunk = ep.read(ep.wMaxPacketSize, timeout=500)
        except usb.core.USBError:
            continue
        data = bytes(chunk)
        key = hex_line(data)
        if key in seen:
            continue
        seen.add(key)
        print(f"    IN: {key}")
        hints = looks_like_battery_pct(data)
        for h in hints:
            print(f"      hint: {h}")

    usb.util.release_interface(dev, VENDOR_INTERFACE)
    print(f"  unique interrupt reports: {len(seen)}")


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--listen",
        type=float,
        default=10.0,
        help="seconds to listen on vendor interrupt endpoint (default: 10)",
    )
    parser.add_argument(
        "--skip-usb",
        action="store_true",
        help="skip pyusb (sysfs + BlueZ only)",
    )
    args = parser.parse_args()

    print("ASUS Zenbook Duo keyboard battery / power probe")
    print(f"  target USB id: {VENDOR_ID:04x}:{PRODUCT_ID:04x}")

    scan_power_supply()
    scan_usb_sysfs()
    scan_bluez_battery()

    if not args.skip_usb:
        require_pyusb()
        probe_usb_hid(args.listen)

    print("\n=== Summary ===")
    print(
        "  If BlueZ Battery1 is missing and power_supply is empty, live battery % "
        "is not exposed over USB on this stack — only BT GATT Battery Service when connected."
    )


if __name__ == "__main__":
    main()
