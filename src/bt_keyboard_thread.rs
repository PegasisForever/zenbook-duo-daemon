use std::{
    io::ErrorKind,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use evdev_rs::{
    Device, DeviceWrapper as _, ReadFlag,
    enums::{EV_ABS, EV_KEY, EventCode},
};
use inotify::{Inotify, WatchMask};

use crate::{KeyboardState, toggle_secondary_display, virtual_keyboard::VirtualKeyboard};

pub fn bt_input_monitor_thread(
    keyboard_state: Arc<Mutex<KeyboardState>>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
) {
    for entry in std::fs::read_dir("/dev/input").unwrap() {
        let entry = entry.unwrap();

        let path = entry.path();
        try_start_bt_keyboard_thread(path, keyboard_state.clone(), virtual_keyboard.clone());
    }

    let mut inotify = Inotify::init().unwrap();
    inotify
        .watches()
        .add("/dev/input/", WatchMask::CREATE)
        .unwrap();

    let mut buffer = [0; 1024];

    loop {
        let events = inotify.read_events_blocking(&mut buffer).unwrap();

        for event in events {
            if let Some(name) = event.name {
                if event.mask.contains(inotify::EventMask::CREATE) {
                    if name.to_str().unwrap_or("").starts_with("event") {
                        let path = PathBuf::from("/dev/input/").join(name);
                        try_start_bt_keyboard_thread(
                            path,
                            keyboard_state.clone(),
                            virtual_keyboard.clone(),
                        );
                    }
                }
            }
        }
    }
}

fn try_start_bt_keyboard_thread(
    path: PathBuf,
    keyboard_state: Arc<Mutex<KeyboardState>>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
) {
    if path.is_dir() {
        return;
    }
    if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
        if !fname.starts_with("event") {
            return;
        }
    } else {
        return;
    }

    if let Ok(input) = evdev_rs::Device::new_from_path(&path) {
        // This name only matches when the keyboard is connected via Bluetooth, which is desired.
        if input.name() == Some("ASUS Zenbook Duo Keyboard") {
            let keyboard_state = keyboard_state.clone();
            let virtual_keyboard = virtual_keyboard.clone();
            thread::spawn(move || {
                bt_keyboard_thread(path, input, keyboard_state, virtual_keyboard);
            });
        }
    }
}

pub fn bt_keyboard_thread(
    path: PathBuf,
    keyboard: Device,
    keyboard_state: Arc<Mutex<KeyboardState>>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
) {
    println!("Starting BT keyboard thread for {}", path.display());

    loop {
        let event = keyboard.next_event(ReadFlag::NORMAL | ReadFlag::BLOCKING);

        match event {
            Ok((_status, event)) => {
                if event.event_code == EventCode::EV_ABS(EV_ABS::ABS_MISC) {
                    if event.value == 0 {
                        // no key pressed
                        virtual_keyboard.lock().unwrap().release_all_keys();
                    } else if event.value == 199 {
                        // keyboard backlight key pressed
                        println!("Backlight key pressed");
                    } else if event.value == 16 {
                        // brightness down key pressed
                        virtual_keyboard
                            .lock()
                            .unwrap()
                            .release_prev_and_press_keys(&[EV_KEY::KEY_BRIGHTNESSDOWN]);
                    } else if event.value == 32 {
                        // brightness up key pressed
                        virtual_keyboard
                            .lock()
                            .unwrap()
                            .release_prev_and_press_keys(&[EV_KEY::KEY_BRIGHTNESSUP]);
                    } else if event.value == 156 {
                        println!("Swap up down display key pressed");
                    } else if event.value == 124 {
                        // microphone mute key pressed
                        let mut keyboard_state = keyboard_state.lock().unwrap();
                        keyboard_state.mute_microphone_led =
                            keyboard_state.mute_microphone_led.next();
                        // TODO: control microphone mute led

                        virtual_keyboard
                            .lock()
                            .unwrap()
                            .release_prev_and_press_keys(&[EV_KEY::KEY_MICMUTE]);
                    } else if event.value == 126 {
                        println!("Emoji picker key pressed");
                        virtual_keyboard
                            .lock()
                            .unwrap()
                            .release_prev_and_press_keys(&[EV_KEY::KEY_EMOJI_PICKER]);
                    } else if event.value == 134 {
                        println!("MyASUS key pressed");
                    } else if event.value == 106 {
                        // toggle secondary display key pressed
                        toggle_secondary_display();
                    } else {
                        println!("[BT] Unknown key pressed: {:?}", event);
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                if !path.exists() {
                    println!("Event file disappeared. Exiting BT keyboard thread.");
                    return;
                } else {
                    println!("Failed to read event: {:?}", e);
                }
            }
        }
    }
}
