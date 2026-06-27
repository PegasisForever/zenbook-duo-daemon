//! Runtime configuration (`config.toml`) and [`KeyFunction`] handlers.
//!
//! # Zenbook Duo physical Fn row vs daemon HID paths
//!
//! Special keys are delivered two different ways:
//!
//! 1. **ASUS vendor channel** — USB HID reports on endpoint 5 (`0x5a` …), or Bluetooth
//!    `EV_ABS` / `ABS_MISC` on the sibling “ASUS Zenbook Duo Keyboard” evdev nodes. Parsed in
//!    `parse_keyboard_data` in `keyboard_usb.rs` and `parse_keyboard_event` in `keyboard_bt.rs`.
//!
//! 2. **Ordinary evdev** — the main keyboard device (e.g. `/dev/input/event3`). Fn+F1–F3 (mute /
//!    volume) and **Fn+F7** use this path. **Fn+F7** injects **Super+P** (`KEY_LEFTMETA` +
//!    `KEY_P`) so GNOME opens its display-mode UI (join / mirror / internal only / external).
//!    Those keys are **not** vendor-byte codes and are **not** handled by this module.
//!
//! | Physical key | Vendor value (USB byte 2 / BT ABS_MISC) | Default [`KeyFunction`] |
//! |--------------|----------------------------------------|-------------------------|
//! | Fn+F1 mute, Fn+F2/F3 volume | *(main keyboard only — not vendor channel)* | — |
//! | Fn+F4 keyboard backlight | `199` | [`KeyFunction::KeyboardBacklight`] |
//! | Fn+F5 display brightness down | `16` | `KEY_BRIGHTNESSDOWN` |
//! | Fn+F6 display brightness up | `32` | `KEY_BRIGHTNESSUP` |
//! | Fn+F7 display mode cycle | Super+P on main keyboard | *(GNOME Shell — not daemon)* |
//! | Fn+F8 swap primary internal panel | `156` | [`KeyFunction::SwapDisplays`] |
//! | Fn+F9 mic mute | `124` | `KEY_MICMUTE` |
//! | Fn+F10 Bluetooth pairing | *(firmware / BlueZ — intentionally unmapped)* | — |
//! | Fn+F11 emoji | `126` | Ctrl+. |
//! | Fn+F12 ASUS / Control Center | `134` | [`KeyFunction::NoOp`] |
//! | Key **right of F12** (bottom panel on/off) | `106` | [`KeyFunction::ToggleSecondaryDisplay`] |

use log::{info, warn};
use std::{path::PathBuf, sync::Arc};
use std::time::Duration;
use tokio::fs;
use tokio::sync::Mutex;

use evdev_rs::enums::EV_KEY;
use serde::{Deserialize, Serialize};

use crate::state::KeyboardStateManager;

// All the enum carries a value so the serialized toml looks better
#[derive(Serialize, Deserialize, Clone)]
pub enum KeyFunction {
    KeyboardBacklight(bool),
    ToggleSecondaryDisplay(bool),
    SwapDisplays(bool),
    KeyBind(Vec<EV_KEY>),
    Command(String),
    NoOp(bool),
}

