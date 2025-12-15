use std::{
    sync::{Arc, Mutex},
    thread,
};

use crate::{
    secondary_display::sync_secondary_display_brightness_thread,
    virtual_keyboard::VirtualKeyboard,
    wired_keyboard_thread::{find_wired_keyboard, wired_keyboard_thread},
};
use bt_keyboard_thread::bt_input_monitor_thread;
use futures_lite::stream;
use nusb::{hotplug::HotplugEvent, watch_devices};

mod bt_keyboard_thread;
mod secondary_display;
mod virtual_keyboard;
mod wired_keyboard_thread;

pub const VENDOR_ID: u16 = 0x0b05;
pub const PRODUCT_ID: u16 = 0x1bf2;

fn main() {
    env_logger::init();

    let virtual_keyboard = Arc::new(Mutex::new(VirtualKeyboard::new()));
    let keyboard_state = Arc::new(Mutex::new(KeyboardState::new()));

    thread::spawn(sync_secondary_display_brightness_thread);

    {
        let keyboard_state = keyboard_state.clone();
        let virtual_keyboard = virtual_keyboard.clone();
        thread::spawn(move || bt_input_monitor_thread(keyboard_state, virtual_keyboard));
    }

    if let Some(keyboard) = find_wired_keyboard() {
        wired_keyboard_thread(keyboard, keyboard_state.clone(), virtual_keyboard.clone());
    }

    for event in stream::block_on(watch_devices().unwrap()) {
        match event {
            HotplugEvent::Connected(d)
                if d.vendor_id() == VENDOR_ID && d.product_id() == PRODUCT_ID =>
            {
                if let Some(keyboard) = find_wired_keyboard() {
                    wired_keyboard_thread(
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
    mute_microphone_led: MuteMicrophoneState,
}

impl KeyboardState {
    pub fn new() -> Self {
        Self {
            backlight: BacklightState::Low,
            mute_microphone_led: MuteMicrophoneState::Unmuted,
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

#[derive(Clone, Copy)]
pub enum MuteMicrophoneState {
    Muted,
    Unmuted,
}

impl MuteMicrophoneState {
    pub fn next(&self) -> Self {
        match self {
            Self::Muted => Self::Unmuted,
            Self::Unmuted => Self::Muted,
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
