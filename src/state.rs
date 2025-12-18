use std::sync::{Arc, Mutex};

#[derive(Clone, Copy, Debug)]
pub enum BacklightState {
    Off,
    Low,
    Medium,
    High,
}

impl BacklightState {
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
    backlight: BacklightState,
    mic_mute_led: bool,
    is_suspended: bool,
    is_usb_attached: bool,
    is_secondary_display_enabled: bool,
}

/// Shared state manager that maintains keyboard state across attach/detach cycles
#[derive(Clone)]
pub struct KeyboardStateManager {
    state: Arc<Mutex<InnerState>>,
}

impl KeyboardStateManager {
    pub fn new(is_usb_attached: bool) -> Self {
        Self {
            state: Arc::new(Mutex::new(InnerState {
                backlight: BacklightState::Low,
                mic_mute_led: false,
                is_suspended: false,
                is_usb_attached,
                is_secondary_display_enabled: !is_usb_attached,
            })),
        }
    }
    
    /// Get actual backlight state - returns Off if suspended, otherwise returns the actual state
    pub fn get_backlight(&self) -> BacklightState {
        let state = self.state.lock().unwrap();
        if state.is_suspended {
            BacklightState::Off
        } else {
            state.backlight
        }
    }
    
    /// Get raw backlight state (ignoring suspended flag) - used for resume restoration
    pub fn get_backlight_raw(&self) -> BacklightState {
        self.state.lock().unwrap().backlight
    }
    
    pub fn set_backlight(&self, new_state: BacklightState) {
        let mut state = self.state.lock().unwrap();
        // Always update the state (even when suspended, to preserve for resume)
        state.backlight = new_state;
    }
    
    /// Get actual mic mute LED state - returns false if suspended, otherwise returns the actual state
    pub fn get_mic_mute_led(&self) -> bool {
        let state = self.state.lock().unwrap();
        if state.is_suspended {
            false
        } else {
            state.mic_mute_led
        }
    }
    
    /// Get raw mic mute LED state (ignoring suspended flag) - used for resume restoration
    pub fn get_mic_mute_led_raw(&self) -> bool {
        self.state.lock().unwrap().mic_mute_led
    }
    
    pub fn set_mic_mute_led(&self, new_state: bool) {
        let mut state = self.state.lock().unwrap();
        // Always update the state (even when suspended, to preserve for resume)
        state.mic_mute_led = new_state;
    }
    
    /// Set the suspended state
    pub fn set_suspended(&self, suspended: bool) {
        let mut state = self.state.lock().unwrap();
        state.is_suspended = suspended;
    }
    
    /// Get the USB attached state
    pub fn is_usb_attached(&self) -> bool {
        self.state.lock().unwrap().is_usb_attached
    }
    
    /// Set the USB attached state
    pub fn set_usb_attached(&self, attached: bool) {
        let mut state = self.state.lock().unwrap();
        state.is_usb_attached = attached;
    }
    
    /// Get the secondary display enabled state
    pub fn is_secondary_display_enabled(&self) -> bool {
        self.state.lock().unwrap().is_secondary_display_enabled
    }
    
    /// Set the secondary display enabled state
    pub fn set_secondary_display_enabled(&self, enabled: bool) {
        let mut state = self.state.lock().unwrap();
        state.is_secondary_display_enabled = enabled;
    }
}
