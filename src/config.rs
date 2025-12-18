use std::{fs, path::PathBuf, sync::{Arc, Mutex}};

use evdev_rs::enums::EV_KEY;
use log::debug;
use serde::{Deserialize, Serialize};

// All the enum carries a value so the serialized toml looks better
#[derive(Serialize, Deserialize, Clone)]
pub enum KeyFunction {
    KeyboardBacklight(bool),
    ToggleSecondaryDisplay(bool),
    KeyBind(Vec<EV_KEY>),
    Command(String),
    NoOp(bool),
}

impl KeyFunction {
    /// Execute a key function - handles KeyBind, Command, KeyboardBacklight, and ToggleSecondaryDisplay
    pub fn execute(
        &self,
        virtual_keyboard: &Arc<Mutex<crate::virtual_keyboard::VirtualKeyboard>>,
        event_sender: &std::sync::mpmc::Sender<crate::events::Event>,
    ) {
        match self {
            KeyFunction::KeyBind(items) => {
                virtual_keyboard
                    .lock()
                    .unwrap()
                    .release_prev_and_press_keys(items);
            }
            KeyFunction::Command(command) => {
                crate::execute_command(command);
            }
            KeyFunction::KeyboardBacklight(true) => {
                event_sender.send(crate::events::Event::BacklightToggle).ok();
            }
            KeyFunction::ToggleSecondaryDisplay(true) => {
                event_sender.send(crate::events::Event::SecondaryDisplayToggle).ok();
            }
            _ => {
                // do nothing
            }
        }
    }
}

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    usb_vendor_id: String,
    usb_product_id: String,
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
    pub pipe_path: String,
}

impl Config {
    pub fn vendor_id(&self) -> u16 {
        u16::from_str_radix(&self.usb_vendor_id, 16).unwrap()
    }

    pub fn product_id(&self) -> u16 {
        u16::from_str_radix(&self.usb_product_id, 16).unwrap()
    }
}

impl Default for Config {
    fn default() -> Self {
        Self {
            usb_vendor_id: "0b05".to_string(),
            usb_product_id: "1bf2".to_string(),
            keyboard_backlight_key: KeyFunction::KeyboardBacklight(true),
            brightness_down_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_BRIGHTNESSDOWN]),
            brightness_up_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_BRIGHTNESSUP]),
            swap_up_down_display_key: KeyFunction::NoOp(true),
            microphone_mute_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_MICMUTE]),
            emoji_picker_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_LEFTCTRL, EV_KEY::KEY_DOT]),
            myasus_key: KeyFunction::NoOp(true),
            toggle_secondary_display_key: KeyFunction::ToggleSecondaryDisplay(true),
            secondary_display_status_path: "/sys/class/drm/card1-eDP-2/status".to_string(),
            primary_backlight_path: "/sys/class/backlight/intel_backlight/brightness".to_string(),
            secondary_backlight_path: "/sys/class/backlight/card1-eDP-2-backlight/brightness".to_string(),
            pipe_path: "/tmp/zenbook-duo-daemon.pipe".to_string(),
        }
    }
}

pub const DEFAULT_CONFIG_PATH: &str = "/etc/zenbook-duo-daemon/config.toml";

impl Config {
    pub fn write_default_config(config_path: &PathBuf) {
        let config = Config::default();
        let config_str = toml::to_string(&config).unwrap();
        let help = "
# # Example Configuration:
#
# [keyboard_backlight_key]                  # This specifies the physical key to configure
# # Only one of the following values is allowed:
# KeyBind = [\"KEY_LEFTCTRL\", \"KEY_F10\"]     # Maps the physical key to left ctrl + f10, a list of all the keys can be found in https://docs.rs/evdev-rs/0.6.3/evdev_rs/enums/enum.EV_KEY.html
# Command = \"echo 'Hello, world!'\"          # Runs a custom command as root when the physical key is pressed
# KeyboardBacklight = true                  # Toggles the keyboard backlight
# ToggleSecondaryDisplay = true             # Toggles the secondary display
# NoOp = true                               # Does nothing when the physical key is pressed
        ".trim();
        let config_str = format!("{}\n\n\n{}", help, config_str);

        let parent = config_path.parent().unwrap();
        if !parent.exists() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(config_path, config_str).unwrap();
    }

    /// Try to read config file, returns error if read or parse fails
    pub fn try_read(config_path: &PathBuf) -> Result<Config, String> {
        let config_str = fs::read_to_string(config_path)
            .map_err(|e| format!("Failed to read config file: {}", e))?;
        toml::from_str(&config_str)
            .map_err(|e| format!("Failed to parse config file: {}", e))
    }
    
    /// Read config file, creating default if it doesn't exist
    pub fn read(config_path: &PathBuf) -> Config {
        if !fs::metadata(config_path).is_ok() {
            Self::write_default_config(config_path);
        }
        let config_str = fs::read_to_string(config_path).unwrap();
        toml::from_str(&config_str).unwrap()
    }
}
