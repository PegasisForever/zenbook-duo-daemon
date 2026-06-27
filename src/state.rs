use crate::config::{TabletMapMode, TabletMappingConfig, tablet_mode_from_str, tablet_mode_to_str};
use crate::events::Event;
use serde::{Deserialize, Serialize};
use std::path::Path;
use std::sync::{Arc, OnceLock, RwLock};
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use tokio::sync::Mutex as TokioMutex;

const STATE_DIR: &str = "/var/lib/zenbook-duo-daemon";
const BACKLIGHT_STATE_FILE: &str = "/var/lib/zenbook-duo-daemon/backlight";
const DISPLAY_STATE_FILE: &str = "/var/lib/zenbook-duo-daemon/display-state";
const BATTERY_STATE_FILE: &str = "/var/lib/zenbook-duo-daemon/keyboard-battery";
const KEYBOARD_CONNECTION_GRACE: Duration = Duration::from_secs(1);
pub(crate) const LOW_LIGHT_KEYBOARD_BACKLIGHT_LUX_THRESHOLD: f64 = 10.0;

#[derive(Default, Serialize, Deserialize)]
struct PersistedKeyboardBattery {
    pct: u8,
    charging: bool,
    full: bool,
}

#[derive(Default, Serialize, Deserialize)]
struct PersistedDisplayState {
    desired_primary: Option<String>,
    desired_secondary_enabled: Option<bool>,
    display_brightness: Option<u32>,
    ambient_light_enabled: Option<bool>,
    /// `builtin_only` | `external_only` | `all_connected` (see `README.md`, external monitors).
    desired_display_attachment: Option<String>,
    /// `mirror` | `joined`
    desired_display_layout: Option<String>,
    /// GNOME pen → panel remapping (`src/session/tablet_mapping.rs`).
    tablet_mapping_enabled: Option<bool>,
    /// `one_to_one` | `all_to_primary`
    tablet_mapping_mode: Option<String>,
}

/// Serializes all display-state JSON read/modify/write so concurrent tasks cannot clobber fields.
fn display_state_mutex() -> &'static TokioMutex<()> {
    static LOCK: OnceLock<TokioMutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| TokioMutex::new(()))
}

async fn read_display_state_file_inner() -> PersistedDisplayState {
    let path = Path::new(DISPLAY_STATE_FILE);
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return PersistedDisplayState::default();
    }

    match tokio::fs::read_to_string(path).await {
        Ok(content) => serde_json::from_str::<PersistedDisplayState>(&content).unwrap_or_default(),
        Err(_) => PersistedDisplayState::default(),
    }
}

async fn write_display_state_file_atomically(state: &PersistedDisplayState) {
    let _ = tokio::fs::create_dir_all(STATE_DIR).await;
    let Ok(json) = serde_json::to_string(state) else {
        return;
    };
    let tmp = format!("{}.tmp", DISPLAY_STATE_FILE);
    if tokio::fs::write(&tmp, &json).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return;
    }
    let _ = tokio::fs::rename(&tmp, DISPLAY_STATE_FILE).await;
}

async fn with_display_state<R>(f: impl FnOnce(&mut PersistedDisplayState) -> R) -> R {
    let _guard = display_state_mutex().lock().await;
    let mut state = read_display_state_file_inner().await;
    let before_json = serde_json::to_string(&state).unwrap_or_default();
    let out = f(&mut state);
    match serde_json::to_string(&state) {
        Ok(after_json) if after_json == before_json => {}
        Ok(_) | Err(_) => write_display_state_file_atomically(&state).await,
    }
    out
}

async fn load_battery_disk() -> Option<PersistedKeyboardBattery> {
    let path = Path::new(BATTERY_STATE_FILE);
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return None;
    }
    let content = tokio::fs::read_to_string(path).await.ok()?;
    let snap = serde_json::from_str::<PersistedKeyboardBattery>(&content).ok()?;
    if snap.pct > 100 {
        return None;
    }
    Some(snap)
}

async fn persist_battery_disk(pct: u8, charging: bool, full: bool) {
    let snap = PersistedKeyboardBattery {
        pct,
        charging,
        full,
    };
    let _ = tokio::fs::create_dir_all(STATE_DIR).await;
    let Ok(json) = serde_json::to_string(&snap) else {
        return;
    };
    let tmp = format!("{}.tmp", BATTERY_STATE_FILE);
    if tokio::fs::write(&tmp, &json).await.is_err() {
        let _ = tokio::fs::remove_file(&tmp).await;
        return;
    }
    let _ = tokio::fs::rename(&tmp, BATTERY_STATE_FILE).await;
}

async fn persist_backlight_disk(state: KeyboardBacklightState) {
    let value = match state {
        KeyboardBacklightState::Off => "0",
        KeyboardBacklightState::Low => "1",
        KeyboardBacklightState::Medium => "2",
        KeyboardBacklightState::High => "3",
    };
    let _ = tokio::fs::create_dir_all(STATE_DIR).await;
    let _ = tokio::fs::write(BACKLIGHT_STATE_FILE, value).await;
}

async fn load_backlight_disk() -> KeyboardBacklightState {
    let path = Path::new(BACKLIGHT_STATE_FILE);
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return KeyboardBacklightState::Low;
    }
    match tokio::fs::read_to_string(path).await {
        Ok(content) => match content.trim() {
            "0" => KeyboardBacklightState::Off,
            "1" => KeyboardBacklightState::Low,
            "2" => KeyboardBacklightState::Medium,
            "3" => KeyboardBacklightState::High,
            _ => KeyboardBacklightState::Low,
        },
        Err(_) => KeyboardBacklightState::Low,
    }
}

