use std::time::Duration;

use log::warn;
use tokio::fs;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::events::Event;
use crate::state::KeyboardStateManager;

async fn control_secondary_display(status_path: &str, enable: bool) {
    let data: &[u8] = if enable { b"on" } else { b"off" };
    if let Err(e) = fs::write(status_path, data).await {
        warn!("Failed to control secondary display: {}", e);
    }
}

/// Check if the secondary display is currently enabled by reading its status
async fn is_secondary_display_enabled_actual(status_path: &str) -> bool {
    if let Ok(contents) = fs::read_to_string(status_path).await {
        let status = contents.trim();
        // Display is enabled if status is "on" or "connected" (when enabled)
        status == "on" || status == "connected"
    } else {
        false
    }
}

/// Secondary display consumer - manages secondary display state and syncs with hardware
pub async fn start_secondary_display_task(
    config: Config,
    state_manager: KeyboardStateManager,
    mut event_receiver: broadcast::Receiver<Event>,
) {
    let status_path = config.secondary_display_status_path.clone();
    
    // If keyboard is attached, ensure display is disabled
    if state_manager.is_usb_attached().await {
        state_manager.set_secondary_display_enabled(false).await;
        control_secondary_display(&status_path, false).await;
    } else {
        let actual_enabled = is_secondary_display_enabled_actual(&status_path).await;
        state_manager.set_secondary_display_enabled(actual_enabled).await;
    }

    // Task to handle events
    {
        let state_manager = state_manager.clone();
        let status_path = status_path.clone();
        tokio::spawn(async move {
            loop {
                match event_receiver.recv().await {
                    Ok(event) => {
                        match event {
                            Event::SecondaryDisplayToggle => {
                                // Only allow toggle if keyboard is not attached
                                if !state_manager.is_usb_attached().await {
                                    let current_state = state_manager.is_secondary_display_enabled().await;
                                    let new_state = !current_state;
                                    state_manager.set_secondary_display_enabled(new_state).await;
                                    control_secondary_display(&status_path, new_state).await;
                                }
                            }
                            Event::USBKeyboardAttached => {
                                // Always disable display when keyboard attaches
                                state_manager.set_secondary_display_enabled(false).await;
                                control_secondary_display(&status_path, false).await;
                            }
                            Event::USBKeyboardDetached => {
                                // Enable display when keyboard detaches
                                state_manager.set_secondary_display_enabled(true).await;
                                control_secondary_display(&status_path, true).await;
                            }
                            _ => {}
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });
    }

    // Task to periodically verify and enforce secondary display state
    // For some reason the secondary display always get enabled when resuming from suspend
    {
        let state_manager = state_manager.clone();
        let status_path = status_path.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                let actual_enabled = is_secondary_display_enabled_actual(&status_path).await;
                let desired_enabled = state_manager.is_secondary_display_enabled().await;
                if actual_enabled != desired_enabled {
                    warn!("Secondary display is not in the desired state, actual: {}, desired: {}", actual_enabled, desired_enabled);
                    control_secondary_display(&status_path, desired_enabled).await;
                }
            }
        });
    }

    // Task to sync secondary display brightness
    {
        let source = config.primary_backlight_path.clone();
        let target = config.secondary_backlight_path.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                if let Ok(brightness) = fs::read_to_string(&source).await {
                    fs::write(&target, brightness.trim()).await.ok();
                }
            }
        });
    }
}
