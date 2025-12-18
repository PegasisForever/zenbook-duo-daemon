use crate::state::KeyboardBacklightState;

#[derive(Debug, Clone)]
pub enum Event {
    MicMuteLed(bool),
    Backlight(KeyboardBacklightState),
    SecondaryDisplay(bool),
}
