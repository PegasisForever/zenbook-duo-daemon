use crate::state::KeyboardBacklightState;

#[derive(Debug, Clone)]
pub enum Event {
    MicMuteLed(bool),
    Backlight(KeyboardBacklightState),
    SecondaryDisplay(bool),
    /// Keyboard seated on bottom-panel pogo USB (clamshell / secondary display policy).
    KeyboardPogoDocked(bool),
}
