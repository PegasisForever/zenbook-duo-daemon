use std::{fs, thread, time::Duration};

use log::warn;

use crate::config::Config;
use crate::events::Event;
use crate::state::KeyboardStateManager;

pub fn sync_secondary_display_brightness_thread(config: Config) {
    let source = config.primary_backlight_path.clone();
    let target = config.secondary_backlight_path.clone();

    thread::spawn(move || {
        loop {
            match fs::read_to_string(&source) {
                Ok(brightness) => {
                    fs::write(&target, brightness.trim()).ok();
                }
                Err(_) => {}
            }
            thread::sleep(Duration::from_millis(500));
        }
    });
}

pub fn control_secondary_display(status_path: &str, enable: bool) {
    if enable {
        fs::write(status_path, b"on").unwrap();
    } else {
        fs::write(status_path, b"off").unwrap();
    }
}

/// Check if the secondary display is currently enabled by reading its status
fn is_secondary_display_enabled_actual(status_path: &str) -> bool {
    if let Ok(contents) = fs::read_to_string(status_path) {
        let status = contents.trim();
        // Display is enabled if status is "on" or "connected" (when enabled)
        status == "on" || status == "connected"
    } else {
        false
    }
}

/// Secondary display consumer - manages secondary display state and syncs with hardware
pub fn secondary_display_consumer(
    config: Config,
    state_manager: KeyboardStateManager,
    event_receiver: crossbeam_channel::Receiver<Event>,
) {
    let status_path = config.secondary_display_status_path.clone();
    
    // If keyboard is attached, ensure display is disabled
    if state_manager.is_usb_attached() {
        state_manager.set_secondary_display_enabled(false);
        control_secondary_display(&status_path, false);
    } else {
        let actual_enabled = is_secondary_display_enabled_actual(&status_path);
        state_manager.set_secondary_display_enabled(actual_enabled);
    }

    // Thread to handle events
    {
        let state_manager = state_manager.clone();
        let status_path = status_path.clone();
        thread::spawn(move || {
            for event in event_receiver.iter() {
                match event {
                    Event::SecondaryDisplayToggle => {
                        // Only allow toggle if keyboard is not attached
                        if !state_manager.is_usb_attached() {
                            let current_state = state_manager.is_secondary_display_enabled();
                            let new_state = !current_state;
                            state_manager.set_secondary_display_enabled(new_state);
                            control_secondary_display(&status_path, new_state);
                        }
                    }
                    Event::USBKeyboardAttached => {
                        // Always disable display when keyboard attaches
                        state_manager.set_secondary_display_enabled(false);
                        control_secondary_display(&status_path, false);
                    }
                    Event::USBKeyboardDetached => {
                        // Enable display when keyboard detaches
                        state_manager.set_secondary_display_enabled(true);
                        control_secondary_display(&status_path, true);
                    }
                    _ => {}
                }
            }
        });
    }

    // Thread to periodically verify and enforce secondary display state
    // For some reason the secondary display always get enabled when resuming from suspend
    {
        let state_manager = state_manager.clone();
        let status_path = status_path.clone();
        thread::spawn(move || {
            loop {
                let actual_enabled = is_secondary_display_enabled_actual(&status_path);
                let desired_enabled = state_manager.is_secondary_display_enabled();
                if actual_enabled != desired_enabled {
                    warn!("Secondary display is not in the desired state, actual: {}, desired: {}", actual_enabled, desired_enabled);
                    control_secondary_display(&status_path, desired_enabled);
                }
                thread::sleep(Duration::from_secs(1));
            }
        });
    }
}
