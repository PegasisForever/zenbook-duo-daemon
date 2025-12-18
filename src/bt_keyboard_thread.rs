use std::{io::ErrorKind, path::PathBuf, thread, time::Duration};

use evdev_rs::{
    Device, DeviceWrapper as _, ReadFlag,
    enums::{EV_ABS, EventCode},
};
use inotify::{Inotify, WatchMask};
use log::{debug, info, warn};

use crate::{
    config::Config,
    events::{Event, KeyPressEvent},
    state::KeyboardStateManager,
};

pub fn bt_input_monitor_thread(
    config: &Config,
    key_press_event_sender: crossbeam_channel::Sender<KeyPressEvent>,
    event_receiver: crossbeam_channel::Receiver<Event>,
    state_manager: KeyboardStateManager,
) {
    for entry in std::fs::read_dir("/dev/input").unwrap() {
        let entry = entry.unwrap();
        let path = entry.path();
        try_start_bt_keyboard_thread(
            config,
            path,
            key_press_event_sender.clone(),
            event_receiver.clone(),
            state_manager.clone(),
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
                        // there may be multiple event files for the same keyboard, so multiple threads may be started
                        try_start_bt_keyboard_thread(
                            config,
                            path,
                            key_press_event_sender.clone(),
                            event_receiver.clone(),
                            state_manager.clone(),
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
    key_press_event_sender: crossbeam_channel::Sender<KeyPressEvent>,
    event_receiver: crossbeam_channel::Receiver<Event>,
    state_manager: KeyboardStateManager,
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
            let key_press_event_sender = key_press_event_sender.clone();
            let event_receiver = event_receiver.clone();
            let state_manager = state_manager.clone();
            let config = config.clone();
            thread::spawn(move || {
                bt_keyboard_thread(
                    &config,
                    path,
                    input,
                    key_press_event_sender,
                    event_receiver,
                    state_manager,
                );
            });
        }
    }
}

pub fn bt_keyboard_thread(
    config: &Config,
    path: PathBuf,
    keyboard: Device,
    key_press_event_sender: crossbeam_channel::Sender<KeyPressEvent>,
    event_receiver: crossbeam_channel::Receiver<Event>,
    state_manager: KeyboardStateManager,
) {
    info!("Bluetooth connected on {}", path.display());

    // Spawn a thread to handle backlight events
    let state_manager_control = state_manager.clone();
    let config_control = config.clone();
    thread::spawn(move || {
        for event in event_receiver.iter() {
            match event {
                Event::BacklightToggle => {
                    if let crate::config::KeyFunction::KeyboardBacklight(true) =
                        config_control.keyboard_backlight_key
                    {
                        let new_state = state_manager_control.get_backlight().next();
                        state_manager_control.set_backlight(new_state);
                        // TODO: Send backlight command to Bluetooth keyboard when implemented
                    }
                }
                Event::Backlight(state) => {
                    state_manager_control.set_backlight(state);
                    // TODO: Send backlight command to Bluetooth keyboard when implemented
                }
                _ => {}
            }
        }
    });

    loop {
        let event = keyboard.next_event(ReadFlag::NORMAL | ReadFlag::BLOCKING);

        match event {
            Ok((_status, event)) => {
                // Only one function key can be pressed at a time, this is a hardware limitation
                if event.event_code == EventCode::EV_ABS(EV_ABS::ABS_MISC) {
                    if event.value == 0 {
                        debug!("No key pressed");
                        key_press_event_sender
                            .send(KeyPressEvent::AllKeysReleased)
                            .ok();
                    } else if event.value == 199 {
                        debug!("Backlight key pressed");
                        key_press_event_sender
                            .send(KeyPressEvent::KeyboardBacklightKeyPressed)
                            .ok();
                    } else if event.value == 16 {
                        debug!("Brightness down key pressed");
                        key_press_event_sender
                            .send(KeyPressEvent::BrightnessDownKeyPressed)
                            .ok();
                    } else if event.value == 32 {
                        debug!("Brightness up key pressed");
                        key_press_event_sender
                            .send(KeyPressEvent::BrightnessUpKeyPressed)
                            .ok();
                    } else if event.value == 156 {
                        debug!("Swap up down display key pressed");
                        key_press_event_sender
                            .send(KeyPressEvent::SwapUpDownDisplayKeyPressed)
                            .ok();
                    } else if event.value == 124 {
                        debug!("Microphone mute key pressed");
                        key_press_event_sender
                            .send(KeyPressEvent::MicrophoneMuteKeyPressed)
                            .ok();
                    } else if event.value == 126 {
                        debug!("Emoji picker key pressed");
                        key_press_event_sender
                            .send(KeyPressEvent::EmojiPickerKeyPressed)
                            .ok();
                    } else if event.value == 134 {
                        debug!("MyASUS key pressed");
                        key_press_event_sender
                            .send(KeyPressEvent::MyAsusKeyPressed)
                            .ok();
                    } else if event.value == 106 {
                        debug!("Toggle secondary display key pressed");
                        key_press_event_sender
                            .send(KeyPressEvent::ToggleSecondaryDisplayKeyPressed)
                            .ok();
                    } else {
                        debug!("Unknown key pressed: {:?}", event);
                        key_press_event_sender
                            .send(KeyPressEvent::AllKeysReleased)
                            .ok();
                    }
                }
            }
            Err(e) if e.kind() == ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                if !path.exists() {
                    info!("Event file disappeared. Exiting thread.");
                    key_press_event_sender
                        .send(KeyPressEvent::AllKeysReleased)
                        .ok();
                    return;
                } else {
                    warn!("Failed to read event: {:?}", e);
                }
            }
        }
    }
}
