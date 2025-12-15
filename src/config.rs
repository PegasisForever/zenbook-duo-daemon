use std::fs;

use evdev_rs::enums::EV_KEY;
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

#[derive(Serialize, Deserialize, Clone)]
pub struct Config {
    pub keyboard_backlight_key: KeyFunction,
    pub brightness_down_key: KeyFunction,
    pub brightness_up_key: KeyFunction,
    pub swap_up_down_display_key: KeyFunction,
    pub microphone_mute_key: KeyFunction,
    pub emoji_picker_key: KeyFunction,
    pub myasus_key: KeyFunction,
    pub toggle_secondary_display_key: KeyFunction,
}

impl Default for Config {
    fn default() -> Self {  
        Self {
            keyboard_backlight_key: KeyFunction::KeyboardBacklight(true),
            brightness_down_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_BRIGHTNESSDOWN]),
            brightness_up_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_BRIGHTNESSUP]),
            swap_up_down_display_key: KeyFunction::NoOp(true),
            microphone_mute_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_MICMUTE]),
            emoji_picker_key: KeyFunction::KeyBind(vec![EV_KEY::KEY_LEFTCTRL, EV_KEY::KEY_DOT]),
            myasus_key: KeyFunction::NoOp(true),
            toggle_secondary_display_key: KeyFunction::ToggleSecondaryDisplay(true),
        }
    }
}

const CONFIG_PATH: &str = "/etc/zenbook-duo-daemon/config.toml";

impl Config {
    fn write_default_config() {
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

        let parent = std::path::Path::new(CONFIG_PATH).parent().unwrap();
        if !parent.exists() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(CONFIG_PATH, config_str).unwrap();
    }

    pub fn read() -> Config {
        if !fs::metadata(CONFIG_PATH).is_ok() {
            Self::write_default_config();
        }
        let config_str = fs::read_to_string(CONFIG_PATH).unwrap();
        toml::from_str(&config_str).unwrap()
    }
}