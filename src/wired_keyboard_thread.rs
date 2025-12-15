use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use log::{debug, info, warn};
use nusb::{
    Device, DeviceInfo, MaybeFuture as _,
    transfer::{ControlOut, ControlType, In, Interrupt, Recipient, TransferError},
};

use crate::{
    BacklightState, KeyboardState,
    config::{Config, KeyFunction},
    execute_command, parse_hex_string,
    secondary_display::control_secondary_display,
    virtual_keyboard::VirtualKeyboard,
};

pub fn find_wired_keyboard(config: &Config) -> Option<DeviceInfo> {
    nusb::list_devices()
        .wait()
        .unwrap()
        .find(|d| d.vendor_id() == config.vendor_id() && d.product_id() == config.product_id())
}

pub fn wired_keyboard_thread(
    config: &Config,
    keyboard: DeviceInfo,
    keyboard_state: Arc<Mutex<KeyboardState>>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
) {
    control_secondary_display(false);
    let keyboard = keyboard.open().wait().unwrap();
    info!("USB connected");

    let interface_4 = keyboard.detach_and_claim_interface(4).wait().unwrap();
    let mut endpoint_5 = interface_4.endpoint::<Interrupt, In>(0x85).unwrap();

    // enable fn keys
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &parse_hex_string("5ad04e00000000000000000000000000"),
            },
            Duration::from_millis(100),
        )
        .wait()
        .unwrap();

    {
        let keyboard_state = keyboard_state.lock().unwrap();
        send_backlight_state(&keyboard, keyboard_state.backlight);
    }

    let execute_key_function = |key_function: &KeyFunction| match key_function {
        KeyFunction::KeyboardBacklight(true) => {
            let mut keyboard_state = keyboard_state.lock().unwrap();
            keyboard_state.backlight = keyboard_state.backlight.next();
            send_backlight_state(&keyboard, keyboard_state.backlight);
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
        let buffer = endpoint_5.allocate(64);
        let result = endpoint_5.transfer_blocking(buffer, Duration::MAX);
        match result.status {
            Err(TransferError::Disconnected) => {
                info!("USB disconnected");
                virtual_keyboard.lock().unwrap().release_all_keys();
                control_secondary_display(true);
                return;
            }
            Err(e) => {
                warn!("USB error: {:?}", e);
            }
            Ok(_) => {
                let data = result.buffer.into_vec();
                // Only one function key can be pressed at a time, this is a hardware limitation
                if data == vec![90, 0, 0, 0, 0, 0] {
                    debug!("No key pressed");
                    virtual_keyboard.lock().unwrap().release_all_keys();
                } else if data == vec![90, 199, 0, 0, 0, 0] {
                    debug!("Backlight key pressed");
                    execute_key_function(&config.keyboard_backlight_key);
                } else if data == vec![90, 16, 0, 0, 0, 0] {
                    debug!("Brightness down key pressed");
                    execute_key_function(&config.brightness_down_key);
                } else if data == vec![90, 32, 0, 0, 0, 0] {
                    debug!("Brightness up key pressed");
                    execute_key_function(&config.brightness_up_key);
                } else if data == vec![90, 156, 0, 0, 0, 0] {
                    debug!("Swap up down display key pressed");
                    execute_key_function(&config.swap_up_down_display_key);
                } else if data == vec![90, 124, 0, 0, 0, 0] {
                    debug!("Microphone mute key pressed");
                    execute_key_function(&config.microphone_mute_key);
                } else if data == vec![90, 126, 0, 0, 0, 0] {
                    debug!("Emoji picker key pressed");
                    execute_key_function(&config.emoji_picker_key);
                } else if data == vec![90, 134, 0, 0, 0, 0] {
                    debug!("MyASUS key pressed");
                    execute_key_function(&config.myasus_key);
                } else if data == vec![90, 106, 0, 0, 0, 0] {
                    debug!("Toggle secondary display key pressed, no-op when keyboard is wired");
                } else {
                    debug!("Unknown key pressed: {:?}", data);
                    virtual_keyboard.lock().unwrap().release_all_keys();
                }
            }
        }
    }
}

fn send_backlight_state(keyboard: &Device, state: BacklightState) {
    let data = match state {
        BacklightState::Off => parse_hex_string("5abac5c4000000000000000000000000"),
        BacklightState::Low => parse_hex_string("5abac5c4010000000000000000000000"),
        BacklightState::Medium => parse_hex_string("5abac5c4020000000000000000000000"),
        BacklightState::High => parse_hex_string("5abac5c4030000000000000000000000"),
    };

    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &data,
            },
            Duration::from_millis(100),
        )
        .wait()
        .unwrap();
}

fn send_mute_microphone_state(keyboard: &Device, state: bool) {
    let data = if state {
        // turn on microphone mute led
        parse_hex_string("5ad07c01000000000000000000000000")
    } else {
        parse_hex_string("5ad07c00000000000000000000000000")
    };

    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &data,
            },
            Duration::from_millis(100),
        )
        .wait()
        .unwrap();
}
