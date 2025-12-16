# ASUS Zenbook Duo Daemon

This is a daemon that runs on the Zenbook Duo laptop to handle the keyboard and secondary display under linux.

## Device Support

- ✅ Zenbook Duo 2025
- ⚠️ Zenbook Duo 2024 (Not tested, likely needs some modifications)

## Distribution Support

- ✅ Ubuntu 25.10 6.17.0-8-generic
- ⚠️ Other distributions may work, but are not tested

## Features

- ✅ Enable secondary display when keyboard is detached
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
| Microphone Mute Key LED Control | ⚠️ (3)     | ❌ (2)         | N/A                          | ✅                          |
| Emoji Picker Key                | ✅         | ✅             | `KEY_LEFTCTRL + KEY_DOT` (4) | ✅                          |
| MyASUS Key                      | ✅         | ✅             | None                         | ✅                          |
| Toggle Secondary Display Key    | ✅         | ✅             | Toggle Secondary Display     | ❌                          |
| Fn + Function Keys              | ✅         | ✅             | F1 - F12                     | ✅                          |

1. Should be possible, the packet capture file under windows is at `pcap/bt_change_backlight.pcapng`
2. Should be possible, the packet capture file under windows is at `pcap/bt_micmute_led.pcapng`
3. Possible in code using the `send_mute_microphone_state` function, however determining the microphone mute state of the system is complicated.
4. This key combination only works for GTK apps in GNOME.

## Known Issues

- Keyboard backlight stays on after laptop suspended
- Secondary display turns on after laptop resumes from suspension

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
3. Enable and start the service

## Configuration

By default, the config file is located at `/etc/zenbook-duo-daemon/config.toml`. You can edit the key mappings and keyboard VID:PID in the config file. The instructions are provided in the config file.

## Development

### Prerequisites

```bash
sudo apt install libevdev-dev libdbus-1-dev pkg-config autoconf
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
