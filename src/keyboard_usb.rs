use std::{sync::Arc, time::Duration};

use log::{debug, info, warn};
use nusb::{
    Device, DeviceInfo,
    hotplug::HotplugEvent,
    transfer::{ControlOut, ControlType, Interrupt, In, Recipient},
};
use tokio::sync::{broadcast, Mutex};
use futures::stream::StreamExt;

use crate::{
    BacklightState, config::Config, events::Event, parse_hex_string, state::KeyboardStateManager,
    virtual_keyboard::VirtualKeyboard,
};

pub async fn find_wired_keyboard(config: &Config) -> Option<DeviceInfo> {
    nusb::list_devices()
        .await
        .unwrap()
        .find(|d| d.vendor_id() == config.vendor_id() && d.product_id() == config.product_id())
}

/// Monitor USB keyboard hotplug events and start wired_keyboard_task when keyboard connects
pub fn start_usb_keyboard_monitor_task(
    config: &Config,
    event_sender: broadcast::Sender<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
) {
    let config = config.clone();
    tokio::spawn(async move {
        // watch_devices() returns a HotplugWatch that implements Stream
        let mut watch = nusb::watch_devices().unwrap();

        while let Some(event) = watch.next().await {
            match event {
                HotplugEvent::Connected(d)
                    if d.vendor_id() == config.vendor_id() && d.product_id() == config.product_id() =>
                {
                    if let Some(keyboard) = find_wired_keyboard(&config).await {
                        start_wired_keyboard_task(
                            &config,
                            keyboard,
                            event_sender.clone(),
                            event_sender.subscribe(),
                            virtual_keyboard.clone(),
                            state_manager.clone(),
                        ).await;
                    }
                }
                HotplugEvent::Disconnected(_d) => {
                    // We rely on the start_wired_keyboard_task to detect disconnection
                }
                _ => {}
            }
        }
    });
}

