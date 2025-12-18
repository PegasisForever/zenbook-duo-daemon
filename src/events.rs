use crate::state::BacklightState;

/// Key press events - sent by USB/BT keyboard threads, received by virtual_keyboard_consumer
#[derive(Debug, Clone)]
pub enum KeyPressEvent {
    KeyboardBacklightKeyPressed,
    BrightnessDownKeyPressed,
    BrightnessUpKeyPressed,
    SwapUpDownDisplayKeyPressed,
    MicrophoneMuteKeyPressed,
    EmojiPickerKeyPressed,
    MyAsusKeyPressed,
    ToggleSecondaryDisplayKeyPressed,
    AllKeysReleased,
}

/// Other events - system events, control events, etc.
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

/// Key press event bus for distributing key press events
pub struct KeyPressEventBus {
    sender: crossbeam_channel::Sender<KeyPressEvent>,
    receiver: crossbeam_channel::Receiver<KeyPressEvent>,
}

impl KeyPressEventBus {
    pub fn new() -> Self {
        let (sender, receiver) = crossbeam_channel::unbounded();
        Self { sender, receiver }
    }

    pub fn sender(&self) -> crossbeam_channel::Sender<KeyPressEvent> {
        self.sender.clone()
    }

    pub fn receiver(&self) -> crossbeam_channel::Receiver<KeyPressEvent> {
        self.receiver.clone()
    }
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