impl KeyFunction {
    /// Execute a key function: `KeyBind`, `Command`, `KeyboardBacklight`, `ToggleSecondaryDisplay`,
    /// [`SwapDisplays`](KeyFunction::SwapDisplays), and `NoOp`.
    pub async fn execute(
        &self,
        virtual_keyboard: &Arc<Mutex<crate::virtual_keyboard::VirtualKeyboard>>,
        state_manager: &KeyboardStateManager,
    ) {
        match self {
            KeyFunction::KeyBind(items) => {
                if items.as_slice() == [EV_KEY::KEY_MICMUTE] {
                    match crate::mute_state::toggle_default_source_mute() {
                        Ok(muted) => state_manager.set_mic_mute_led(muted),
                        Err(e) => warn!("KeyFunction: toggle microphone mute failed: {e}"),
                    }
                    return;
                }
                virtual_keyboard
                    .lock()
                    .await
                    .release_prev_and_press_keys(items);
            }
            KeyFunction::Command(command) => {
                crate::execute_command(command);
            }
            KeyFunction::KeyboardBacklight(true) => {
                state_manager.toggle_keyboard_backlight().await;
            }
            KeyFunction::ToggleSecondaryDisplay(true) => {
                info!("KeyFunction: executing ToggleSecondaryDisplay");
                crate::secondary_coordinator::coordinate_secondary_display_toggle(state_manager).await;
            }
            KeyFunction::SwapDisplays(true) => {
                info!("KeyFunction: executing SwapDisplays");
                crate::secondary_display::pause_brightness_sync_for(Duration::from_secs(4));
                
                // Read current desired state to determine what we want
                let current_desired = state_manager.get_desired_primary();
                let new_desired = if current_desired.as_deref() == Some("eDP-2") {
                    "eDP-1"
                } else {
                    "eDP-2"
                };
                
                // Save the intended primary BEFORE attempting swap (independent of success)
                state_manager.set_desired_primary(new_desired).await;
                info!("User intention: swap to {}", new_desired);

                // Root + persisted state are authoritative: nothing is "dropped" if no session is
                // registered yet. `run_session_client` reads `desired_primary` from the root D-Bus
                // property when the session daemon connects and follows property updates thereafter.
                // `notify_desired_primary_changed` returns Ok(false) when no session has registered
                // — we only emit the signal; Mutter apply happens once GNOME session is up.
                match crate::dbus_state::notify_desired_primary_changed().await {
                    Ok(true) => {
                        info!("Published desired_primary={new_desired} over D-Bus (session registered)");
                    }
                    Ok(false) => {
                        warn!(
                            "SwapDisplays: persisted desired_primary={new_desired}; no session daemon registered yet — will apply when a GNOME session connects and registers"
                        );
                    }
                    Err(e) => {
                        warn!("Failed to publish desired_primary update over D-Bus: {e}");
                    }
                }
            }
            _ => {
                // do nothing
            }
        }
    }
}

/// How to map ASUS Zenbook Duo integrated pen (`04f3:4447` / `04f3:4448`) to internal panels in GNOME.
///
/// GNOME stores this under `org.gnome.desktop.peripherals.tablet` relocatable schemas
/// (`…/tablets/<vid>:<pid>/` `output` key = EDID triple for the chosen logical monitor).
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
#[serde(rename_all = "snake_case")]
pub enum TabletMapMode {
    /// `04f3:4447` → **eDP-1**, `04f3:4448` → **eDP-2** (physical pairing on the UX8406 stack).
    #[default]
    OneToOne,
    /// Both pen devices follow whichever internal connector is currently **primary** (`desired_primary`).
    AllToPrimary,
}

/// Maps xhci hub port indices (`lsusb -t` port number) to pogo dock.
#[derive(Serialize, Deserialize, Clone, Debug)]
pub struct UsbKeyboardPortsConfig {
    /// Hub port numbers when the keyboard is on the bottom panel (pogo). UX8406CA: `6`.
    #[serde(default = "default_pogo_dock_hub_ports", alias = "pogo_dock_devpaths")]
    pub pogo_dock_hub_ports: Vec<String>,
}

fn default_pogo_dock_hub_ports() -> Vec<String> {
    vec!["6".to_string()]
}