fn resolve_tablet_mapping_config(
    snap: &PersistedDisplayState,
    defaults: TabletMappingConfig,
) -> TabletMappingConfig {
    let enable = snap.tablet_mapping_enabled.unwrap_or(defaults.enable);
    let mode = snap
        .tablet_mapping_mode
        .as_deref()
        .and_then(|s| tablet_mode_from_str(s).ok())
        .unwrap_or(defaults.mode);
    TabletMappingConfig { enable, mode }
}

fn get_desired_display_mode_from_snapshot(snap: &PersistedDisplayState) -> (String, String) {
    (
        snap.desired_display_attachment
            .as_deref()
            .unwrap_or("builtin_only")
            .to_string(),
        snap.desired_display_layout
            .as_deref()
            .unwrap_or("joined")
            .to_string(),
    )
}

pub fn validate_desired_primary(primary: &str) -> Result<(), String> {
    match primary {
        "eDP-1" | "eDP-2" => Ok(()),
        other => Err(format!("desired primary must be eDP-1 or eDP-2, got {other}")),
    }
}

pub fn validate_desired_display_mode_strings(attachment: &str, layout: &str) -> Result<(), String> {
    match attachment {
        "builtin_only" | "external_only" | "all_connected" => {}
        other => return Err(format!("invalid desired_display_attachment: {other}")),
    }
    match layout {
        "mirror" | "joined" => {}
        other => return Err(format!("invalid desired_display_layout: {other}")),
    }
    Ok(())
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardBacklightState {
    Off,
    Low,
    Medium,
    High,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum KeyboardConnectionKind {
    Detached,
    Bluetooth,
    Usb,
    Pogo,
}

impl KeyboardConnectionKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Detached => "detached",
            Self::Bluetooth => "bt",
            Self::Usb => "usb",
            Self::Pogo => "pogo",
        }
    }

    fn is_connected(self) -> bool {
        self != Self::Detached
    }
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

    /// Backlight level encoding: 0 = off, 1 = low, 2 = medium, 3 = high.
    pub fn level(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }

    fn max(self, other: Self) -> Self {
        if self.level() >= other.level() {
            self
        } else {
            other
        }
    }
}

/// Inner state structure containing all keyboard state
struct InnerState {
    backlight: KeyboardBacklightState,
    mic_mute_led: bool,

    /// when suspended, both backlight and mic mute led are disabled
    is_suspended: bool,

    /// when idle, only backlight is disabled
    is_idle: bool,
    is_usb_connected: bool,
    is_pogo_docked: bool,
    bt_connected_count: usize,
    keyboard_last_connection_kind: KeyboardConnectionKind,
    keyboard_disconnected_at: Option<Instant>,
    desired_secondary_display_enabled: bool,
    is_secondary_display_enabled: bool,
    ambient_light_enabled: bool,
    ambient_keyboard_backlight_enabled: bool,
    ambient_light_available: bool,
    ambient_light_lux: Option<f64>,
    idle_timeout_seconds: u64,
    /// Mirrors persisted display-state JSON (`desired_primary`).
    desired_primary: Option<String>,
    desired_display_attachment: String,
    desired_display_layout: String,
    display_rotation: String,
    display_brightness: Option<u32>,
    /// Last keyboard battery report; USB interrupt (`0x5a 0x3d`) or BlueZ `Battery1`.
    keyboard_battery_pct: Option<u8>,
    keyboard_battery_charging: bool,
    keyboard_battery_full: bool,
    /// Last known keyboard battery persisted from any source, kept across detach / reconnect.
    keyboard_battery_last_known_pct: Option<u8>,
    keyboard_battery_last_known_full: bool,
    tablet_mapping_enabled: bool,
    tablet_mapping_mode: TabletMapMode,
    /// Incremented by operator `ApplyTabletMapping`; session watches for immediate pen remap.
    tablet_mapping_apply_nonce: u32,
}

/// Shared state manager that maintains keyboard state across attach/detach cycles
#[derive(Clone)]
pub struct KeyboardStateManager {
    state: Arc<RwLock<InnerState>>,
    sender: broadcast::Sender<Event>,
}

impl KeyboardStateManager {
    pub async fn new(
        is_usb_connected: bool,
        is_pogo_docked: bool,
        idle_timeout_seconds: u64,
        ambient_keyboard_backlight_enabled: bool,
        tablet_defaults: TabletMappingConfig,
        sender: broadcast::Sender<Event>,
    ) -> Self {
        let snap = read_display_state_file_inner().await;
        let desired_secondary_display_enabled = snap.desired_secondary_enabled.unwrap_or(true);
        let ambient_light_enabled = snap.ambient_light_enabled.unwrap_or(false);
        let is_secondary_display_enabled = if is_pogo_docked {
            false
        } else {
            desired_secondary_display_enabled
        };

        let (desired_display_attachment, desired_display_layout) = get_desired_display_mode_from_snapshot(&snap);
        let tablet_mapping = resolve_tablet_mapping_config(&snap, tablet_defaults);

        if snap.ambient_light_enabled.is_none() {
            let enabled = ambient_light_enabled;
            with_display_state(|state| {
                state.ambient_light_enabled = Some(enabled);
            })
            .await;
        }

        let battery_snap = load_battery_disk().await;
        let keyboard_battery_last_known_pct =
            match battery_snap {
                Some(ref b) if b.pct > 0 || b.full => {
                    log::info!(
                        "Keyboard battery last known from disk at startup: {}% charging={} full={}",
                        b.pct,
                        b.charging,
                        b.full
                    );
                    Some(b.pct)
                }
                _ => None,
            };

        Self {
            state: Arc::new(RwLock::new(InnerState {
                backlight: load_backlight_disk().await,
                mic_mute_led: false,
                is_suspended: false,
                is_idle: false,
                is_usb_connected,
                is_pogo_docked,
                bt_connected_count: 0,
                keyboard_last_connection_kind: if is_pogo_docked {
                    KeyboardConnectionKind::Pogo
                } else if is_usb_connected {
                    KeyboardConnectionKind::Usb
                } else {
                    KeyboardConnectionKind::Detached
                },
                keyboard_disconnected_at: None,
                desired_secondary_display_enabled,
                is_secondary_display_enabled,
                ambient_light_enabled,
                ambient_keyboard_backlight_enabled,
                ambient_light_available: false,
                ambient_light_lux: None,
                idle_timeout_seconds,
                desired_primary: snap.desired_primary.clone(),
                desired_display_attachment,
                desired_display_layout,
                display_rotation: "normal".to_string(),
                display_brightness: snap.display_brightness,
                keyboard_battery_pct: None,
                keyboard_battery_charging: false,
                keyboard_battery_full: false,
                keyboard_battery_last_known_pct,
                keyboard_battery_last_known_full: battery_snap.map(|b| b.full).unwrap_or(false),
                tablet_mapping_enabled: tablet_mapping.enable,
                tablet_mapping_mode: tablet_mapping.mode,
                tablet_mapping_apply_nonce: 0,
            })),
            sender,
        }
    }

