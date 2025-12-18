use std::{
    sync::Arc,
    thread,
    time::Duration,
};

use futures_lite::stream;
use log::{debug, info, warn};
use nusb::{
    Device, DeviceInfo, MaybeFuture as _,
    hotplug::HotplugEvent,
    transfer::{ControlOut, ControlType, In, Interrupt, Recipient, TransferError},
    watch_devices,
};

use crate::{
    BacklightState,
    config::Config,
    events::{Event, KeyPressEvent},
    parse_hex_string,
    state::KeyboardStateManager,
};

pub fn find_wired_keyboard(config: &Config) -> Option<DeviceInfo> {
    nusb::list_devices()
        .wait()
        .unwrap()
        .find(|d| d.vendor_id() == config.vendor_id() && d.product_id() == config.product_id())
}

/// Monitor USB keyboard hotplug events and start wired_keyboard_thread when keyboard connects
pub fn monitor_usb_keyboard_hotplug(
    config: Config,
    event_sender: crossbeam_channel::Sender<Event>,
    event_receiver: crossbeam_channel::Receiver<Event>,
    key_press_event_sender: crossbeam_channel::Sender<KeyPressEvent>,
    state_manager: KeyboardStateManager,
) {
    for event in stream::block_on(watch_devices().unwrap()) {
        match event {
            HotplugEvent::Connected(d)
                if d.vendor_id() == config.vendor_id() && d.product_id() == config.product_id() =>
            {
                if let Some(keyboard) = find_wired_keyboard(&config) {
                    wired_keyboard_thread(
                        &config,
                        keyboard,
                        event_sender.clone(),
                        key_press_event_sender.clone(),
                        event_receiver.clone(),
                        state_manager.clone(),
                    );
                }
            }
            HotplugEvent::Disconnected(_d) => {
                // We rely on the wired_keyboard_thread to detect disconnection
            }
            _ => {}
        }
    }
}



pub fn wired_keyboard_thread(
    _config: &Config,
    keyboard: DeviceInfo,
    event_sender: crossbeam_channel::Sender<Event>,
    key_press_event_sender: crossbeam_channel::Sender<KeyPressEvent>,
    event_receiver: crossbeam_channel::Receiver<crate::events::Event>,
    state_manager: KeyboardStateManager,
) {
    let keyboard_device = Arc::new(keyboard.open().wait().unwrap());
    state_manager.set_usb_attached(true);
    event_sender.send(Event::USBKeyboardAttached).ok();
    info!("USB connected");

    let interface_4 = keyboard_device.detach_and_claim_interface(4).wait().unwrap();
    let mut endpoint_5 = interface_4.endpoint::<Interrupt, In>(0x85).unwrap();

    // enable fn keys
    keyboard_device
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

    // Restore backlight state
    let backlight_state = state_manager.get_backlight();
    send_backlight_state(&keyboard_device, backlight_state);
    
    // Restore mic mute LED state
    let mic_mute_state = state_manager.get_mic_mute_led();
    send_mute_microphone_state(&keyboard_device, mic_mute_state);

    // Spawn a thread to handle backlight/mic mute events
    let keyboard_device_control = keyboard_device.clone();
    let state_manager_control = state_manager.clone();
    thread::spawn(move || {
        for event in event_receiver.iter() {
            match event {
                Event::BacklightToggle => {
                    let new_state = state_manager_control.get_backlight().next();
                    state_manager_control.set_backlight(new_state);
                    send_backlight_state(&keyboard_device_control, new_state);
                }
                Event::Backlight(state) => {
                    state_manager_control.set_backlight(state);
                    send_backlight_state(&keyboard_device_control, state);
                }
                Event::MicMuteLed(true) => {
                    state_manager_control.set_mic_mute_led(true);
                    send_mute_microphone_state(&keyboard_device_control, true);
                }
                Event::MicMuteLed(false) => {
                    state_manager_control.set_mic_mute_led(false);
                    send_mute_microphone_state(&keyboard_device_control, false);
                }
                Event::MicMuteLedToggle => {
                    let new_state = !state_manager_control.get_mic_mute_led();
                    state_manager_control.set_mic_mute_led(new_state);
                    send_mute_microphone_state(&keyboard_device_control, new_state);
                }
                _ => {}
            }
        }
    });

    loop {
        let buffer = endpoint_5.allocate(64);
        let result = endpoint_5.transfer_blocking(buffer, Duration::MAX);
        match result.status {
            Err(TransferError::Disconnected) => {
                info!("USB disconnected");
                state_manager.set_usb_attached(false);
                event_sender.send(Event::USBKeyboardDetached).ok();
                key_press_event_sender.send(KeyPressEvent::AllKeysReleased).ok();
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
                    key_press_event_sender.send(KeyPressEvent::AllKeysReleased).ok();
                } else if data == vec![90, 199, 0, 0, 0, 0] {
                    debug!("Backlight key pressed");
                    key_press_event_sender.send(KeyPressEvent::KeyboardBacklightKeyPressed).ok();
                } else if data == vec![90, 16, 0, 0, 0, 0] {
                    debug!("Brightness down key pressed");
                    key_press_event_sender.send(KeyPressEvent::BrightnessDownKeyPressed).ok();
                } else if data == vec![90, 32, 0, 0, 0, 0] {
                    debug!("Brightness up key pressed");
                    key_press_event_sender.send(KeyPressEvent::BrightnessUpKeyPressed).ok();
                } else if data == vec![90, 156, 0, 0, 0, 0] {
                    debug!("Swap up down display key pressed");
                    key_press_event_sender.send(KeyPressEvent::SwapUpDownDisplayKeyPressed).ok();
                } else if data == vec![90, 124, 0, 0, 0, 0] {
                    debug!("Microphone mute key pressed");
                    key_press_event_sender.send(KeyPressEvent::MicrophoneMuteKeyPressed).ok();
                } else if data == vec![90, 126, 0, 0, 0, 0] {
                    debug!("Emoji picker key pressed");
                    key_press_event_sender.send(KeyPressEvent::EmojiPickerKeyPressed).ok();
                } else if data == vec![90, 134, 0, 0, 0, 0] {
                    debug!("MyASUS key pressed");
                    key_press_event_sender.send(KeyPressEvent::MyAsusKeyPressed).ok();
                } else if data == vec![90, 106, 0, 0, 0, 0] {
                    debug!("Toggle secondary display key pressed, no-op when keyboard is wired");
                    key_press_event_sender.send(KeyPressEvent::ToggleSecondaryDisplayKeyPressed).ok();
                } else {
                    debug!("Unknown key pressed: {:?}", data);
                    key_press_event_sender.send(KeyPressEvent::AllKeysReleased).ok();
                }
            }
        }
    }
}

/// USB keyboard control functions
fn send_backlight_state(keyboard: &Arc<Device>, state: BacklightState) {
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

fn send_mute_microphone_state(keyboard: &Arc<Device>, state: bool) {
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