impl Default for UsbKeyboardPortsConfig {
    fn default() -> Self {
        Self {
            pogo_dock_hub_ports: default_pogo_dock_hub_ports(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug)]
pub struct TabletMappingConfig {
    /// When `true`, the session daemon reapplies GNOME tablet `output` mappings after each
    /// successful display reconcile (and on the same cadence as layout stabilisation).
    #[serde(default)]
    pub enable: bool,
    #[serde(default)]
    pub mode: TabletMapMode,
}

impl Default for TabletMappingConfig {
    fn default() -> Self {
        Self {
            enable: true,
            mode: TabletMapMode::default(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq, Eq)]
pub struct BatteryUiConfig {
    #[serde(default = "default_discharge_warning_pct")]
    pub discharge_warning_pct: u8,
    #[serde(default = "default_discharge_severe_pct")]
    pub discharge_severe_pct: u8,
    #[serde(default = "default_discharge_critical_pct")]
    pub discharge_critical_pct: u8,
    #[serde(default = "default_charge_half_pct")]
    pub charge_half_pct: u8,
    #[serde(default = "default_charge_high_pct")]
    pub charge_high_pct: u8,
    #[serde(default = "default_charge_full_pct")]
    pub charge_full_pct: u8,
    #[serde(default = "default_discharge_warning_color")]
    pub discharge_warning_color: String,
    #[serde(default = "default_discharge_severe_color")]
    pub discharge_severe_color: String,
    #[serde(default = "default_discharge_critical_color")]
    pub discharge_critical_color: String,
    #[serde(default = "default_charge_half_color")]
    pub charge_half_color: String,
    #[serde(default = "default_charge_high_color")]
    pub charge_high_color: String,
    #[serde(default = "default_charge_full_color")]
    pub charge_full_color: String,
}

fn default_discharge_warning_pct() -> u8 {
    25
}

fn default_discharge_severe_pct() -> u8 {
    10
}

fn default_discharge_critical_pct() -> u8 {
    5
}

fn default_charge_half_pct() -> u8 {
    50
}

fn default_charge_high_pct() -> u8 {
    75
}

fn default_charge_full_pct() -> u8 {
    100
}

fn default_discharge_warning_color() -> String {
    "#f9f06b".to_string()
}

fn default_discharge_severe_color() -> String {
    "#ffbe6f".to_string()
}

fn default_discharge_critical_color() -> String {
    "#f66151".to_string()
}

fn default_charge_half_color() -> String {
    "#ffa348".to_string()
}

fn default_charge_high_color() -> String {
    "#f9f06b".to_string()
}

fn default_charge_full_color() -> String {
    "#8ff0a4".to_string()
}

fn default_ambient_keyboard_backlight_enabled() -> bool {
    false
}

impl Default for BatteryUiConfig {
    fn default() -> Self {
        Self {
            discharge_warning_pct: default_discharge_warning_pct(),
            discharge_severe_pct: default_discharge_severe_pct(),
            discharge_critical_pct: default_discharge_critical_pct(),
            charge_half_pct: default_charge_half_pct(),
            charge_high_pct: default_charge_high_pct(),
            charge_full_pct: default_charge_full_pct(),
            discharge_warning_color: default_discharge_warning_color(),
            discharge_severe_color: default_discharge_severe_color(),
            discharge_critical_color: default_discharge_critical_color(),
            charge_half_color: default_charge_half_color(),
            charge_high_color: default_charge_high_color(),
            charge_full_color: default_charge_full_color(),
        }
    }
}

fn is_valid_hex_color(color: &str) -> bool {
    color.len() == 7
        && color.starts_with('#')
        && color.chars().skip(1).all(|c| c.is_ascii_hexdigit())
}

impl BatteryUiConfig {
    pub fn validate(&self) -> Result<(), String> {
        if !(self.discharge_critical_pct < self.discharge_severe_pct
            && self.discharge_severe_pct < self.discharge_warning_pct
            && self.discharge_warning_pct <= 100)
        {
            return Err(
                "discharge thresholds must satisfy critical < severe < warning <= 100".to_string(),
            );
        }
        if !(self.charge_half_pct < self.charge_high_pct
            && self.charge_high_pct < self.charge_full_pct
            && self.charge_full_pct <= 100)
        {
            return Err("charge thresholds must satisfy half < high < full <= 100".to_string());
        }

        for (name, color) in [
            ("discharge_warning_color", &self.discharge_warning_color),
            ("discharge_severe_color", &self.discharge_severe_color),
            ("discharge_critical_color", &self.discharge_critical_color),
            ("charge_half_color", &self.charge_half_color),
            ("charge_high_color", &self.charge_high_color),
            ("charge_full_color", &self.charge_full_color),
        ] {
            if !is_valid_hex_color(color) {
                return Err(format!("{name} must be a #RRGGBB color, got {color}"));
            }
        }
        Ok(())
    }
}

pub fn tablet_mode_to_str(mode: TabletMapMode) -> &'static str {
    match mode {
        TabletMapMode::OneToOne => "one_to_one",
        TabletMapMode::AllToPrimary => "all_to_primary",
    }
}

pub fn tablet_mode_from_str(mode: &str) -> Result<TabletMapMode, String> {
    match mode {
        "one_to_one" => Ok(TabletMapMode::OneToOne),
        "all_to_primary" => Ok(TabletMapMode::AllToPrimary),
        other => Err(format!(
            "tablet mapping mode must be one_to_one or all_to_primary, got {other}"
        )),
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    usb_vendor_id: String,
    usb_product_id: String,
    #[serde(default)]
    pub usb_keyboard_ports: UsbKeyboardPortsConfig,
    #[serde(default)]
    pub tablet: TabletMappingConfig,
    pub fn_lock: bool,
    pub keyboard_backlight_key: KeyFunction,
    pub brightness_down_key: KeyFunction,
    pub brightness_up_key: KeyFunction,
    pub swap_up_down_display_key: KeyFunction,
    pub microphone_mute_key: KeyFunction,
    pub emoji_picker_key: KeyFunction,
    pub myasus_key: KeyFunction,
    pub toggle_secondary_display_key: KeyFunction,
    pub secondary_display_status_path: String,
    pub primary_backlight_path: String,
    pub secondary_backlight_path: String,
    /// Idle timeout in seconds. Set to 0 to disable idle detection.
    pub idle_timeout_seconds: u64,
    #[serde(default = "default_ambient_keyboard_backlight_enabled")]
    pub ambient_keyboard_backlight_enabled: bool,
    #[serde(default)]
    pub battery_ui: BatteryUiConfig,
}

impl Config {
    pub fn vendor_id(&self) -> u16 {
        u16::from_str_radix(&self.usb_vendor_id, 16).unwrap()
    }

    pub fn product_id(&self) -> u16 {
        u16::from_str_radix(&self.usb_product_id, 16).unwrap()
    }
}

fn get_usb_product_id() -> String {
    let board_name = std::fs::read_to_string("/sys/class/dmi/id/board_name")
        .unwrap_or_default()
        .trim()
        .to_string();
    if board_name == "UX8406CA" {
        info!("Detected Zenbook Duo 2025");
        "1bf2".to_string()
    } else if board_name == "UX8406MA" {
        info!("Detected Zenbook Duo 2024");
        "1b2c".to_string()
    } else {
        warn!(
            "Unknown board name: {}, using default product id 1b2c",
            board_name
        );
        "1b2c".to_string()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            usb_vendor_id: "0b05".to_string(),
            usb_product_id: get_usb_product_id(),
            usb_keyboard_ports: UsbKeyboardPortsConfig::default(),
            tablet: TabletMappingConfig::default(),
            fn_lock: true,
            keyboard_backlight_key: KeyFunction::KeyboardBacklight(true),
            brightness_down_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_BRIGHTNESSDOWN]),
            brightness_up_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_BRIGHTNESSUP]),
            swap_up_down_display_key: KeyFunction::SwapDisplays(true),
            microphone_mute_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_MICMUTE]),
            emoji_picker_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_LEFTCTRL, EV_KEY::KEY_DOT]),
            myasus_key: KeyFunction::NoOp(true),
            toggle_secondary_display_key: KeyFunction::ToggleSecondaryDisplay(true),
            secondary_display_status_path: "/sys/class/drm/card1-eDP-2/status".to_string(),
            primary_backlight_path: "/sys/class/backlight/intel_backlight/brightness".to_string(),
            secondary_backlight_path: "/sys/class/backlight/card1-eDP-2-backlight/brightness"
                .to_string(),
            idle_timeout_seconds: 300, // 5 minutes
            ambient_keyboard_backlight_enabled: default_ambient_keyboard_backlight_enabled(),
            battery_ui: BatteryUiConfig::default(),
        }
    }
}

