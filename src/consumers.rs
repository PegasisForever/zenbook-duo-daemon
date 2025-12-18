use tokio::sync::broadcast;

use crate::{
    events::Event,
    state::{BacklightState, KeyboardStateManager},
};

/// Suspend/Resume consumer - handles laptop suspend and resume events
pub fn start_suspend_resume_control_task(
    state_manager: KeyboardStateManager,
    mut event_receiver: broadcast::Receiver<Event>,
    event_sender: broadcast::Sender<Event>,
) {
    tokio::spawn(async move {
        loop {
            match event_receiver.recv().await {
                Ok(event) => {
                    match event {
                        Event::LaptopSuspend => {
                            log::info!("Laptop suspending - turning off keyboard lights");
                            // Set suspended flag (getters will now return Off/false)
                            state_manager.set_suspended(true).await;
                            // Turn off backlight and mic mute LED hardware
                            event_sender
                                .send(Event::Backlight(BacklightState::Off))
                                .ok();
                            event_sender.send(Event::MicMuteLed(false)).ok();
                        }
                        Event::LaptopResume => {
                            log::info!("Laptop resuming - restoring keyboard lights");
                            // Clear suspended flag first
                            state_manager.set_suspended(false).await;

                            // Restore backlight state
                            event_sender.send(Event::Backlight(state_manager.get_backlight().await)).ok();

                            // Restore mic mute LED state
                            event_sender.send(Event::MicMuteLed(state_manager.get_mic_mute_led().await)).ok();
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
