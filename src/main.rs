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

    let interface_0 = keyboard.detach_and_claim_interface(0).wait().unwrap();
    let mut endpoint_1 = interface_0.endpoint::<Interrupt, In>(0x81).unwrap();

    let interface_4 = keyboard.detach_and_claim_interface(4).wait().unwrap();
    let mut endpoint_5 = interface_4.endpoint::<Interrupt, In>(0x85).unwrap();

    let interface_5 = keyboard.detach_and_claim_interface(5).wait().unwrap();
    let mut endpoint_6 = interface_5.endpoint::<Interrupt, In>(0x86).unwrap();

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

    // capture #99
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &parse_hex_string("5ad07c00000000000000000000000000"),
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(10));

    // capture #101
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x035a,
                index: 4,
                data: &parse_hex_string("5abac5c4010000000000000000000000"),
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

    // capture #105
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x030d,
                index: 5,
                data: &[0x0d, 0x05, 0x03, 0xa0, 0x01],
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    // capture #113
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x0307,
                index: 5,
                data: &[0x07, 0x00, 0x00],
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    // capture #117
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x030d,
                index: 5,
                data: &[0x0d, 0x05, 0x03, 0x04, 0x01],
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    // capture #121
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x030d,
                index: 5,
                data: &[0x0d, 0x05, 0x03, 0xa5, 0x01],
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    // capture #125
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x030d,
                index: 5,
                data: &[0x0d, 0x05, 0x03, 0xaa, 0x01],
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    // capture #129
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x030d,
                index: 5,
                data: &[0x0d, 0x05, 0x03, 0x05, 0x01],
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    // capture #129
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x030d,
                index: 5,
                data: &[0x0d, 0x05, 0x03, 0x02, 0x01],
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    // capture #137
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x0303,
                index: 5,
                data: &[0x03, 0x03],
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    // capture #138
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x0305,
                index: 5,
                data: &[0x05, 0x03],
            },
            Duration::from_millis(200),
        )
        .wait()
        .unwrap();
    thread::sleep(Duration::from_millis(200));

    // capture #141
    keyboard
        .control_out(
            ControlOut {
                control_type: ControlType::Class,
                recipient: Recipient::Interface,
                request: 0x09,
                value: 0x0200,
                index: 0,
                data: &[0x00],
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

    let ep1_thread = thread::spawn(move || {
        loop {
            let buffer = endpoint_1.allocate(64);
            let result = endpoint_1.transfer_blocking(buffer, Duration::MAX);
            let data = result.buffer.into_vec();
            println!("[EP1] {:?}", data);
        }
    });

    let ep5_thread = thread::spawn(move || {
        loop {
            let buffer = endpoint_5.allocate(64);
            let result = endpoint_5.transfer_blocking(buffer, Duration::MAX);
            let data = result.buffer.into_vec();
            println!("[EP5] {:?}", data);
        }
    });
    let ep6_thread = thread::spawn(move || {
        loop {
            let buffer = endpoint_6.allocate(64);
            let result = endpoint_6.transfer_blocking(buffer, Duration::MAX);

            let data = result.buffer.into_vec();
            if data == vec![1, 0, 0, 0, 0] {
                println!("[EP6] Fn pressed");
            } else {
                println!("[EP6] Unknown key pressed, {:?}", data);
            }
        }
    });

    ep1_thread.join().ok();
    ep5_thread.join().ok();
    ep6_thread.join().ok();
}

fn parse_hex_string(hex_string: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in (0..hex_string.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&hex_string[i..i + 2], 16).unwrap());
    }
    bytes
}