pub async fn start_wired_keyboard_task(
    config: &Config,
    keyboard: DeviceInfo,
    event_sender: broadcast::Sender<Event>,
    mut event_receiver: broadcast::Receiver<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
) {
    let keyboard_device = Arc::new(keyboard.open().await.unwrap());
    state_manager.set_usb_attached(true).await;
    event_sender.send(Event::USBKeyboardAttached).ok();
    info!("USB connected");

    let interface_4 = keyboard_device
        .detach_and_claim_interface(4)
        .await
        .unwrap();
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
        .await
        .unwrap();

    // Restore backlight state
    let backlight_state = state_manager.get_backlight().await;
    send_backlight_state(&keyboard_device, backlight_state).await;

    // Restore mic mute LED state
    let mic_mute_state = state_manager.get_mic_mute_led().await;
    send_mute_microphone_state(&keyboard_device, mic_mute_state).await;

    // Create a cancellation token for the control task
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Spawn a task to handle backlight/mic mute events
    let keyboard_device_control = keyboard_device.clone();
    let state_manager_control = state_manager.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("USB control task shutting down");
                    break;
                }
                result = event_receiver.recv() => {
                    match result {
                        Ok(event) => {
                            match event {
                                Event::BacklightToggle => {
                                    let new_state = state_manager_control.get_backlight().await.next();
                                    state_manager_control.set_backlight(new_state).await;
                                    send_backlight_state(&keyboard_device_control, new_state).await;
                                }
                                Event::Backlight(state) => {
                                    state_manager_control.set_backlight(state).await;
                                    send_backlight_state(&keyboard_device_control, state).await;
                                }
                                Event::MicMuteLedToggle => {
                                    let new_state = !state_manager_control.get_mic_mute_led().await;
                                    state_manager_control.set_mic_mute_led(new_state).await;
                                    send_mute_microphone_state(&keyboard_device_control, new_state).await;
                                }
                                Event::MicMuteLed(enabled) => {
                                    state_manager_control.set_mic_mute_led(enabled).await;
                                    send_mute_microphone_state(&keyboard_device_control, enabled).await;
                                }
                                _ => {}
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            // Skip lagged messages
                            continue;
                        }
                        Err(broadcast::error::RecvError::Closed) => {
                            break;
                        }
                    }
                }
            }
        }
    });

    let config = config.clone();
    tokio::spawn(async move {
        // Submit initial transfers to the queue
        while endpoint_5.pending() < 3 {
            endpoint_5.submit(vec![0u8; 64].into());
        }

        loop {
            let completion = endpoint_5.next_complete().await;
            
            match completion.status {
                Ok(_) => {
                    let data = &completion.buffer[..completion.actual_len];
                    // Only one function key can be pressed at a time, this is a hardware limitation
                    match data {
                        [90, 0, 0, 0, 0, 0] => {
                            debug!("No key pressed");
                            virtual_keyboard.lock().await.release_all_keys();
                        }
                        [90, 199, 0, 0, 0, 0] => {
                            debug!("Backlight key pressed");
                            config
                                .keyboard_backlight_key
                                .execute(&virtual_keyboard, &event_sender)
                                .await;
                        }
                        [90, 16, 0, 0, 0, 0] => {
                            debug!("Brightness down key pressed");
                            config
                                .brightness_down_key
                                .execute(&virtual_keyboard, &event_sender)
                                .await;
                        }
                        [90, 32, 0, 0, 0, 0] => {
                            debug!("Brightness up key pressed");
                            config
                                .brightness_up_key
                                .execute(&virtual_keyboard, &event_sender)
                                .await;
                        }
                        [90, 156, 0, 0, 0, 0] => {
                            debug!("Swap up down display key pressed");
                            config
                                .swap_up_down_display_key
                                .execute(&virtual_keyboard, &event_sender)
                                .await;
                        }
                        [90, 124, 0, 0, 0, 0] => {
                            debug!("Microphone mute key pressed");
                            config
                                .microphone_mute_key
                                .execute(&virtual_keyboard, &event_sender)
                                .await;
                        }
                        [90, 126, 0, 0, 0, 0] => {
                            debug!("Emoji picker key pressed");
                            config
                                .emoji_picker_key
                                .execute(&virtual_keyboard, &event_sender)
                                .await;
                        }
                        [90, 134, 0, 0, 0, 0] => {
                            debug!("MyASUS key pressed");
                            config.myasus_key.execute(&virtual_keyboard, &event_sender).await;
                        }
                        [90, 106, 0, 0, 0, 0] => {
                            debug!("Toggle secondary display key pressed");
                            config
                                .toggle_secondary_display_key
                                .execute(&virtual_keyboard, &event_sender)
                                .await;
                        }
                        _ => {
                            debug!("Unknown key pressed: {:?}", data);
                            virtual_keyboard.lock().await.release_all_keys();
                        }
                    }
                    // Resubmit a buffer
                    endpoint_5.submit(vec![0u8; 64].into());
                }
                Err(e) => {
                    // Check if it's a disconnect or other error
                    info!("USB disconnected or error: {:?}", e);
                    state_manager.set_usb_attached(false).await;
                    event_sender.send(Event::USBKeyboardDetached).ok();
                    virtual_keyboard.lock().await.release_all_keys();
                    drop(shutdown_tx); // Signal control task to shut down
                    return;
                }
            }
        }
    });
}

async fn send_backlight_state(keyboard: &Arc<Device>, state: BacklightState) {
    info!("Sending backlight state: {:?}", state);
    let data = match state {
        BacklightState::Off => parse_hex_string("5abac5c4000000000000000000000000"),
        BacklightState::Low => parse_hex_string("5abac5c4010000000000000000000000"),
        BacklightState::Medium => parse_hex_string("5abac5c4020000000000000000000000"),
        BacklightState::High => parse_hex_string("5abac5c4030000000000000000000000"),
    };

    if let Err(e) = keyboard
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
        .await
    {
        warn!("Failed to send backlight state: {:?}", e);
    }
}

async fn send_mute_microphone_state(keyboard: &Arc<Device>, state: bool) {
    let data = if state {
        // turn on microphone mute led
        parse_hex_string("5ad07c01000000000000000000000000")
    } else {
        parse_hex_string("5ad07c00000000000000000000000000")
    };

    if let Err(e) = keyboard
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
        .await
    {
        warn!("Failed to send mic mute state: {:?}", e);
    }
}
