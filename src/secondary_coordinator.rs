//! Root-side coordination when `desired_secondary_enabled` changes: publish to the session
//! daemon, sysfs enable wait, and sysfs-off rules (session-stable ack vs delayed no-session).

use std::time::Duration;

use log::{info, warn};

use crate::state::KeyboardStateManager;

/// Apply a new **desired** secondary-enabled value from the root daemon (keyboard toggle,
/// `ToggleSecondaryDisplay` / `SetSecondaryDisplay` D-Bus, etc.).
pub async fn coordinate_secondary_display_to_state(
    state_manager: &KeyboardStateManager,
    new_state: bool,
) {
    if state_manager.is_secondary_display_desired_enabled() == new_state {
        return;
    }

    crate::secondary_display::pause_brightness_sync_for(Duration::from_secs(4));
    info!("SecondaryDisplay: coordinating desired state -> {}", new_state);
    state_manager.set_secondary_display_tracked(new_state).await;

    if new_state && state_manager.is_secondary_display_enabled() {
        crate::secondary_display::arm_sysfs_fallback_once();
        state_manager.emit_secondary_display_state();
        if crate::secondary_display::wait_for_secondary_display_state(true, Duration::from_secs(8))
            .await
        {
            info!("SecondaryDisplay: secondary reported enabled via sysfs");
        } else {
            warn!("SecondaryDisplay: timed out waiting for sysfs enable");
        }
    }

    let (session_registered, acked) = if new_state {
        crate::dbus_state::notify_desired_secondary_changed_wait(true, Duration::from_secs(8)).await
    } else {
        let registered = match crate::dbus_state::notify_desired_secondary_changed().await {
            Ok(registered) => registered,
            Err(e) => {
                warn!("SecondaryDisplay: D-Bus notify failed: {}", e);
                false
            }
        };
        (registered, false)
    };

    if new_state {
        if session_registered {
            if acked {
                info!("SecondaryDisplay: session applied enable");
            } else {
                warn!("SecondaryDisplay: no session ack for enable; sysfs fallback");
                crate::secondary_display::arm_sysfs_fallback_once();
                state_manager.emit_secondary_display_state();
            }
        } else {
            warn!("SecondaryDisplay: no session; sysfs fallback for enable");
            crate::secondary_display::arm_sysfs_fallback_once();
            state_manager.emit_secondary_display_state();
        }
    } else if !session_registered {
        warn!("SecondaryDisplay: no session; scheduling delayed sysfs secondary off");
        let sm = state_manager.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(600)).await;
            if sm.is_secondary_display_desired_enabled() {
                return;
            }
            crate::secondary_display::arm_sysfs_fallback_once();
            sm.emit_secondary_display_state();
        });
    } else {
        info!("SecondaryDisplay: session path; sysfs off after stable-layout ack from session");
    }
}

pub async fn coordinate_secondary_display_toggle(state_manager: &KeyboardStateManager) {
    let new_state = !state_manager.is_secondary_display_desired_enabled();
    coordinate_secondary_display_to_state(state_manager, new_state).await;
}
