use crate::{
    virtual_keyboard::VirtualKeyboard,
    wired_keyboard_thread::{find_wired_keyboard, wired_keyboard_thread},
};
use futures_lite::stream;
use nusb::{hotplug::HotplugEvent, watch_devices};

mod virtual_keyboard;
mod wired_keyboard_thread;

pub const VENDOR_ID: u16 = 0x0b05;
pub const PRODUCT_ID: u16 = 0x1bf2;

fn main() {
    env_logger::init();

    let mut virtual_keyboard = VirtualKeyboard::new();
    let mut keyboard_state = KeyboardState::new();

    if let Some(keyboard) = find_wired_keyboard() {
        wired_keyboard_thread(keyboard, &mut keyboard_state, &mut virtual_keyboard);
    }

    for event in stream::block_on(watch_devices().unwrap()) {
        match event {
            HotplugEvent::Connected(d)
                if d.vendor_id() == VENDOR_ID && d.product_id() == PRODUCT_ID =>
            {
                if let Some(keyboard) = find_wired_keyboard() {
                    wired_keyboard_thread(keyboard, &mut keyboard_state, &mut virtual_keyboard);
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