    pub fn get_tablet_mapping_config(&self) -> TabletMappingConfig {
        let state = self.state.read().unwrap();
        TabletMappingConfig {
            enable: state.tablet_mapping_enabled,
            mode: state.tablet_mapping_mode,
        }
    }

    pub fn tablet_mapping_mode_string(&self) -> String {
        let state = self.state.read().unwrap();
        tablet_mode_to_str(state.tablet_mapping_mode).to_string()
    }

    pub fn tablet_mapping_apply_nonce(&self) -> u32 {
        self.state.read().unwrap().tablet_mapping_apply_nonce
    }

    pub async fn set_tablet_mapping_enabled(&self, enabled: bool) {
        {
            let mut state = self.state.write().unwrap();
            if state.tablet_mapping_enabled == enabled {
                return;
            }
            state.tablet_mapping_enabled = enabled;
        }
        with_display_state(|snap| {
            snap.tablet_mapping_enabled = Some(enabled);
        })
        .await;
        Self::notify_tablet_mapping_changed_async();
    }

    pub async fn set_tablet_mapping_mode(&self, mode: TabletMapMode) {
        {
            let mut state = self.state.write().unwrap();
            if state.tablet_mapping_mode == mode {
                return;
            }
            state.tablet_mapping_mode = mode;
        }
        let mode_str = tablet_mode_to_str(mode).to_string();
        with_display_state(|snap| {
            snap.tablet_mapping_mode = Some(mode_str);
        })
        .await;
        Self::notify_tablet_mapping_changed_async();
    }

    /// Cycle `one_to_one` ↔ `all_to_primary`. No-op when mapping is disabled.
    pub async fn toggle_tablet_mapping_mode(&self) -> Option<TabletMapMode> {
        let next = {
            let state = self.state.read().unwrap();
            if !state.tablet_mapping_enabled {
                return None;
            }
            match state.tablet_mapping_mode {
                TabletMapMode::OneToOne => TabletMapMode::AllToPrimary,
                TabletMapMode::AllToPrimary => TabletMapMode::OneToOne,
            }
        };
        self.set_tablet_mapping_mode(next).await;
        Some(next)
    }

    pub async fn bump_tablet_mapping_apply_nonce(&self) -> u32 {
        let nonce = {
            let mut state = self.state.write().unwrap();
            state.tablet_mapping_apply_nonce = state.tablet_mapping_apply_nonce.wrapping_add(1);
            state.tablet_mapping_apply_nonce
        };
        Self::notify_tablet_mapping_apply_nonce_async();
        nonce
    }

