use std::time::Duration;

use evdev_rs::enums::EV_KEY;
use nusb::{
    Device, DeviceInfo, MaybeFuture as _,
    transfer::{ControlOut, ControlType, In, Interrupt, Recipient, TransferError},
};

use crate::{
    BacklightState, KeyboardState, MuteMicrophoneState, PRODUCT_ID, VENDOR_ID,
    virtual_keyboard::VirtualKeyboard,
};

pub fn find_wired_keyboard() -> Option<DeviceInfo> {
    nusb::list_devices()
        .wait()
        .unwrap()
        .find(|d| d.vendor_id() == VENDOR_ID && d.product_id() == PRODUCT_ID)
}

pub fn wired_keyboard_thread(
    keyboard: DeviceInfo,
    keyboard_state: &mut KeyboardState,
    virtual_keyboard: &mut VirtualKeyboard,
) {
    let keyboard = keyboard.open().wait().unwrap();
    println!("Zenbook Duo Keyboard wired connected");

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

    send_backlight_state(&keyboard, keyboard_state.backlight);
    send_mute_microphone_state(&keyboard, keyboard_state.mute_microphone_led);

    loop {
        let buffer = endpoint_5.allocate(64);
        let result = endpoint_5.transfer_blocking(buffer, Duration::MAX);
        match result.status {
            Err(TransferError::Disconnected) => {
                println!("Wired keyboard disconnected");
                return;
            }
            Err(e) => {
                println!("Wired keyboard error: {:?}", e);
            }
            Ok(_) => {
                let data = result.buffer.into_vec();
                // only one function key can be pressed at a time, this is a hardware limitation
                if data == vec![90, 0, 0, 0, 0, 0] {
                    println!("All function keys released");
                    virtual_keyboard.release_all_keys();
                } else if data == vec![90, 199, 0, 0, 0, 0] {
                    println!("Backlight key pressed");
                    keyboard_state.backlight = keyboard_state.backlight.next();
                    send_backlight_state(&keyboard, keyboard_state.backlight);
                } else if data == vec![90, 16, 0, 0, 0, 0] {
                    println!("Brightness down key pressed");
                    virtual_keyboard.release_prev_and_press_keys(&[EV_KEY::KEY_BRIGHTNESSDOWN]);
                } else if data == vec![90, 32, 0, 0, 0, 0] {
                    println!("Brightness up key pressed");
                    virtual_keyboard.release_prev_and_press_keys(&[EV_KEY::KEY_BRIGHTNESSUP]);
                } else if data == vec![90, 156, 0, 0, 0, 0] {
                    println!("Swap up down display key pressed");
                } else if data == vec![90, 124, 0, 0, 0, 0] {
                    println!("Mute microphone key pressed");
                    keyboard_state.mute_microphone_led = keyboard_state.mute_microphone_led.next();
                    send_mute_microphone_state(&keyboard, keyboard_state.mute_microphone_led);

                    // virtual_keyboard.release_prev_and_press_keys(&[EV_KEY::KEY_MICMUTE]);
                    virtual_keyboard.release_prev_and_press_keys(&[
                        EV_KEY::KEY_LEFTCTRL,
                        EV_KEY::KEY_LEFTSHIFT,
                        EV_KEY::KEY_B,
                    ]);
                } else if data == vec![90, 126, 0, 0, 0, 0] {
                    println!("Emoji key pressed");
                    virtual_keyboard.release_prev_and_press_keys(&[EV_KEY::KEY_EMOJI_PICKER]);
                } else if data == vec![90, 134, 0, 0, 0, 0] {
                    println!("MyASUS key pressed");
                } else if data == vec![90, 106, 0, 0, 0, 0] {
                    println!("Disable second display key pressed");
                } else {
                    println!("[EP5] Unknown key pressed, {:?}", data);
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

fn send_mute_microphone_state(keyboard: &Device, state: MuteMicrophoneState) {
    let data = match state {
        MuteMicrophoneState::Muted => parse_hex_string("5ad07c01000000000000000000000000"),
        MuteMicrophoneState::Unmuted => parse_hex_string("5ad07c00000000000000000000000000"),
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

fn parse_hex_string(hex_string: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in (0..hex_string.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&hex_string[i..i + 2], 16).unwrap());
    }
    bytes
}
