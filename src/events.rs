use crate::state::BacklightState;

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
    sender: crossbeam_channel::Sender<Event>,
    receiver: crossbeam_channel::Receiver<Event>,
}

impl EventBus {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self { sender, receiver }
    }

    pub fn sender(&self) -> crossbeam_channel::Sender<Event> {
        self.sender.clone()
    }

    pub fn receiver(&self) -> crossbeam_channel::Receiver<Event> {
        self.receiver.clone()
    }
}
