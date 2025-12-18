use crate::events::Event;
use std::sync::{Arc, RwLock};
use tokio::sync::broadcast;

#[derive(Clone, Copy, Debug)]
pub enum KeyboardBacklightState {
    Off,
    Low,
    Medium,
    High,
}

impl KeyboardBacklightState {
    pub fn next(&self) -> Self {
        match self {
            Self::Off => Self::Low,
            Self::Low => Self::Medium,
            Self::Medium => Self::High,
            Self::High => Self::Off,
        }
    }
}

/// Inner state structure containing all keyboard state
struct InnerState {
    backlight: KeyboardBacklightState,
    mic_mute_led: bool,
    is_idle: bool,
    is_usb_attached: bool,
    is_secondary_display_enabled: bool,
}

/// Shared state manager that maintains keyboard state across attach/detach cycles
#[derive(Clone)]
pub struct KeyboardStateManager {
    state: Arc<RwLock<InnerState>>,
    sender: broadcast::Sender<Event>,
}

impl KeyboardStateManager {
    pub fn new(is_usb_attached: bool, sender: broadcast::Sender<Event>) -> Self {
        Self {
            state: Arc::new(RwLock::new(InnerState {
                backlight: KeyboardBacklightState::Low,
                mic_mute_led: false,
                is_idle: false,
                is_usb_attached,
                is_secondary_display_enabled: !is_usb_attached,
            })),
            sender,
        }
    }

    pub fn idle_start(&self) {
        let mut state = self.state.write().unwrap();
        state.is_idle = true;
        self.sender.send(Event::MicMuteLed(false)).ok();
        self.sender
            .send(Event::Backlight(KeyboardBacklightState::Off))
            .ok();
    }

    pub fn idle_end(&self) {
        let mut state = self.state.write().unwrap();
        state.is_idle = false;
        self.sender.send(Event::MicMuteLed(state.mic_mute_led)).ok();
        self.sender.send(Event::Backlight(state.backlight)).ok();
    }

    pub fn set_mic_mute_led(&self, enabled: bool) {
        let mut state = self.state.write().unwrap();
        state.mic_mute_led = enabled;
        if !state.is_idle {
            self.sender.send(Event::MicMuteLed(enabled)).ok();
        }
    }

    pub fn toggle_mic_mute_led(&self) {
        let mut state = self.state.write().unwrap();
        state.mic_mute_led = !state.mic_mute_led;
        if !state.is_idle {
            self.sender.send(Event::MicMuteLed(state.mic_mute_led)).ok();
        }
    }

    pub fn get_mic_mute_led(&self) -> bool {
        let state = self.state.read().unwrap();
        state.mic_mute_led
    }

    pub fn set_keyboard_backlight(&self, new_state: KeyboardBacklightState) {
        let mut state = self.state.write().unwrap();
        state.backlight = new_state;
        if !state.is_idle {
            self.sender.send(Event::Backlight(new_state)).ok();
        }
    }

    pub fn toggle_keyboard_backlight(&self) {
        let mut state = self.state.write().unwrap();
        state.backlight = state.backlight.next();
        if !state.is_idle {
            self.sender.send(Event::Backlight(state.backlight)).ok();
        }
    }

    pub fn get_keyboard_backlight(&self) -> KeyboardBacklightState {
        let state = self.state.read().unwrap();
        state.backlight
    }

    pub fn set_secondary_display(&self, enabled: bool) {
        let mut state = self.state.write().unwrap();
        state.is_secondary_display_enabled = enabled;

        if state.is_usb_attached {
            state.is_secondary_display_enabled = false;
        }

        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn toggle_secondary_display(&self) {
        let mut state = self.state.write().unwrap();
        state.is_secondary_display_enabled = !state.is_secondary_display_enabled;

        if state.is_usb_attached {
            state.is_secondary_display_enabled = false;
        }

        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn set_usb_keyboard_attached(&self, attached: bool) {
        let mut state = self.state.write().unwrap();
        state.is_usb_attached = attached;

        if attached {
            state.is_secondary_display_enabled = false;
        } else {
            state.is_secondary_display_enabled = true;
        }

        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub fn is_secondary_display_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.is_secondary_display_enabled
    }

    pub fn is_idle(&self) -> bool {
        let state = self.state.read().unwrap();
        state.is_idle
    }
}
