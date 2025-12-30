use evdev_rs::{
    DeviceWrapper, InputEvent, UInputDevice, UninitDevice,
    enums::{BusType, EV_KEY, EV_SYN, EventCode},
};
use std::time::SystemTime;

use crate::config::{Config, KeyFunction};

pub enum KeyEventType {
    Release,
    Press,
}

impl KeyEventType {
    pub fn value(&self) -> i32 {
        match self {
            Self::Release => 0,
            Self::Press => 1,
        }
    }
}

pub struct VirtualKeyboard {
    device: UInputDevice,
    pressed_keys: Vec<EV_KEY>,
}

impl VirtualKeyboard {
    pub fn new(config: &Config) -> Self {
        let u = UninitDevice::new().unwrap();

        u.set_name("Zenbook Duo Daemon");
        u.set_bustype(BusType::BUS_VIRTUAL as u16);
        u.set_vendor_id(config.vendor_id());
        u.set_product_id(config.product_id());

        let enable_key = |key_function: &KeyFunction| {
            if let KeyFunction::KeyBind(keys) = key_function {
                for key in keys {
                    u.enable(EventCode::EV_KEY(*key)).unwrap();
                }
            }
        };
        enable_key(&config.keyboard_backlight_key);
        enable_key(&config.brightness_down_key);
        enable_key(&config.brightness_up_key);
        enable_key(&config.swap_up_down_display_key);
        enable_key(&config.microphone_mute_key);
        enable_key(&config.emoji_picker_key);
        enable_key(&config.myasus_key);
        enable_key(&config.toggle_secondary_display_key);

        Self {
            device: UInputDevice::create_from_device(&u).unwrap(),
            pressed_keys: Vec::new(),
        }
    }

    pub fn release_prev_and_press_keys(&mut self, keys: &[EV_KEY]) {
        self.release_all_keys();

        let time = SystemTime::now().try_into().unwrap();
        for key in keys {
            let event =
                InputEvent::new(&time, &EventCode::EV_KEY(*key), KeyEventType::Press.value());
            self.device.write_event(&event).unwrap();
        }

        let sync_event = InputEvent::new(&time, &EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0);
        self.device.write_event(&sync_event).unwrap();

        self.pressed_keys.extend(keys);
    }

    pub fn release_all_keys(&mut self) {
        if !self.pressed_keys.is_empty() {
            let time = SystemTime::now().try_into().unwrap();
            for key in &self.pressed_keys {
                let event = InputEvent::new(
                    &time,
                    &EventCode::EV_KEY(*key),
                    KeyEventType::Release.value(),
                );
                self.device.write_event(&event).unwrap();
            }

            let sync_event = InputEvent::new(&time, &EventCode::EV_SYN(EV_SYN::SYN_REPORT), 0);
            self.device.write_event(&sync_event).unwrap();

            self.pressed_keys.clear();
        }
    }
}
