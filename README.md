# ASUS Zenbook Duo Daemon

This is a daemon that runs on the Zenbook Duo laptop to handle the keyboard and secondary display under linux.

AI Generated Wiki: [![Ask DeepWiki](https://deepwiki.com/badge.svg)](https://deepwiki.com/PegasisForever/zenbook-duo-daemon)

## Device Support

- ✅ Zenbook Duo 2025
- ✅ Zenbook Duo 2024

## Distribution Support

- ✅ Ubuntu 25.10 6.17.0-8-generic
- ✅ Fedora 42 6.17.13-200.fc42.x86_64
- ⚠️ NixOS: see this [fork](https://github.com/0Tick/zenbook-duo-daemon/tree/copilot/convert-to-nix-flake)
- ⚠️ Other distributions may work, but are not tested

## Features

- ✅ Enable secondary display when keyboard is detached
- ✅ Disable keyboard backlight when idle
- ✅ Brightness sync between primary and secondary display
- ✅ Remap keys to run custom commands or key combinations

| Keyboard Function               | Wired Mode | Bluetooth Mode | Default Mapping              | Remappable via config file? |
| ------------------------------- | ---------- | -------------- | ---------------------------- | --------------------------- |
| Mute Key                        | ✅         | ✅             | `KEY_MUTE`                   | ❌                          |
| Volume Down Key                 | ✅         | ✅             | `KEY_VOLUMEDOWN`             | ❌                          |
| Volume Up Key                   | ✅         | ✅             | `KEY_VOLUMEUP`               | ❌                          |
| Keyboard Backlight Key          | ✅         | ✅             | `KEY_BACKLIGHT`              | ✅                          |
| Keyboard Backlight Control      | ✅         | ❌ (1)         | N/A                          | ✅                          |
| Brightness Down Key             | ✅         | ✅             | `KEY_BRIGHTNESSDOWN`         | ✅                          |
| Brightness Up Key               | ✅         | ✅             | `KEY_BRIGHTNESSUP`           | ✅                          |
| Extended Display Mode Key       | ✅         | ✅             | `KEY_LEFT_META + KEY_P`      | ❌                          |
| Swap Up Down Display Key        | ✅         | ✅             | None                         | ✅                          |
| Microphone Mute Key             | ✅         | ✅             | `KEY_MICMUTE`                | ✅                          |
| Microphone Mute Key LED Control | ✅         | ❌ (2)         | N/A                          | ✅                          |
| Emoji Picker Key                | ✅         | ✅             | `KEY_LEFTCTRL + KEY_DOT` (3) | ✅                          |
| MyASUS Key                      | ✅         | ✅             | None                         | ✅                          |
| Toggle Secondary Display Key    | ✅         | ✅             | Toggle Secondary Display     | ✅                          |
| Fn + Function Keys              | ✅         | ✅             | F1 - F12                     | ❌                          |

1. Should be possible, the packet capture file under windows is at `pcap/bt_change_backlight.pcapng`
2. Should be possible, the packet capture file under windows is at `pcap/bt_micmute_led.pcapng`
3. This key combination only works for GTK apps in GNOME.

## Installation

```bash
# Upgrade or install the latest release from GitHub
curl -fsSL https://raw.githubusercontent.com/PegasisForever/zenbook-duo-daemon/refs/heads/master/install.sh | sudo bash -s install

# Uninstall
curl -fsSL https://raw.githubusercontent.com/PegasisForever/zenbook-duo-daemon/refs/heads/master/install.sh | sudo bash -s uninstall

# Check logs
systemctl status zenbook-duo-daemon
```

The install script will:

1. Download the latest release from GitHub and install it to `/opt/zenbook-duo-daemon`.
2. Create a systemd service file in `/etc/systemd/system/zenbook-duo-daemon.service`
3. Create a backup of the old config file if it is not compatible with the new config file.
4. Enable and start the service

## Configuration

By default, the config file is located at `/etc/zenbook-duo-daemon/config.toml`. You can edit the fn lock, idle timeout, key mappings and keyboard VID:PID in the config file. The instructions are provided in the config file.

## Control Pipe

The daemon creates a named pipe for receiving commands at `/tmp/zenbook-duo-daemon.pipe` by default (configurable via `pipe_path` in the config file). The pipe is accessible by all users.

Send commands using echo example:

```bash
echo mic_mute_led_toggle > /tmp/zenbook-duo-daemon.pipe
```

Available commands:

| Command                    | Description                               |
| -------------------------- | ----------------------------------------- |
| `mic_mute_led_toggle`      | Toggle microphone mute LED                |
| `mic_mute_led_on`          | Turn on microphone mute LED               |
| `mic_mute_led_off`         | Turn off microphone mute LED              |
| `backlight_toggle`         | Cycle keyboard backlight                  |
| `backlight_off`            | Turn off keyboard backlight               |
| `backlight_low`            | Set keyboard backlight to low             |
| `backlight_medium`         | Set keyboard backlight to medium          |
| `backlight_high`           | Set keyboard backlight to high            |
| `secondary_display_toggle` | Toggle secondary display                  |
| `secondary_display_on`     | Turn on secondary display                 |
| `secondary_display_off`    | Turn off secondary display                |
| `suspend_start`            | Signal suspend start (disables backlight) |
| `suspend_end`              | Signal suspend end (restores backlight)   |

Notes:

1. The `suspend_start` and `suspend_end` commands are sent automatically by the systemd services `zenbook-duo-daemon-pre-sleep` and `zenbook-duo-daemon-post-sleep` to disable keyboard backlight during suspend.
2. The secondary display commands are no-op when the keyboard is attached.

## Development

### Prerequisites

```bash
sudo apt install build-essential libevdev-dev libdbus-1-dev pkg-config autoconf
```

### Build

This is a standard Rust project, you can run the project with:

```bash
# Stop the systemctl service to prevent two instances running
sudo systemctl stop zenbook-duo-daemon

cargo run
```

Or you can build and install the binary to your system with:

```bash
cargo build --release
sudo ./install.sh local-install target/release/zenbook-duo-daemon
```
