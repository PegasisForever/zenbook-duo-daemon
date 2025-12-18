use std::sync::{Arc, Mutex};
use std::thread;

use crate::{
    config::{Config, KeyFunction},
    events::{Event, KeyPressEvent},
    execute_command,
    state::{BacklightState, KeyboardStateManager},
    virtual_keyboard::VirtualKeyboard,
};

/// Virtual keyboard consumer - receives key press events and maps them to actions based on config
pub fn virtual_keyboard_consumer(
    config: Config,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    key_press_event_receiver: crossbeam_channel::Receiver<KeyPressEvent>,
    event_sender: crossbeam_channel::Sender<Event>,
) {
    thread::spawn(move || {
        let execute_key_function = |key_function: &KeyFunction| {
            match key_function {
                KeyFunction::KeyBind(items) => {
                    virtual_keyboard
                        .lock()
                        .unwrap()
                        .release_prev_and_press_keys(items);
                }
                KeyFunction::Command(command) => {
                    execute_command(command);
                }
                KeyFunction::KeyboardBacklight(true) => {
                    event_sender.send(Event::BacklightToggle).ok();
                }
                KeyFunction::ToggleSecondaryDisplay(true) => {
                    event_sender.send(Event::SecondaryDisplayToggle).ok();
                }
                _ => {
                    // do nothing
                }
            }
        };
        for key_event in key_press_event_receiver.iter() {
            match key_event {
                KeyPressEvent::KeyboardBacklightKeyPressed => {
                    execute_key_function(&config.keyboard_backlight_key);
                }
                KeyPressEvent::BrightnessDownKeyPressed => {
                    execute_key_function(&config.brightness_down_key);
                }
                KeyPressEvent::BrightnessUpKeyPressed => {
                    execute_key_function(&config.brightness_up_key);
                }
                KeyPressEvent::SwapUpDownDisplayKeyPressed => {
                    execute_key_function(&config.swap_up_down_display_key);
                }
                KeyPressEvent::MicrophoneMuteKeyPressed => {
                    execute_key_function(&config.microphone_mute_key);
                }
                KeyPressEvent::EmojiPickerKeyPressed => {
                    execute_key_function(&config.emoji_picker_key);
                }
                KeyPressEvent::ToggleSecondaryDisplayKeyPressed => {
                    execute_key_function(&config.toggle_secondary_display_key);
                }
                KeyPressEvent::MyAsusKeyPressed => {
                    execute_key_function(&config.myasus_key);
                }
                KeyPressEvent::AllKeysReleased => {
                    virtual_keyboard.lock().unwrap().release_all_keys();
                }
            }
        }
    });
}

/// Suspend/Resume consumer - handles laptop suspend and resume events
pub fn suspend_resume_consumer(
    state_manager: KeyboardStateManager,
    event_receiver: crossbeam_channel::Receiver<Event>,
    event_sender: crossbeam_channel::Sender<Event>,
) {
    thread::spawn(move || {
        for event in event_receiver.iter() {
            match event {
                Event::LaptopSuspend => {
                    log::info!("Laptop suspending - turning off keyboard lights");
                    // Set suspended flag (getters will now return Off/false)
                    state_manager.set_suspended(true);
                    // Turn off backlight and mic mute LED hardware
                    event_sender
                        .send(Event::Backlight(BacklightState::Off))
                        .ok();
                    event_sender.send(Event::MicMuteLed(false)).ok();
                }
                Event::LaptopResume => {
                    log::info!("Laptop resuming - restoring keyboard lights");
                    // Get the raw state (actual stored state, ignoring suspended flag)
                    let backlight_state = state_manager.get_backlight_raw();
                    let mic_mute_state = state_manager.get_mic_mute_led_raw();

                    // Clear suspended flag first
                    state_manager.set_suspended(false);

                    // Restore backlight state
                    event_sender.send(Event::Backlight(backlight_state)).ok();

                    // Restore mic mute LED state
                    event_sender.send(Event::MicMuteLed(mic_mute_state)).ok();
                }
                _ => {}
            }
        }
    });
}
