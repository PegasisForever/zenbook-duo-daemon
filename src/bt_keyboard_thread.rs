use std::{
    io::ErrorKind,
    path::PathBuf,
    sync::{Arc, Mutex},
    thread,
    time::Duration,
};

use evdev_rs::{
    Device, DeviceWrapper as _, ReadFlag,
    enums::{EV_ABS, EventCode},
};
use inotify::{Inotify, WatchMask};
use log::{debug, info, warn};

use crate::{
    KeyboardState,
    config::{Config, KeyFunction},
    execute_command,
    secondary_display::toggle_secondary_display,
    virtual_keyboard::VirtualKeyboard,
};

pub fn bt_input_monitor_thread(
    config: &Config,
    keyboard_state: Arc<Mutex<KeyboardState>>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
) {
    for entry in std::fs::read_dir("/dev/input").unwrap() {
        let entry = entry.unwrap();

        let path = entry.path();
        try_start_bt_keyboard_thread(
            config,
            path,
            keyboard_state.clone(),
            virtual_keyboard.clone(),
        );
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
                            config,
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
    config: &Config,
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
            let config = config.clone();
            thread::spawn(move || {
                bt_keyboard_thread(&config, path, input, keyboard_state, virtual_keyboard);
            });
        }
    }
}

pub fn bt_keyboard_thread(
    config: &Config,
    path: PathBuf,
    keyboard: Device,
    keyboard_state: Arc<Mutex<KeyboardState>>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
) {
    info!("Bluetooth connected on {}", path.display());

    let execute_key_function = |key_function: &KeyFunction| match key_function {
        KeyFunction::KeyboardBacklight(true) => {
            let mut keyboard_state = keyboard_state.lock().unwrap();
            keyboard_state.backlight = keyboard_state.backlight.next();
            // TODO: control keyboard backlight
        }
        KeyFunction::ToggleSecondaryDisplay(true) => {
            toggle_secondary_display();
        }
        KeyFunction::KeyBind(items) => {
            virtual_keyboard
                .lock()
                .unwrap()
                .release_prev_and_press_keys(items);
        }
        KeyFunction::Command(command) => {
            execute_command(command);
        }
        _ => {}
    };

    loop {
        let event = keyboard.next_event(ReadFlag::NORMAL | ReadFlag::BLOCKING);

        match event {
            Ok((_status, event)) => {
                // Only one function key can be pressed at a time, this is a hardware limitation
                if event.event_code == EventCode::EV_ABS(EV_ABS::ABS_MISC) {
                    if event.value == 0 {
                        debug!("No key pressed");
                        virtual_keyboard.lock().unwrap().release_all_keys();
                    } else if event.value == 199 {
                        debug!("Backlight key pressed");
                        execute_key_function(&config.keyboard_backlight_key);
                    } else if event.value == 16 {
                        debug!("Brightness down key pressed");
                        execute_key_function(&config.brightness_down_key);
                    } else if event.value == 32 {
                        debug!("Brightness up key pressed");
                        execute_key_function(&config.brightness_up_key);
                    } else if event.value == 156 {
                        debug!("Swap up down display key pressed");
                        execute_key_function(&config.swap_up_down_display_key);
                    } else if event.value == 124 {
                        debug!("Microphone mute key pressed");
                        execute_key_function(&config.microphone_mute_key);
                    } else if event.value == 126 {
                        debug!("Emoji picker key pressed");
                        execute_key_function(&config.emoji_picker_key);
                    } else if event.value == 134 {
                        debug!("MyASUS key pressed");
                        execute_key_function(&config.myasus_key);
                    } else if event.value == 106 {
                        debug!("Toggle secondary display key pressed");
                        execute_key_function(&config.toggle_secondary_display_key);
                    } else {
                        debug!("Unknown key pressed: {:?}", event);
                        virtual_keyboard.lock().unwrap().release_all_keys();
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                if !path.exists() {
                    info!("Event file disappeared. Exiting thread.");
                    virtual_keyboard.lock().unwrap().release_all_keys();
                    return;
                } else {
                    warn!("Failed to read event: {:?}", e);
                }
            }
        }
    }
}
