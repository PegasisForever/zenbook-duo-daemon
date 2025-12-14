use std::{thread, time::Duration};

use nusb::{
    MaybeFuture,
    transfer::{ControlOut, ControlType, In, Interrupt, Out, Recipient},
};

fn main() {
    env_logger::init();
    let keyboard = nusb::list_devices()
        .wait()
        .unwrap()
        .find(|d| d.vendor_id() == 0x0b05 && d.product_id() == 0x1bf2)
        .expect("Zenbook Duo Keyboard not found");
    println!("{:?}", keyboard);

    let keyboard = keyboard.open().wait().unwrap();

    let interface_4 = keyboard.detach_and_claim_interface(4).wait().unwrap();
    let mut endpoint_5 = interface_4.endpoint::<Interrupt, In>(0x85).unwrap();

    // capture #87
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &parse_hex_string("5a4153555320546563682e496e632e00"),
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(10));

    // capture #89
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &parse_hex_string("5ad03dffffff00000000000000000000"),
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(10));

    // capture #93
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &parse_hex_string("5a052031000800000000000000000000"),
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(10));

    // capture #97
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
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(10));

    // capture #103
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &parse_hex_string("5ad08f01000000000000000000000000"),
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    // capture #143
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &parse_hex_string("5ad089e0000000000000000000000000"),
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(10));

    // capture #147
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &parse_hex_string("5ad089ff000000000000000000000000"),
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    println!("Control out sent");

    let ep5_thread = thread::spawn(move || {
        let mut backlight_state = BacklightState::Low;
        let mut mute_microphone_led_state = MuteMicrophoneLEDState::Off;
        // TODO send initial state to keyboard
        loop {
            let buffer = endpoint_5.allocate(64);
            let result = endpoint_5.transfer_blocking(buffer, Duration::MAX);
            let data = result.buffer.into_vec();
            // only one function key can be pressed at a time, this is a hardware limitation
            if data == vec![90, 0, 0, 0, 0, 0] {
                println!("All function keys released");
            } else if data == vec![90, 199, 0, 0, 0, 0] {
                println!("Backlight key pressed");
                backlight_state = backlight_state.next();
                keyboard.control_out(
                    ControlOut {
                        control_type: ControlType::Class,
                        recipient: Recipient::Interface,
                        request: 0x09,
                        value: 0x035a,
                        index: 4,
                        data: &backlight_state.get_control_data(),
                    },
                    Duration::from_millis(200),
                )
                .wait()
                .unwrap();
            } else if data == vec![90, 16, 0, 0, 0, 0] {
                println!("Brightness down key pressed");
            } else if data == vec![90, 32, 0, 0, 0, 0] {
                println!("Brightness up key pressed");
            } else if data == vec![90, 156, 0, 0, 0, 0] {
                println!("Swap up down display key pressed");
            } else if data == vec![90, 124, 0, 0, 0, 0] {
                println!("Mute microphone key pressed");
                mute_microphone_led_state = mute_microphone_led_state.next();
                keyboard.control_out(
                    ControlOut {
                        control_type: ControlType::Class,
                        recipient: Recipient::Interface,
                        request: 0x09,
                        value: 0x035a,
                        index: 4,
                        data: &mute_microphone_led_state.get_control_data(),
                    },
                    Duration::from_millis(200),
                )
                .wait()
                .unwrap();
            } else if data == vec![90, 126, 0, 0, 0, 0] {
                println!("Emoji key pressed");
            } else if data == vec![90, 134, 0, 0, 0, 0] {
                println!("MyASUS key pressed");
            } else if data == vec![90, 106, 0, 0, 0, 0] {
                println!("Disable second display key pressed");
            } else {
                println!("[EP5] Unknown key pressed, {:?}", data);
            }
        }
    });

    ep5_thread.join().ok();
}

fn parse_hex_string(hex_string: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in (0..hex_string.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&hex_string[i..i + 2], 16).unwrap());
    }
    bytes
}


enum BacklightState {
    Off,
    Low,
    Medium,
    High,
}

impl BacklightState {
    fn next(&self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Off,
        }
    }

    fn get_control_data(&self) -> Vec<u8> {
        match self {
            Self::Off => parse_hex_string("5abac5c4000000000000000000000000"),
            Self::Low => parse_hex_string("5abac5c4010000000000000000000000"),
            Self::Medium => parse_hex_string("5abac5c4020000000000000000000000"),
            Self::High => parse_hex_string("5abac5c4030000000000000000000000"),
        }
    }
}

enum MuteMicrophoneLEDState {
    Off,
    On,
}

impl MuteMicrophoneLEDState {
    fn next(&self) -> Self {
        match self {
            Self::Off => Self::On,
            Self::On => Self::Off,
        }
    }

    fn get_control_data(&self) -> Vec<u8> {
        match self {
            Self::Off => parse_hex_string("5ad07c00000000000000000000000000"),
            Self::On => parse_hex_string("5ad07c01000000000000000000000000"),
        }
    }
}