use std::thread;

use crate::{
    events::Event,
    state::{BacklightState, KeyboardStateManager},
};

/// Suspend/Resume consumer - handles laptop suspend and resume events
pub fn suspend_resume_consumer(
    state_manager: KeyboardStateManager,
    event_receiver: crossbeam_channel::Receiver<Event>,
    event_sender: crossbeam_channel::Sender<Event>,
) {
    thread::spawn(move || {
        for event in event_receiver.iter() {
            match event {
                Event::LaptopSuspend => {
                    log::info!("Laptop suspending - turning off keyboard lights");
                    // Set suspended flag (getters will now return Off/false)
                    state_manager.set_suspended(true);
                    // Turn off backlight and mic mute LED hardware
                    event_sender
                        .send(Event::Backlight(BacklightState::Off))
                        .ok();
                    event_sender.send(Event::MicMuteLed(false)).ok();
                }
                Event::LaptopResume => {
                    log::info!("Laptop resuming - restoring keyboard lights");
                    // Get the raw state (actual stored state, ignoring suspended flag)
                    let backlight_state = state_manager.get_backlight_raw();
                    let mic_mute_state = state_manager.get_mic_mute_led_raw();

                    // Clear suspended flag first
                    state_manager.set_suspended(false);

                    // Restore backlight state
                    event_sender.send(Event::Backlight(backlight_state)).ok();

                    // Restore mic mute LED state
                    event_sender.send(Event::MicMuteLed(mic_mute_state)).ok();
                }
                _ => {}
            }
        }
    });
}
