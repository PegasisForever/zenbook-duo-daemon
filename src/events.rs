use crate::state::BacklightState;
use std::sync::mpmc;

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
    sender: mpmc::Sender<Event>,
    receiver: mpmc::Receiver<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, receiver) = mpmc::channel();
        Self { sender, receiver }
    }

    pub fn sender(&self) -> mpmc::Sender<Event> {
        self.sender.clone()
    }

    pub fn receiver(&self) -> mpmc::Receiver<Event> {
        self.receiver.clone()
    }
}
