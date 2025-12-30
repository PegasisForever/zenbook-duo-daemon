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

    control_secondary_display(&status_path, state_manager.is_secondary_display_enabled()).await;

    // Task to handle events
    {
        let status_path = status_path.clone();
        tokio::spawn(async move {
            loop {
                match event_receiver.recv().await {
                    Ok(Event::SecondaryDisplay(new_state)) => {
                        control_secondary_display(&status_path, new_state).await;
                    }
                    Ok(_) => {}
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
                let desired_enabled = state_manager.is_secondary_display_enabled();
                if actual_enabled != desired_enabled {
                    warn!(
                        "Secondary display is not in the desired state, actual: {}, desired: {}",
                        actual_enabled, desired_enabled
                    );
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
