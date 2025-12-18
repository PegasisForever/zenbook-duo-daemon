use crate::state::BacklightState;
use tokio::sync::broadcast;

#[derive(Debug, Clone)]
pub enum Event {
    LaptopSuspend,
    LaptopResume,

    MicMuteLed(bool), // true = on, false = off
    MicMuteLedToggle,

    Backlight(BacklightState),
    BacklightToggle,

    SecondaryDisplayToggle,
    USBKeyboardAttached,
    USBKeyboardDetached,
}

/// Event bus for distributing events to consumers
pub struct EventBus {
    sender: broadcast::Sender<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, _) = broadcast::channel(64);
        Self { sender }
    }

    pub fn sender(&self) -> broadcast::Sender<Event> {
        self.sender.clone()
    }

    pub fn receiver(&self) -> broadcast::Receiver<Event> {
        self.sender.subscribe()
    }
}