pub const DEFAULT_CONFIG_PATH: &str = "/etc/zenbook-duo-daemon/config.toml";

impl Config {
    fn help_text() -> &'static str {
        "
# # Example Configuration:
#
# # Zenbook Duo keys (physical → config keys; see `src/config.rs` module docs for vendor bytes):
# #   Fn+F4  → keyboard_backlight_key       Fn+F5/F6 → brightness down/up
# #   Fn+F8  → swap_up_down_display_key     Fn+F9 → microphone_mute_key
# #   Fn+F11 → emoji_picker_key             Fn+F12 → myasus_key
# #   Key right of F12 → toggle_secondary_display_key
# #   Fn+F1–F3, Fn+F7 are NOT on the ASUS vendor HID path: F7 sends Super+P on the main keyboard.
#
# [keyboard_backlight_key]                  # This specifies the physical key to configure
# # Only one of the following values is allowed:
# KeyBind = [\"KEY_LEFTCTRL\", \"KEY_F10\"]     # Maps the physical key to left ctrl + f10, a list of all the keys can be found in https://docs.rs/evdev-rs/0.6.3/evdev_rs/enums/enum.EV_KEY.html
# Command = \"echo 'Hello, world!'\"          # Runs a custom command as root when the physical key is pressed
# KeyboardBacklight = true                  # Toggles the keyboard backlight
# # Vendor-style toggles (`KeyboardBacklight`, `SwapDisplays`, `ToggleSecondaryDisplay`, `NoOp`):
# #   use `= true` or `= false` in TOML (the bool is only so the table shape serializes cleanly).
# SwapDisplays = true                       # Swap which internal panel is primary (eDP-1 <-> eDP-2); see README “Dual display / Mutter apply ordering”
# ToggleSecondaryDisplay = true             # Toggles the secondary display
# NoOp = true                               # Does nothing when the physical key is pressed
#
# fn_lock = true             # To input F1-F12, you need to press Fn + F1-F12
# idle_timeout_seconds = 300 # 5 minutes, set to 0 to disable idle detection
# ambient_keyboard_backlight_enabled = false # When true, very low ambient light raises keyboard backlight to at least Low while active
#
# [usb_keyboard_ports]                   # Hub port = lsusb -t port (see `usb_keyboard_ports.rs`)
# pogo_dock_hub_ports = [\"6\"]          # UX8406CA: 6 = bottom pogo, 4 = side charge (not pogo)
#
# [tablet]                               # Optional: GNOME Wayland pen → panel mapping (see README)
# enable = true                          # When true, reapplies after each successful display reconcile
# mode = \"one_to_one\"                  # or \"all_to_primary\" — see `TabletMapMode` in `src/config.rs`
#
# [battery_ui]                           # Extension + notification thresholds/colors, persisted in the daemon
# discharge_warning_pct = 25
# discharge_severe_pct = 10
# discharge_critical_pct = 5
# charge_half_pct = 50
# charge_high_pct = 75
# charge_full_pct = 100
# discharge_warning_color = \"#f9f06b\"
# discharge_severe_color = \"#ffbe6f\"
# discharge_critical_color = \"#f66151\"
# charge_half_color = \"#ffa348\"
# charge_high_color = \"#f9f06b\"
# charge_full_color = \"#8ff0a4\"
        "
        .trim()
    }

    fn to_file_text(&self) -> Result<String, String> {
        self.battery_ui.validate()?;
        let config_str = toml::to_string(self)
            .map_err(|e| format!("Failed to serialize config: {e}"))?;
        Ok(format!("{}\n\n\n{}", Self::help_text(), config_str))
    }

    pub async fn write(&self, config_path: &PathBuf) -> Result<(), String> {
        let config_str = self.to_file_text()?;
        let parent = config_path.parent().ok_or_else(|| {
            format!("Config path {} has no parent directory", config_path.display())
        })?;
        if !fs::try_exists(parent)
            .await
            .map_err(|e| format!("Failed to check config dir {}: {e}", parent.display()))?
        {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| format!("Failed to create config dir {}: {e}", parent.display()))?;
        }
        fs::write(config_path, config_str)
            .await
            .map_err(|e| format!("Failed to write config file {}: {e}", config_path.display()))
    }

    pub async fn write_default_config(config_path: &PathBuf) {
        let config = Config::default();
        config.write(config_path).await.unwrap();
    }

    /// Try to read config file, returns error if read or parse fails
    pub async fn try_read(config_path: &PathBuf) -> Result<Config, String> {
        let config_str = fs::read_to_string(config_path)
            .await
            .map_err(|e| format!("Failed to read config file: {}", e))?;
        let config: Config = toml::from_str(&config_str)
            .map_err(|e| format!("Failed to parse config file: {}", e))?;
        config.battery_ui.validate()?;
        Ok(config)
    }

    /// Read config file, creating default if it doesn't exist
    pub async fn read(config_path: &PathBuf) -> Config {
        if !fs::try_exists(config_path).await.unwrap_or(false) {
            Self::write_default_config(config_path).await;
        }
        let config_str = fs::read_to_string(config_path).await.unwrap();
        let config: Config = toml::from_str(&config_str).unwrap();
        config.battery_ui.validate().unwrap();
        config
    }
}