    fn notify_tablet_mapping_changed_async() {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = crate::dbus_state::notify_tablet_mapping_changed().await {
                    log::warn!("Failed to publish tablet mapping update: {e}");
                }
            });
        }
    }

    fn notify_tablet_mapping_apply_nonce_async() {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = crate::dbus_state::notify_tablet_mapping_apply_nonce_changed().await
                {
                    log::warn!("Failed to publish tablet mapping apply nonce: {e}");
                }
            });
        }
    }

    fn low_light_active(state: &InnerState) -> bool {
        state.ambient_keyboard_backlight_enabled
            && state.ambient_light_available
            && state
                .ambient_light_lux
                .is_some_and(|lux| lux <= LOW_LIGHT_KEYBOARD_BACKLIGHT_LUX_THRESHOLD)
    }

    fn effective_keyboard_backlight(state: &InnerState) -> KeyboardBacklightState {
        if state.is_suspended || state.is_idle {
            return KeyboardBacklightState::Off;
        }

        if Self::low_light_active(state) {
            state.backlight.max(KeyboardBacklightState::Low)
        } else {
            state.backlight
        }
    }

    fn emit_effective_keyboard_backlight(&self) {
        self.sender.send(Event::Backlight(self.get_keyboard_backlight())).ok();
    }

    fn backlight_snapshot(state: &InnerState) -> (KeyboardBacklightState, KeyboardBacklightState) {
        (state.backlight, Self::effective_keyboard_backlight(state))
    }

    fn notify_keyboard_backlight_changes_async(requested_changed: bool, effective_changed: bool) {
        if !requested_changed && !effective_changed {
            return;
        }
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = crate::dbus_state::notify_keyboard_backlight_changes(
                    requested_changed,
                    effective_changed,
                )
                .await
                {
                    log::warn!("Failed to publish keyboard backlight update: {e}");
                }
            });
        }
    }

    pub fn suspend_start(&self) {
        let effective_changed = {
            let mut state = self.state.write().unwrap();
            let (_, previous_effective) = Self::backlight_snapshot(&state);
            state.is_suspended = true;
            let (_, current_effective) = Self::backlight_snapshot(&state);
            previous_effective != current_effective
        };
        self.sender.send(Event::MicMuteLed(false)).ok();
        if effective_changed {
            self.sender
                .send(Event::Backlight(KeyboardBacklightState::Off))
                .ok();
        }
        Self::notify_keyboard_backlight_changes_async(false, effective_changed);
    }

    pub fn suspend_end(&self) {
        let effective_changed = {
            let mut state = self.state.write().unwrap();
            let (_, previous_effective) = Self::backlight_snapshot(&state);
            state.is_suspended = false;
            let (_, current_effective) = Self::backlight_snapshot(&state);
            previous_effective != current_effective
        };
        self.sender
            .send(Event::MicMuteLed(self.get_mic_mute_led()))
            .ok();
        if effective_changed {
            self.emit_effective_keyboard_backlight();
        }
        Self::notify_keyboard_backlight_changes_async(false, effective_changed);
    }

    pub fn idle_start(&self) {
        let effective_changed = {
            let mut state = self.state.write().unwrap();
            let (_, previous_effective) = Self::backlight_snapshot(&state);
            state.is_idle = true;
            let (_, current_effective) = Self::backlight_snapshot(&state);
            previous_effective != current_effective
        };
        if effective_changed {
            self.sender
                .send(Event::Backlight(KeyboardBacklightState::Off))
                .ok();
        }
        Self::notify_keyboard_backlight_changes_async(false, effective_changed);
    }

    pub fn idle_end(&self) {
        let effective_changed = {
            let mut state = self.state.write().unwrap();
            let (_, previous_effective) = Self::backlight_snapshot(&state);
            state.is_idle = false;
            let (_, current_effective) = Self::backlight_snapshot(&state);
            previous_effective != current_effective
        };
        if effective_changed {
            self.emit_effective_keyboard_backlight();
        }
        Self::notify_keyboard_backlight_changes_async(false, effective_changed);
    }

    pub fn set_mic_mute_led(&self, enabled: bool) {
        let changed = {
            let mut state = self.state.write().unwrap();
            let changed = state.mic_mute_led != enabled;
            state.mic_mute_led = enabled;
            if !state.is_suspended {
                self.sender.send(Event::MicMuteLed(enabled)).ok();
            }
            changed
        };
        if changed {
            Self::notify_mic_mute_led_changed_async();
        }
    }

    pub fn get_mic_mute_led(&self) -> bool {
        let state = self.state.read().unwrap();
        if state.is_suspended {
            false
        } else {
            state.mic_mute_led
        }
    }

    pub async fn set_keyboard_backlight(&self, new_state: KeyboardBacklightState) {
        let ((previous_requested, previous_effective), (current_requested, current_effective)) = {
            let mut state = self.state.write().unwrap();
            let previous = Self::backlight_snapshot(&state);
            state.backlight = new_state;
            let current = Self::backlight_snapshot(&state);
            (previous, current)
        };
        persist_backlight_disk(new_state).await;
        if previous_requested != current_requested || previous_effective != current_effective {
            self.sender.send(Event::Backlight(current_effective)).ok();
        }
        Self::notify_keyboard_backlight_changes_async(
            previous_requested != current_requested,
            previous_effective != current_effective,
        );
    }

    pub async fn toggle_keyboard_backlight(&self) {
        let (next, previous_requested, current_requested, previous_effective, current_effective) = {
            let mut state = self.state.write().unwrap();
            let (previous_requested, previous_effective) = Self::backlight_snapshot(&state);
            state.backlight = state.backlight.next();
            let (current_requested, current_effective) = Self::backlight_snapshot(&state);
            (
                state.backlight,
                previous_requested,
                current_requested,
                previous_effective,
                current_effective,
            )
        };
        persist_backlight_disk(next).await;
        if previous_requested != current_requested || previous_effective != current_effective {
            self.sender.send(Event::Backlight(current_effective)).ok();
        }
        Self::notify_keyboard_backlight_changes_async(
            previous_requested != current_requested,
            previous_effective != current_effective,
        );
    }

    pub fn requested_keyboard_backlight(&self) -> KeyboardBacklightState {
        self.state.read().unwrap().backlight
    }

    pub fn get_keyboard_backlight(&self) -> KeyboardBacklightState {
        let state = self.state.read().unwrap();
        Self::effective_keyboard_backlight(&state)
    }

    pub async fn set_secondary_display_tracked(&self, enabled: bool) {
        {
            let mut state = self.state.write().unwrap();
            state.desired_secondary_display_enabled = enabled;
            state.is_secondary_display_enabled = if state.is_pogo_docked { false } else { enabled };
        }
        with_display_state(|state| {
            state.desired_secondary_enabled = Some(enabled);
        })
        .await;
    }

    fn raw_keyboard_connection_kind(state: &InnerState) -> KeyboardConnectionKind {
        if state.is_pogo_docked {
            KeyboardConnectionKind::Pogo
        } else if state.is_usb_connected {
            KeyboardConnectionKind::Usb
        } else if state.bt_connected_count > 0 {
            KeyboardConnectionKind::Bluetooth
        } else {
            KeyboardConnectionKind::Detached
        }
    }

    fn effective_keyboard_connection_kind(state: &InnerState) -> KeyboardConnectionKind {
        let raw = Self::raw_keyboard_connection_kind(state);
        if raw.is_connected() {
            raw
        } else if state
            .keyboard_disconnected_at
            .is_some_and(|at| at.elapsed() < KEYBOARD_CONNECTION_GRACE)
        {
            state.keyboard_last_connection_kind
        } else {
            KeyboardConnectionKind::Detached
        }
    }

    fn update_keyboard_connection_tracking(state: &mut InnerState) -> bool {
        let raw = Self::raw_keyboard_connection_kind(state);
        if raw.is_connected() {
            state.keyboard_last_connection_kind = raw;
            state.keyboard_disconnected_at = None;
            false
        } else if state.keyboard_last_connection_kind.is_connected()
            && state.keyboard_disconnected_at.is_none()
        {
            state.keyboard_disconnected_at = Some(Instant::now());
            true
        } else {
            false
        }
    }

    /// Update USB presence and/or pogo-dock state (from hotplug / resume resync).
    pub fn set_usb_keyboard_connection(&self, usb_connected: bool, pogo_docked: bool) {
        let (usb_changed, pogo_changed, secondary_enabled, refresh_bt_battery, grace_started) = {
            let mut state = self.state.write().unwrap();
            let usb_changed = state.is_usb_connected != usb_connected;
            let pogo_changed = state.is_pogo_docked != pogo_docked;
            state.is_usb_connected = usb_connected;
            state.is_pogo_docked = pogo_docked;
            let refresh_bt_battery =
                usb_changed && !usb_connected && state.bt_connected_count > 0;
            if !usb_connected {
                state.keyboard_battery_pct = None;
                state.keyboard_battery_charging = false;
                state.keyboard_battery_full = false;
            }
            let secondary_enabled = if pogo_docked {
                false
            } else {
                state.desired_secondary_display_enabled
            };
            let secondary_changed = state.is_secondary_display_enabled != secondary_enabled;
            state.is_secondary_display_enabled = secondary_enabled;
            let grace_started = Self::update_keyboard_connection_tracking(&mut state);
            (
                usb_changed,
                pogo_changed,
                (secondary_enabled, secondary_changed),
                refresh_bt_battery,
                grace_started,
            )
        };

        if refresh_bt_battery {
            let sm = self.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    crate::keyboard_battery_bt::refresh_keyboard_battery_from_bluetooth(&sm).await;
                });
            }
        }

        if usb_changed && usb_connected {
            let sm = self.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    sm.restore_keyboard_battery_from_disk().await;
                });
            } else {
                log::warn!(
                    "USB connect: no tokio runtime handle; keyboard battery restore skipped until next sample"
                );
            }
        }

        if usb_changed {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = crate::dbus_state::notify_keyboard_usb_connected_changed().await
                    {
                        log::warn!("Failed to publish keyboard_usb_connected update: {e}");
                    }
                    if let Err(e) = crate::dbus_state::notify_keyboard_battery_changed().await {
                        log::warn!("Failed to publish keyboard battery update: {e}");
                    }
                    if let Err(e) = crate::dbus_state::notify_keyboard_connection_changed().await {
                        log::warn!("Failed to publish keyboard connection update: {e}");
                    }
                });
            }
        }
        if pogo_changed {
            self.sender
                .send(Event::KeyboardPogoDocked(pogo_docked))
                .ok();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async {
                    if let Err(e) = crate::dbus_state::notify_keyboard_pogo_docked_changed().await {
                        log::warn!("Failed to publish keyboard_pogo_docked update: {e}");
                    }
                    if let Err(e) = crate::dbus_state::notify_keyboard_battery_changed().await {
                        log::warn!("Failed to publish keyboard battery update: {e}");
                    }
                    if let Err(e) = crate::dbus_state::notify_keyboard_connection_changed().await {
                        log::warn!("Failed to publish keyboard connection update: {e}");
                    }
                });
            }
        }
        if grace_started {
            Self::schedule_keyboard_connection_refresh_async();
        }
        if secondary_enabled.1 {
            self.sender
                .send(Event::SecondaryDisplay(secondary_enabled.0))
                .ok();
        }
    }

    /// Side USB or pogo — keyboard available over USB (charge, HID, battery).
    pub fn is_usb_keyboard_connected(&self) -> bool {
        let state = self.state.read().unwrap();
        state.is_usb_connected
    }

    pub fn is_keyboard_connected(&self) -> bool {
        let state = self.state.read().unwrap();
        Self::effective_keyboard_connection_kind(&state).is_connected()
    }

    pub fn keyboard_connection_kind(&self) -> KeyboardConnectionKind {
        let state = self.state.read().unwrap();
        Self::effective_keyboard_connection_kind(&state)
    }

    /// USB interrupt report (`0x5a 0x3d`); `None` when no source has reported yet.
    pub fn keyboard_battery_percentage(&self) -> Option<u8> {
        let state = self.state.read().unwrap();
        state.keyboard_battery_pct
    }

    pub fn keyboard_battery_last_known_percentage(&self) -> Option<u8> {
        let state = self.state.read().unwrap();
        state.keyboard_battery_last_known_pct
    }

    pub fn is_keyboard_battery_charging(&self) -> bool {
        let state = self.state.read().unwrap();
        state.keyboard_battery_charging
    }

    pub fn is_keyboard_battery_effective_charging(&self) -> bool {
        let state = self.state.read().unwrap();
        if state.keyboard_battery_pct.is_some() {
            state.keyboard_battery_charging
        } else if state.keyboard_battery_last_known_full {
            false
        } else {
            state.is_usb_connected || state.is_pogo_docked
        }
    }

    pub fn is_keyboard_battery_full(&self) -> bool {
        let state = self.state.read().unwrap();
        state.keyboard_battery_full
    }

    pub fn display_rotation(&self) -> String {
        self.state.read().unwrap().display_rotation.clone()
    }

    pub fn set_display_rotation(&self, rotation: String) {
        let changed = {
            let mut state = self.state.write().unwrap();
            if state.display_rotation == rotation {
                false
            } else {
                state.display_rotation = rotation;
                true
            }
        };
        if changed {
            Self::notify_display_rotation_changed_async();
        }
    }

    pub fn set_keyboard_battery_usb(&self, pct: u8, status: u8) {
        use crate::keyboard_battery::{usb_battery_charging, usb_battery_full};

        let (changed, pct, charging, full) = {
            let mut state = self.state.write().unwrap();
            if !state.is_usb_connected {
                return;
            }
            let charging = usb_battery_charging(status, pct);
            let full = usb_battery_full(status, pct);
            let changed = state.keyboard_battery_pct != Some(pct)
                || state.keyboard_battery_charging != charging
                || state.keyboard_battery_full != full
                || state.keyboard_battery_last_known_pct != Some(pct)
                || state.keyboard_battery_last_known_full != full;
            state.keyboard_battery_pct = Some(pct);
            state.keyboard_battery_charging = charging;
            state.keyboard_battery_full = full;
            state.keyboard_battery_last_known_pct = Some(pct);
            state.keyboard_battery_last_known_full = full;
            (changed, pct, charging, full)
        };

        if changed {
            let port = {
                let state = self.state.read().unwrap();
                if state.is_pogo_docked {
                    "POGO"
                } else {
                    "side USB"
                }
            };
            log::debug!(
                "{port}: keyboard battery D-Bus updated to {pct}% charging={charging} full={full} (5a 3d status=0x{status:02x})"
            );
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let charging_p = charging;
                let full_p = full;
                handle.spawn(async move {
                    persist_battery_disk(pct, charging_p, full_p).await;
                });
            }
            Self::notify_keyboard_battery_changed_async();
        }
    }

    /// Refresh the last-known keyboard battery from disk while waiting for a fresh live sample.
    pub async fn restore_keyboard_battery_from_disk(&self) {
        let Some(snap) = load_battery_disk().await else {
            return;
        };
        if snap.pct == 0 && !snap.full {
            return;
        }
        let (changed, pct, charging, full) = {
            let mut state = self.state.write().unwrap();
            let changed = state.keyboard_battery_last_known_pct != Some(snap.pct)
                || state.keyboard_battery_last_known_full != snap.full;
            state.keyboard_battery_last_known_pct = Some(snap.pct);
            state.keyboard_battery_last_known_full = snap.full;
            (changed, snap.pct, snap.charging, snap.full)
        };
        if changed {
            log::info!(
                "Keyboard battery last known restored from disk: {pct}% charging={charging} full={full}"
            );
            Self::notify_keyboard_battery_changed_async();
        }
    }

    /// BlueZ `Battery1` while detached on Bluetooth (ignored when side USB is connected).
    pub fn set_keyboard_battery_bluetooth(&self, pct: u8) {
        use crate::keyboard_battery::{BATTERY_FULL_PCT_THRESHOLD, usb_battery_full};

        let (changed, full) = {
            let mut state = self.state.write().unwrap();
            if state.is_usb_connected {
                return;
            }
            let charging = false;
            let full = usb_battery_full(0, pct);
            let changed = state.keyboard_battery_pct != Some(pct)
                || state.keyboard_battery_charging != charging
                || state.keyboard_battery_full != full
                || state.keyboard_battery_last_known_pct != Some(pct)
                || state.keyboard_battery_last_known_full != full;
            state.keyboard_battery_pct = Some(pct);
            state.keyboard_battery_charging = charging;
            state.keyboard_battery_full = full;
            state.keyboard_battery_last_known_pct = Some(pct);
            state.keyboard_battery_last_known_full = full;
            (changed, full)
        };

        if changed {
            log::info!("Bluetooth keyboard battery: {pct}% full={full} (threshold={BATTERY_FULL_PCT_THRESHOLD})");
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                let charging_p = false;
                let full_p = full;
                handle.spawn(async move {
                    persist_battery_disk(pct, charging_p, full_p).await;
                });
            }
            Self::notify_keyboard_battery_changed_async();
        }
    }

    /// Clear battery properties when neither USB nor Bluetooth can supply a level.
    pub fn clear_keyboard_battery_if_no_source(&self) {
        let cleared = {
            let mut state = self.state.write().unwrap();
            if state.is_usb_connected || state.bt_connected_count > 0 {
                return;
            }
            let had = state.keyboard_battery_pct.is_some();
            state.keyboard_battery_pct = None;
            state.keyboard_battery_charging = false;
            state.keyboard_battery_full = false;
            had
        };
        if cleared {
            Self::notify_keyboard_battery_changed_async();
        }
    }

    fn notify_keyboard_battery_changed_async() {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = crate::dbus_state::notify_keyboard_battery_changed().await {
                    log::warn!("Failed to publish keyboard battery update: {e}");
                }
            });
        }
    }

    fn schedule_keyboard_connection_refresh_async() {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                tokio::time::sleep(KEYBOARD_CONNECTION_GRACE).await;
                if let Err(e) = crate::dbus_state::notify_keyboard_connection_changed().await {
                    log::warn!("Failed to publish keyboard connection grace expiry: {e}");
                }
            });
        }
    }

    fn notify_mic_mute_led_changed_async() {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = crate::dbus_state::notify_mic_mute_led_changed().await {
                    log::warn!("Failed to publish mic mute LED update: {e}");
                }
            });
        }
    }

    fn notify_display_rotation_changed_async() {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = crate::dbus_state::notify_display_rotation_changed().await {
                    log::warn!("Failed to publish display rotation update: {e}");
                }
            });
        }
    }

    fn notify_ambient_keyboard_backlight_changed_async() {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = crate::dbus_state::notify_ambient_keyboard_backlight_changed().await {
                    log::warn!("Failed to publish ambient keyboard backlight settings update: {e}");
                }
            });
        }
    }

    fn notify_idle_timeout_changed_async() {
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            handle.spawn(async move {
                if let Err(e) = crate::dbus_state::notify_idle_timeout_changed().await {
                    log::warn!("Failed to publish idle timeout update: {e}");
                }
            });
        }
    }

    /// Bottom-panel pogo dock — clamshell / secondary display power policy.
    pub fn is_keyboard_pogo_docked(&self) -> bool {
        let state = self.state.read().unwrap();
        state.is_pogo_docked
    }

    pub fn is_secondary_display_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.is_secondary_display_enabled
    }

    /// Increments the Bluetooth refcount used for D-Bus `bluetooth_connected`.
    /// Call **once per logical BT keyboard session** (e.g. one MAC), not once per evdev sibling path.
    pub fn bluetooth_connection_started(&self) {
        let (was_connected, is_connected) = {
            let mut state = self.state.write().unwrap();
            let was_connected = state.bt_connected_count > 0;
            state.bt_connected_count = state.bt_connected_count.saturating_add(1);
            let is_connected = state.bt_connected_count > 0;
            Self::update_keyboard_connection_tracking(&mut state);
            (was_connected, is_connected)
        };
        if was_connected != is_connected {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = crate::dbus_state::notify_bluetooth_connected_changed().await {
                        log::warn!("Failed to publish bluetooth_connected update over D-Bus: {}", e);
                    }
                    if let Err(e) = crate::dbus_state::notify_keyboard_connection_changed().await {
                        log::warn!("Failed to publish keyboard connection update over D-Bus: {}", e);
                    }
                });
            }
        }
        if !was_connected && is_connected {
            let sm = self.clone();
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    sm.restore_keyboard_battery_from_disk().await;
                    crate::keyboard_battery_bt::refresh_keyboard_battery_from_bluetooth(&sm).await;
                });
            }
        }
    }

    /// Decrements the Bluetooth refcount. Safe to reason about as **idempotent for over-stops**:
    /// extra `saturating_sub` calls when the count is already `0` do not wrap and do not re-emit
    /// D-Bus transitions.
    pub fn bluetooth_connection_stopped(&self) {
        let (was_connected, is_connected, should_clear_battery, grace_started) = {
            let mut state = self.state.write().unwrap();
            let was_connected = state.bt_connected_count > 0;
            state.bt_connected_count = state.bt_connected_count.saturating_sub(1);
            let is_connected = state.bt_connected_count > 0;
            let should_clear_battery =
                was_connected && !is_connected && !state.is_usb_connected;
            if should_clear_battery {
                state.keyboard_battery_pct = None;
                state.keyboard_battery_charging = false;
                state.keyboard_battery_full = false;
            }
            let grace_started = Self::update_keyboard_connection_tracking(&mut state);
            (was_connected, is_connected, should_clear_battery, grace_started)
        };
        if was_connected != is_connected {
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = crate::dbus_state::notify_bluetooth_connected_changed().await {
                        log::warn!("Failed to publish bluetooth_connected update over D-Bus: {}", e);
                    }
                    if let Err(e) = crate::dbus_state::notify_keyboard_connection_changed().await {
                        log::warn!("Failed to publish keyboard connection update over D-Bus: {}", e);
                    }
                });
            }
        }
        if grace_started {
            Self::schedule_keyboard_connection_refresh_async();
        }
        if should_clear_battery {
            Self::notify_keyboard_battery_changed_async();
        }
    }

    pub fn is_bluetooth_connected(&self) -> bool {
        let state = self.state.read().unwrap();
        state.bt_connected_count > 0
    }

    pub fn is_secondary_display_desired_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.desired_secondary_display_enabled
    }

    pub fn emit_secondary_display_state(&self) {
        let state = self.state.read().unwrap();
        self.sender
            .send(Event::SecondaryDisplay(state.is_secondary_display_enabled))
            .ok();
    }

    pub async fn set_desired_primary(&self, primary: &str) {
        with_display_state(|state| {
            state.desired_primary = Some(primary.to_string());
        })
        .await;
        let mut state = self.state.write().unwrap();
        state.desired_primary = Some(primary.to_string());
    }

    pub fn get_desired_primary(&self) -> Option<String> {
        let state = self.state.read().unwrap();
        state.desired_primary.clone()
    }

    pub fn get_desired_display_mode(&self) -> (String, String) {
        let state = self.state.read().unwrap();
        (
            state.desired_display_attachment.clone(),
            state.desired_display_layout.clone(),
        )
    }

    pub async fn persist_desired_display_mode(&self, attachment: &str, layout: &str) {
        with_display_state(|state| {
            state.desired_display_attachment = Some(attachment.to_string());
            state.desired_display_layout = Some(layout.to_string());
        })
        .await;
        let mut state = self.state.write().unwrap();
        state.desired_display_attachment = attachment.to_string();
        state.desired_display_layout = layout.to_string();
    }

    pub async fn set_display_brightness_value(&self, brightness: u32) {
        let changed = with_display_state(|state| {
            if state.display_brightness == Some(brightness) {
                false
            } else {
                state.display_brightness = Some(brightness);
                true
            }
        })
        .await;
        if changed {
            {
                let mut st = self.state.write().unwrap();
                st.display_brightness = Some(brightness);
            }
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    if let Err(e) = crate::dbus_state::notify_display_brightness_changed().await {
                        log::warn!("Failed to publish display_brightness update over D-Bus: {}", e);
                    }
                });
            }
        }
    }

    pub fn get_display_brightness_value(&self) -> Option<u32> {
        let state = self.state.read().unwrap();
        state.display_brightness
    }

    pub async fn set_ambient_light_enabled(&self, enabled: bool) {
        {
            let mut state = self.state.write().unwrap();
            state.ambient_light_enabled = enabled;
        }
        with_display_state(|state| {
            state.ambient_light_enabled = Some(enabled);
        })
        .await;
    }

    pub fn get_ambient_light_enabled(&self) -> bool {
        let state = self.state.read().unwrap();
        state.ambient_light_enabled
    }

    pub async fn set_ambient_keyboard_backlight_enabled(&self, enabled: bool) {
        let (changed, previous_effective, current_effective) = {
            let mut state = self.state.write().unwrap();
            if state.ambient_keyboard_backlight_enabled == enabled {
                return;
            }
            let (_, previous_effective) = Self::backlight_snapshot(&state);
            state.ambient_keyboard_backlight_enabled = enabled;
            let (_, current_effective) = Self::backlight_snapshot(&state);
            (true, previous_effective, current_effective)
        };
        if previous_effective != current_effective {
            self.sender.send(Event::Backlight(current_effective)).ok();
        }
        if changed {
            Self::notify_ambient_keyboard_backlight_changed_async();
            Self::notify_keyboard_backlight_changes_async(
                false,
                previous_effective != current_effective,
            );
        }
    }

    pub fn ambient_keyboard_backlight_enabled(&self) -> bool {
        self.state.read().unwrap().ambient_keyboard_backlight_enabled
    }

    pub fn ambient_light_available(&self) -> bool {
        self.state.read().unwrap().ambient_light_available
    }

    pub async fn set_ambient_light_sensor_available(&self, available: bool) {
        let (previous_effective, current_effective) = {
            let mut state = self.state.write().unwrap();
            if state.ambient_light_available == available {
                return;
            }
            let (_, previous_effective) = Self::backlight_snapshot(&state);
            state.ambient_light_available = available;
            if !available {
                state.ambient_light_lux = None;
            }
            let (_, current_effective) = Self::backlight_snapshot(&state);
            (previous_effective, current_effective)
        };
        if previous_effective != current_effective {
            self.sender.send(Event::Backlight(current_effective)).ok();
        }
        Self::notify_ambient_keyboard_backlight_changed_async();
        Self::notify_keyboard_backlight_changes_async(false, previous_effective != current_effective);
    }

    pub async fn set_ambient_light_lux(&self, lux: Option<f64>) {
        let (previous_effective, current_effective) = {
            let mut state = self.state.write().unwrap();
            if state.ambient_light_lux == lux {
                return;
            }
            let (_, previous_effective) = Self::backlight_snapshot(&state);
            state.ambient_light_lux = lux;
            let (_, current_effective) = Self::backlight_snapshot(&state);
            (previous_effective, current_effective)
        };
        if previous_effective != current_effective {
            self.sender.send(Event::Backlight(current_effective)).ok();
        }
        Self::notify_keyboard_backlight_changes_async(false, previous_effective != current_effective);
    }

    pub fn idle_timeout_seconds(&self) -> u64 {
        self.state.read().unwrap().idle_timeout_seconds
    }

    pub async fn set_idle_timeout_seconds(&self, seconds: u64) {
        let changed = {
            let mut state = self.state.write().unwrap();
            if state.idle_timeout_seconds == seconds {
                false
            } else {
                state.idle_timeout_seconds = seconds;
                true
            }
        };
        if changed {
            Self::notify_idle_timeout_changed_async();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample_state() -> InnerState {
        InnerState {
            backlight: KeyboardBacklightState::Off,
            mic_mute_led: false,
            is_suspended: false,
            is_idle: false,
            is_usb_connected: false,
            is_pogo_docked: false,
            bt_connected_count: 0,
            keyboard_last_connection_kind: KeyboardConnectionKind::Detached,
            keyboard_disconnected_at: None,
            desired_secondary_display_enabled: true,
            is_secondary_display_enabled: true,
            ambient_light_enabled: false,
            ambient_keyboard_backlight_enabled: false,
            ambient_light_available: false,
            ambient_light_lux: None,
            idle_timeout_seconds: 300,
            desired_primary: Some("eDP-1".to_string()),
            desired_display_attachment: "builtin_only".to_string(),
            desired_display_layout: "joined".to_string(),
            display_rotation: "normal".to_string(),
            display_brightness: None,
            keyboard_battery_pct: None,
            keyboard_battery_charging: false,
            keyboard_battery_full: false,
            keyboard_battery_last_known_pct: None,
            keyboard_battery_last_known_full: false,
            tablet_mapping_enabled: true,
            tablet_mapping_mode: TabletMapMode::OneToOne,
            tablet_mapping_apply_nonce: 0,
        }
    }

    #[test]
    fn low_light_promotes_off_to_low() {
        let mut state = sample_state();
        state.ambient_keyboard_backlight_enabled = true;
        state.ambient_light_available = true;
        state.ambient_light_lux = Some(LOW_LIGHT_KEYBOARD_BACKLIGHT_LUX_THRESHOLD);

        assert_eq!(
            KeyboardStateManager::effective_keyboard_backlight(&state).level(),
            KeyboardBacklightState::Low.level()
        );
    }

    #[test]
    fn idle_still_forces_backlight_off() {
        let mut state = sample_state();
        state.backlight = KeyboardBacklightState::High;
        state.ambient_keyboard_backlight_enabled = true;
        state.ambient_light_available = true;
        state.ambient_light_lux = Some(1.0);
        state.is_idle = true;

        assert_eq!(
            KeyboardStateManager::effective_keyboard_backlight(&state).level(),
            KeyboardBacklightState::Off.level()
        );
    }

    #[test]
    fn requested_backlight_survives_idle_override() {
        let mut state = sample_state();
        state.backlight = KeyboardBacklightState::High;
        state.is_idle = true;

        assert_eq!(state.backlight.level(), KeyboardBacklightState::High.level());
        assert_eq!(
            KeyboardStateManager::effective_keyboard_backlight(&state).level(),
            KeyboardBacklightState::Off.level()
        );
    }
}
