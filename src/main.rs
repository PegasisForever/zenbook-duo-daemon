use std::{
    process, sync::{Arc, Mutex}, thread
};

use crate::{
    config::Config,
    secondary_display::sync_secondary_display_brightness_thread,
    virtual_keyboard::VirtualKeyboard,
    wired_keyboard_thread::{find_wired_keyboard, wired_keyboard_thread},
};
use bt_keyboard_thread::bt_input_monitor_thread;
use futures_lite::stream;
use log::{info, warn};
use nusb::{hotplug::HotplugEvent, watch_devices};

mod bt_keyboard_thread;
mod config;
mod secondary_display;
mod virtual_keyboard;
mod wired_keyboard_thread;

pub const VENDOR_ID: u16 = 0x0b05;
pub const PRODUCT_ID: u16 = 0x1bf2;

fn main() {
    env_logger::init();

    let config = Config::read();

    let virtual_keyboard = Arc::new(Mutex::new(VirtualKeyboard::new(&config)));
    let keyboard_state = Arc::new(Mutex::new(KeyboardState::new()));

    thread::spawn(sync_secondary_display_brightness_thread);

    {
        let keyboard_state = keyboard_state.clone();
        let virtual_keyboard = virtual_keyboard.clone();
        let config = config.clone();
        thread::spawn(move || bt_input_monitor_thread(&config, keyboard_state, virtual_keyboard));
    }

    if let Some(keyboard) = find_wired_keyboard() {
        wired_keyboard_thread(
            &config,
            keyboard,
            keyboard_state.clone(),
            virtual_keyboard.clone(),
        );
    }

    for event in stream::block_on(watch_devices().unwrap()) {
        match event {
            HotplugEvent::Connected(d)
                if d.vendor_id() == VENDOR_ID && d.product_id() == PRODUCT_ID =>
            {
                if let Some(keyboard) = find_wired_keyboard() {
                    wired_keyboard_thread(
                        &config,
                        keyboard,
                        keyboard_state.clone(),
                        virtual_keyboard.clone(),
                    );
                }
            }
            _ => {}
        }
    }
}

#[derive(Clone, Copy)]
pub struct KeyboardState {
    backlight: BacklightState,
}

impl KeyboardState {
    pub fn new() -> Self {
        Self {
            backlight: BacklightState::Low,
        }
    }
}

#[derive(Clone, Copy)]
pub enum BacklightState {
    Off,
    Low,
    Medium,
    High,
}

impl BacklightState {
    pub fn next(&self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Off,
        }
    }
}

pub fn parse_hex_string(hex_string: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in (0..hex_string.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&hex_string[i..i + 2], 16).unwrap());
    }
    bytes
}

pub fn execute_command(command: &str) {
    info!("Executing command: {}", command);
    let command = command.to_owned();
    thread::spawn(move || {
        match process::Command::new("sh").arg("-c").arg(&command).output() {
            Ok(output) => {
                info!(
                    "Command '{}' exited with status {}.\nstdout:\n{}\nstderr:\n{}",
                    command,
                    output.status,
                    String::from_utf8_lossy(&output.stdout).trim(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(e) => {
                warn!("Failed to execute command '{}': {}", command, e);
            }
        }
    });
}