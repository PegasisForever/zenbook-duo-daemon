use evdev_rs::{
    DeviceWrapper, InputEvent, UInputDevice, UninitDevice,
    enums::{BusType, EV_KEY, EV_SYN, EventCode},
};
use std::time::SystemTime;

use crate::{PRODUCT_ID, VENDOR_ID};

pub enum KeyEventType {
    Release,
    Press,
    Repeat,
}

impl KeyEventType {
    pub fn value(&self) -> i32 {
        match self {
            Self::Release => 0,
            Self::Press => 1,
            Self::Repeat => 2,
        }
    }
}

pub struct VirtualKeyboard {
    device: UInputDevice,
    pressed_keys: Vec<EV_KEY>,
}

impl VirtualKeyboard {
    pub fn new() -> Self {
        let u = UninitDevice::new().unwrap();

        u.set_name("Zenbook Duo Keyboard Daemon");
        u.set_bustype(BusType::BUS_VIRTUAL as u16);
        u.set_vendor_id(VENDOR_ID);
        u.set_product_id(PRODUCT_ID);

        u.enable(EventCode::EV_KEY(EV_KEY::KEY_BRIGHTNESSDOWN))
            .unwrap();
        u.enable(EventCode::EV_KEY(EV_KEY::KEY_BRIGHTNESSUP))
            .unwrap();
        u.enable(EventCode::EV_KEY(EV_KEY::KEY_MICMUTE)).unwrap();
        u.enable(EventCode::EV_KEY(EV_KEY::KEY_EMOJI_PICKER))
            .unwrap();
        u.enable(EventCode::EV_KEY(EV_KEY::KEY_LEFTCTRL))
            .unwrap();
        u.enable(EventCode::EV_KEY(EV_KEY::KEY_LEFTSHIFT))
            .unwrap();
        u.enable(EventCode::EV_KEY(EV_KEY::KEY_B))
            .unwrap();

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
