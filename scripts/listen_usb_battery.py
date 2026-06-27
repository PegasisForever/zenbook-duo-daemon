#!/usr/bin/env python3
"""Listen for ASUS keyboard USB vendor battery reports (0x5a 0x3d pct status)."""

from __future__ import annotations

import sys
import time

VENDOR_ID = 0x0B05
PRODUCT_ID = 0x1BF2
VENDOR_INTERFACE = 4
INTERRUPT_EP = 0x85


def main() -> None:
    try:
        import usb.core
        import usb.util
    except ImportError:
        print("pip install pyusb", file=sys.stderr)
        sys.exit(1)

    listen_secs = float(sys.argv[1]) if len(sys.argv) > 1 else 30.0

    dev = usb.core.find(idVendor=VENDOR_ID, idProduct=PRODUCT_ID)
    if dev is None:
        print("keyboard not on USB")
        sys.exit(1)

    devpath = "?"
    try:
        for path in ["/sys/bus/usb/devices"]:
            import glob
            import os
            from pathlib import Path

            for node in glob.glob("/sys/bus/usb/devices/*"):
                vid = Path(node) / "idVendor"
                if vid.is_file() and Path(vid).read_text().strip() == "0b05":
                    pid = Path(node) / "idProduct"
                    if pid.is_file() and Path(pid).read_text().strip() == "1bf2":
                        dp = Path(node) / "devpath"
                        if dp.is_file():
                            devpath = Path(dp).read_text().strip()
                            break
    except OSError:
        pass

    print(f"Keyboard on USB devpath={devpath} (6=pogo, 4=side)")

    try:
        if dev.is_kernel_driver_active(VENDOR_INTERFACE):
            dev.detach_kernel_driver(VENDOR_INTERFACE)
    except (usb.core.USBError, NotImplementedError) as e:
        print(f"detach kernel driver: {e}")

    try:
        usb.util.claim_interface(dev, VENDOR_INTERFACE)
    except usb.core.USBError as e:
        print(f"claim interface failed: {e}")
        print("Stop zenbook-duo-daemon.service first")
        sys.exit(1)

    cfg = dev.get_active_configuration()
    intf = cfg[(VENDOR_INTERFACE, 0)]
    ep = usb.util.find_descriptor(intf, custom_match=lambda e: e.bEndpointAddress == INTERRUPT_EP)
    if ep is None:
        print("endpoint 0x85 not found")
        sys.exit(1)

    print(f"Listening {listen_secs:.0f}s on interrupt IN 0x{INTERRUPT_EP:02x} for 5a 3d battery…")
    deadline = time.monotonic() + listen_secs
    battery_hits = 0
    while time.monotonic() < deadline:
        try:
            chunk = ep.read(ep.wMaxPacketSize, timeout=1000)
        except usb.core.USBError:
            continue
        data = bytes(chunk)
        if len(data) >= 4 and data[0] == 0x5A and data[1] == 0x3D:
            battery_hits += 1
            print(f"  BATTERY: pct={data[2]}% status=0x{data[3]:02x} raw={data.hex(' ')}")
        elif data[0] == 0x5A:
            print(f"  vendor: {data.hex(' ')}")

    usb.util.release_interface(dev, VENDOR_INTERFACE)
    print(f"Done. battery reports (5a 3d): {battery_hits}")


if __name__ == "__main__":
    main()
