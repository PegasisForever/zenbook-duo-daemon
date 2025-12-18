use std::{
    io::ErrorKind,
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use evdev_rs::{
    Device, DeviceWrapper as _, ReadFlag,
    enums::{EV_ABS, EventCode},
};
use inotify::{Inotify, WatchMask};
use log::{debug, info, warn};
use tokio::fs;
use tokio::sync::{broadcast, Mutex};
use futures::stream::StreamExt;

use crate::{
    config::Config, events::Event, state::KeyboardStateManager, virtual_keyboard::VirtualKeyboard,
};

pub fn start_bt_keyboard_monitor_task(
    config: &Config,
    event_sender: broadcast::Sender<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
) {
    // First, check existing devices
    let config_clone = config.clone();
    let virtual_keyboard_clone = virtual_keyboard.clone();
    let state_manager_clone = state_manager.clone();
    
    tokio::spawn(async move {
        // Check existing devices using async read_dir
        let mut entries = match fs::read_dir("/dev/input").await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Failed to read /dev/input: {}", e);
                return;
            }
        };
        
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            try_start_bt_keyboard_task(
                &config_clone,
                path,
                event_sender.subscribe(),
                virtual_keyboard_clone.clone(),
                state_manager_clone.clone(),
            ).await;
        }

        // Watch for new devices using async inotify
        let inotify = Inotify::init().expect("Failed to initialize inotify");
        inotify
            .watches()
            .add("/dev/input/", WatchMask::CREATE)
            .expect("Failed to add inotify watch");

        let mut buffer = [0; 1024];
        let mut stream = inotify.into_event_stream(&mut buffer).unwrap();

        while let Some(event_result) = stream.next().await {
            if let Ok(event) = event_result {
                if let Some(name) = event.name {
                    if event.mask.contains(inotify::EventMask::CREATE) {
                        if name.to_str().unwrap_or("").starts_with("event") {
                            let path = PathBuf::from("/dev/input/").join(name);
                            // there may be multiple event files for the same keyboard, so multiple tasks may be started
                            try_start_bt_keyboard_task(
                                &config_clone,
                                path,
                                event_sender.subscribe(),
                                virtual_keyboard_clone.clone(),
                                state_manager_clone.clone(),
                            ).await;
                        }
                    }
                }
            }
        }
    });
}

async fn try_start_bt_keyboard_task(
    config: &Config,
    path: PathBuf,
    event_receiver: broadcast::Receiver<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
) {
    // Check if path is a directory using async metadata
    if let Ok(metadata) = fs::metadata(&path).await {
        if metadata.is_dir() {
            return;
        }
    } else {
        return;
    }
    
    if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
        if !fname.starts_with("event") {
            return;
        }
    } else {
        return;
    }

    // evdev operations need to be done in a blocking context
    let path_clone = path.clone();
    let result = tokio::task::spawn_blocking(move || {
        evdev_rs::Device::new_from_path(&path_clone)
    }).await;

    if let Ok(Ok(input)) = result {
        // This name only matches when the keyboard is connected via Bluetooth, which is desired.
        if input.name() == Some("ASUS Zenbook Duo Keyboard") {
            start_bt_keyboard_task(
                config,
                path,
                input,
                event_receiver,
                virtual_keyboard,
                state_manager,
            );
        }
    }
}

pub fn start_bt_keyboard_task(
    config: &Config,
    path: PathBuf,
    keyboard: Device,
    mut event_receiver: broadcast::Receiver<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
) {
    info!("Bluetooth connected on {}", path.display());

    // Create a cancellation token for the control task
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel::<()>();

    // Spawn a task to handle backlight events
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = &mut shutdown_rx => {
                    info!("Bluetooth control task shutting down");
                    break;
                }
                result = event_receiver.recv() => {
                    match result {
                        Ok(Event::Backlight(_state)) => {
                            // TODO: send to keyboard device
                        }
                        Ok(Event::MicMuteLed(_enabled)) => {
                            // TODO: send to keyboard device
                        }
                        Ok(_) => {
                            // dont care about other events
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
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
    // Use spawn_blocking for the evdev read loop since it's a blocking operation
    let keyboard = Arc::new(std::sync::Mutex::new(keyboard));
    tokio::spawn(async move {
        loop {
            let keyboard_clone = keyboard.clone();
            
            // Run the blocking evdev read in a blocking thread
            let result = tokio::task::spawn_blocking(move || {
                let kb = keyboard_clone.lock().unwrap();
                kb.next_event(ReadFlag::NORMAL | ReadFlag::BLOCKING)
            }).await;
            
            match result {
                Ok(Ok((_status, event))) => {
                    // Only one function key can be pressed at a time, this is a hardware limitation
                    if event.event_code == EventCode::EV_ABS(EV_ABS::ABS_MISC) {
                        if event.value == 0 {
                            debug!("No key pressed");
                            virtual_keyboard.lock().await.release_all_keys();
                        } else if event.value == 199 {
                            debug!("Backlight key pressed");
                            config
                                .keyboard_backlight_key
                                .execute(&virtual_keyboard, &state_manager)
                                .await;
                        } else if event.value == 16 {
                            debug!("Brightness down key pressed");
                            config
                                .brightness_down_key
                                .execute(&virtual_keyboard, &state_manager)
                                .await;
                        } else if event.value == 32 {
                            debug!("Brightness up key pressed");
                            config
                                .brightness_up_key
                                .execute(&virtual_keyboard, &state_manager)
                                .await;
                        } else if event.value == 156 {
                            debug!("Swap up down display key pressed");
                            config
                                .swap_up_down_display_key
                                .execute(&virtual_keyboard, &state_manager)
                                .await;
                        } else if event.value == 124 {
                            debug!("Microphone mute key pressed");
                            config
                                .microphone_mute_key
                                .execute(&virtual_keyboard, &state_manager)
                                .await;
                        } else if event.value == 126 {
                            debug!("Emoji picker key pressed");
                            config
                                .emoji_picker_key
                                .execute(&virtual_keyboard, &state_manager)
                                .await;
                        } else if event.value == 134 {
                            debug!("MyASUS key pressed");
                            config.myasus_key.execute(&virtual_keyboard, &state_manager).await;
                        } else if event.value == 106 {
                            debug!("Toggle secondary display key pressed");
                            config
                                .toggle_secondary_display_key
                                .execute(&virtual_keyboard, &state_manager)
                                .await;
                        } else {
                            debug!("Unknown key pressed: {:?}", event);
                            virtual_keyboard.lock().await.release_all_keys();
                        }
                    }
                }
                Ok(Err(e)) if e.kind() == ErrorKind::WouldBlock => {
                    tokio::time::sleep(Duration::from_millis(50)).await;
                }
                Ok(Err(e)) => {
                    // Check if path exists using async fs
                    if !fs::try_exists(&path).await.unwrap_or(false) {
                        info!("Event file disappeared. Exiting task.");
                        virtual_keyboard.lock().await.release_all_keys();
                        drop(shutdown_tx);
                        return;
                    } else {
                        warn!("Failed to read event: {:?}", e);
                    }
                }
                Err(e) => {
                    warn!("spawn_blocking error: {:?}", e);
                    virtual_keyboard.lock().await.release_all_keys();
                    drop(shutdown_tx);
                    return;
                }
            }
        }
    });
}
