#!/usr/bin/env python3
"""Poll BlueZ keyboard battery until target percentage is reached."""

from __future__ import annotations

import argparse
import subprocess
import sys
import time
from datetime import datetime, timezone

BT_PATH = "/org/bluez/hci0/dev_FD_0C_1A_6E_06_A3"


def read_pct() -> tuple[int | None, str]:
    r = subprocess.run(
        [
            "busctl",
            "get-property",
            "org.bluez",
            BT_PATH,
            "org.bluez.Battery1",
            "Percentage",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    raw = r.stdout.strip() if r.returncode == 0 else f"n/a ({r.stderr.strip()})"
    if r.returncode != 0:
        return None, raw
    parts = raw.split()
    if len(parts) != 2 or parts[0] != "y":
        return None, raw
    return int(parts[1]), raw


def connected() -> bool:
    r = subprocess.run(
        [
            "busctl",
            "get-property",
            "org.bluez",
            BT_PATH,
            "org.bluez.Device1",
            "Connected",
        ],
        capture_output=True,
        text=True,
        check=False,
    )
    return r.returncode == 0 and "true" in r.stdout


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--target", type=int, default=95, help="stop at or below this percent")
    parser.add_argument("--interval", type=float, default=10.0, help="seconds between polls")
    parser.add_argument("--log", type=str, default="/tmp/zenbook_battery_poll.log")
    args = parser.parse_args()

    logf = open(args.log, "a", encoding="utf-8")
    print(f"Watching BlueZ battery (target <= {args.target}%, every {args.interval}s)")
    print(f"Log: {args.log}")

    last: int | None = None
    while True:
        ts = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")
        if not connected():
            line = f"[{ts}] BT disconnected — waiting for reconnect…"
            print(line)
            logf.write(line + "\n")
            logf.flush()
            time.sleep(args.interval)
            continue

        pct, raw = read_pct()
        if pct is None:
            line = f"[{ts}] Battery1 unreadable: {raw}"
        else:
            delta = "" if last is None else f" (Δ{pct - last:+d})"
            line = f"[{ts}] Battery1: {pct}%{delta}"
            last = pct
        print(line)
        logf.write(line + "\n")
        logf.flush()

        if pct is not None and pct <= args.target:
            print(f"TARGET REACHED: {pct}% (target was <={args.target}%)")
            logf.write(f"TARGET REACHED: {pct}%\n")
            logf.flush()
            logf.close()
            return

        time.sleep(args.interval)


if __name__ == "__main__":
    try:
        main()
    except KeyboardInterrupt:
        sys.exit(130)
