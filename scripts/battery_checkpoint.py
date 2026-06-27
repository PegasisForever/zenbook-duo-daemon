#!/usr/bin/env python3
"""Log BlueZ battery + USB vendor HID reports for battery reverse-engineering.

Usage:
  sudo python3 scripts/battery_checkpoint.py --label baseline
  sudo python3 scripts/battery_checkpoint.py --label after_usb --usb --listen 5
"""

from __future__ import annotations

import argparse
import csv
import subprocess
import sys
import time
from datetime import datetime, timezone
from pathlib import Path

VENDOR_ID = 0x0B05
PRODUCT_ID = 0x1BF2
VENDOR_INTERFACE = 4
INTERRUPT_EP = 0x85
BT_PATH = "/org/bluez/hci0/dev_FD_0C_1A_6E_06_A3"
USB_REPORT_IDS = (0x03, 0x41, 0x42, 0x43, 0xB1, 0x5A)
DEFAULT_CSV = Path("/tmp/zenbook_battery_re.csv")


def bluez_state() -> dict[str, str]:
    out: dict[str, str] = {}
    for iface, prop in (
        ("org.bluez.Device1", "Connected"),
        ("org.bluez.Device1", "Name"),
        ("org.bluez.Battery1", "Percentage"),
    ):
        r = subprocess.run(
            ["busctl", "get-property", "org.bluez", BT_PATH, iface, prop],
            capture_output=True,
            text=True,
            check=False,
        )
        if r.returncode == 0:
            out[f"{iface.split('.')[-1]}_{prop}"] = r.stdout.strip()
        else:
            out[f"{iface.split('.')[-1]}_{prop}"] = "n/a"
    usb = subprocess.run(["lsusb", "-d", "0b05:1bf2"], capture_output=True, text=True)
    out["usb_present"] = "yes" if usb.returncode == 0 and usb.stdout.strip() else "no"
    return out


def get_feature(dev, report_id: int, length: int = 32) -> str:
    import usb.util

    try:
        data = dev.ctrl_transfer(
            0xA1,
            0x01,
            (0x03 << 8) | report_id,
            VENDOR_INTERFACE,
            length,
            timeout=1000,
        )
        return bytes(data).hex(" ")
    except usb.util.USBError as e:
        return f"ERR:{e}"


def listen_interrupt(dev, seconds: float) -> list[str]:
    import usb.util

    cfg = dev.get_active_configuration()
    intf = cfg[(VENDOR_INTERFACE, 0)]
    ep = usb.util.find_descriptor(intf, custom_match=lambda e: e.bEndpointAddress == INTERRUPT_EP)
    if ep is None:
        return ["no_ep"]
    seen: list[str] = []
    deadline = time.monotonic() + seconds
    while time.monotonic() < deadline:
        try:
            chunk = bytes(ep.read(ep.wMaxPacketSize, timeout=500))
        except usb.core.USBError:
            continue
        line = chunk.hex(" ")
        if line not in seen:
            seen.append(line)
    return seen


def usb_probe(listen_secs: float) -> dict[str, str]:
    import usb.core
    import usb.util

    out: dict[str, str] = {}
    dev = usb.core.find(idVendor=VENDOR_ID, idProduct=PRODUCT_ID)
    if dev is None:
        out["usb_hid"] = "device_not_found"
        return out

    try:
        if dev.is_kernel_driver_active(VENDOR_INTERFACE):
            dev.detach_kernel_driver(VENDOR_INTERFACE)
    except (usb.core.USBError, NotImplementedError):
        pass

    usb.util.claim_interface(dev, VENDOR_INTERFACE)
    try:
        for rid in USB_REPORT_IDS:
            out[f"get_0x{rid:02x}"] = get_feature(dev, rid)
        if listen_secs > 0:
            packets = listen_interrupt(dev, listen_secs)
            out["interrupt_in"] = " | ".join(packets) if packets else "(none)"
    finally:
        usb.util.release_interface(dev, VENDOR_INTERFACE)
    return out


def append_row(csv_path: Path, label: str, fields: dict[str, str]) -> None:
    row = {
        "utc": datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
        "label": label,
        **fields,
    }
    write_header = not csv_path.exists()
    with csv_path.open("a", newline="") as f:
        w = csv.DictWriter(f, fieldnames=list(row.keys()))
        if write_header:
            w.writeheader()
        w.writerow(row)


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--label", required=True, help="checkpoint name")
    parser.add_argument("--usb", action="store_true", help="probe USB HID interface 4")
    parser.add_argument("--listen", type=float, default=0.0, help="interrupt listen seconds")
    parser.add_argument("--csv", type=Path, default=DEFAULT_CSV)
    args = parser.parse_args()

    fields = bluez_state()
    if args.usb:
        try:
            import usb.core  # noqa: F401
        except ImportError:
            print("pyusb required for --usb", file=sys.stderr)
            sys.exit(1)
        fields.update(usb_probe(args.listen))

    append_row(args.csv, args.label, fields)

    print(f"Logged checkpoint '{args.label}' -> {args.csv}")
    for k, v in fields.items():
        print(f"  {k}: {v}")


if __name__ == "__main__":
    main()
