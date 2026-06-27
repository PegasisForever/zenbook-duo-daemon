# Zenbook Duo GNOME Shell Extension

A GNOME Shell extension that surfaces the detachable keyboard battery state
from the `zenbook-duo-daemon` directly in the GNOME UI.

## What it adds

| Location | What you see |
|---|---|
| **Top-bar status area** (right side) | Keyboard icon + battery `%`, with a separate accent-colored `↓` while discharging over Bluetooth. The percentage color follows daemon-backed discharging (`25/10/5` by default) and charging (`50/75/100` by default) milestone settings. |
| **Quick Settings panel** (the dropdown) | A compact **Zen Duo** tile with stacked dual-display icon, subtitle like `BT ( 43% ↓ )`, `USB ( 23% ⚡ )`, `POGO ( 29% ⚡ )`, or `Detached ( 24% )` with the cached battery portion dimmed, plus health, keyboard, and rotation status rows. |
| **Quick Settings actions** | **Bottom/Left/Right/Top display** switch for desired on/off state, **Top display as primary** switch, **Stylus match displays** switch, a compact **Keyboard backlight** header with a single row of larger icon-and-label buttons underneath (`Off / Low / Medium / High`, reusing the medium icon for both middle levels), and **Microphone** switch (`on` = unmuted, `off` = muted; the keyboard LED follows the observed backend state). The secondary-display row stays visible and dims while pogo-attached, and switch rows stay open after toggling. |
| **Extension settings** | The extension now exposes a **Settings** window that edits daemon-backed battery thresholds and color values, persisting them into the root daemon config for both the extension and battery notifications. |
| **Daemon/session status** | In the tile menu, shows `Root on/off` and session link status (`linked`, `quiet`, `not registered`, or `daemon update pending`). `quiet` means the registered session stopped heartbeating on D-Bus, not merely that nothing changed. |

> The stylus-mapping switch is shown only when the daemon exposes the tablet mapping mode field; otherwise it stays disabled as "daemon pending".

## D-Bus interface consumed

| Field | Value |
|---|---|
| Bus | **system** |
| Service | `asus.zenbook.duo` |
| Path | `/asus/zenbook/duo/State` |
| Interface | `asus.zenbook.duo.State` |

Properties read: `KeyboardUsbConnected`, `KeyboardPogoDocked`,
`BluetoothConnected`, `KeyboardBatteryPresent`, `KeyboardBatteryPercentage`,
`KeyboardBatteryCharging`, `KeyboardBatteryFull`, `BatteryUiConfigJson`, `DesiredPrimary`,
`DesiredSecondaryEnabled`, `DesiredDisplayAttachment`, `DesiredDisplayLayout`,
`DisplayBrightness`, `DisplayApplyPaused`, `TabletMappingEnabled`,
`TabletMappingMode`, `TabletMappingApplyNonce`, `SessionRegistered`,
`SessionOwner`, `SessionId`, `SessionLastSeenUsec`, `SessionQuiet`,
`MicMuteLed`, `KeyboardBacklightLevel`.

Methods called (from Quick Settings actions): `SetSecondaryDisplayDesired(bool)`,
`SetDesiredPrimary(string)`, `SetTabletMappingMode(string)`, `SetMicMute(bool)`,
`SetKeyboardBacklightLevel(byte)`, `SetBatteryUiConfigJson(string)` (from the prefs window).

## Requirements

- GNOME Shell 45 or newer
- `zenbook-duo-daemon` running on the system bus

## Installation

```bash
cd gnome-extension
bash install.sh --enable
# If GNOME does not detect a freshly added UUID immediately, log out/in once.
```

## Debugging

```bash
# Stream GNOME Shell logs while testing
journalctl /usr/bin/gnome-shell -f | grep -i zenbook

# Inspect live D-Bus property values
busctl --system get-property asus.zenbook.duo \
       /asus/zenbook/duo/State \
       asus.zenbook.duo.State \
       KeyboardBatteryPercentage
```

## Extension structure

```
gnome-extension/
├── install.sh
├── README.md
└── zenbook-duo@zenbook-duo-daemon/
    ├── metadata.json    — UUID, name, supported GNOME versions
    ├── extension.js     — Main extension code (ES module, GNOME 45+ API)
    └── stylesheet.css   — Minimal CSS for the panel label
```
