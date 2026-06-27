use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::path::PathBuf;
use std::time::Duration;

use futures::StreamExt;
use log::{error, info, warn};
use tokio::sync::{Mutex, RwLock, broadcast, mpsc};
use zbus::fdo::{self, DBusProxy};
use zbus::message::{Header, Type};
use zbus::{MatchRule, MessageStream, connection, proxy, Connection};

use crate::config::{BatteryUiConfig, Config};
use crate::idle_detection::ActivityNotifier;
use crate::polkit;
use crate::config::{
    tablet_mode_from_str, tablet_mode_to_str, TabletMapMode, TabletMappingConfig,
};
use crate::state::{KeyboardBacklightState, KeyboardStateManager};

pub const ROOT_DBUS_SERVICE: &str = "asus.zenbook.duo";
pub const ROOT_DBUS_PATH: &str = "/asus/zenbook/duo/State";
/// Budget for apply attempts in a rolling window (see decay below). Higher than the session
/// recovery retry count so a full recovery pass is not cut off by decay, while still
/// bounding bursts that destabilize KMS.
const DISPLAY_APPLY_GUARD_MAX_ATTEMPTS: u32 = 15;
const DISPLAY_APPLY_GUARD_DECAY_SECS: u64 = 5;
const SESSION_HEALTH_STALE_USEC: u64 = 30_000_000;

#[derive(Default)]
struct DisplayApplyGuard {
    attempts: u32,
    paused: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
enum AckEvent {
    KeyboardPogoDocked(bool),
    DesiredSecondaryApplied(bool),
}

#[derive(Clone, Debug)]
struct RegisteredSession {
    session_id: u64,
    owner: String,
    /// Updated on each session-originated D-Bus call (microseconds since Unix epoch).
    last_seen_usec: u64,
}

fn session_now_usec() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_micros() as u64)
        .unwrap_or(0)
}

#[derive(Clone)]
struct RootDbusHandle {
    connection: Connection,
    ack_tx: broadcast::Sender<AckEvent>,
    registration: Arc<RwLock<Option<RegisteredSession>>>,
    registered: Arc<AtomicBool>,
}

static ROOT_DBUS_HANDLE: OnceLock<Mutex<Option<RootDbusHandle>>> = OnceLock::new();

/// System bus connection and session id for `register_session` / `register_display_apply_attempt`.
/// Reusing one connection is required so the message sender matches the registered session owner.
/// The `u64` is the same `session_id` passed to `register_session` for self-heal retries.
type SessionSystemBusRegistration = (Connection, u64);
static SESSION_REGISTERED_SYSTEM_BUS: OnceLock<Mutex<Option<SessionSystemBusRegistration>>> =
    OnceLock::new();

fn session_registered_system_bus_cell() -> &'static Mutex<Option<SessionSystemBusRegistration>> {
    SESSION_REGISTERED_SYSTEM_BUS.get_or_init(|| Mutex::new(None))
}

async fn replace_session_registered_system_bus(reg: Option<SessionSystemBusRegistration>) {
    *session_registered_system_bus_cell().lock().await = reg;
}

fn dbus_handle_cell() -> &'static Mutex<Option<RootDbusHandle>> {
    ROOT_DBUS_HANDLE.get_or_init(|| Mutex::new(None))
}

async fn get_root_handle() -> Option<RootDbusHandle> {
    dbus_handle_cell().lock().await.clone()
}

async fn dbus_caller_pid(connection: &Connection, sender: &str) -> fdo::Result<u32> {
    let dbus_proxy = DBusProxy::new(connection)
        .await
        .map_err(|e| fdo::Error::Failed(format!("failed to query D-Bus for caller PID: {e}")))?;
    let bus_name = zbus::names::BusName::try_from(sender)
        .map_err(|e| fdo::Error::Failed(format!("invalid sender bus name: {e}")))?;
    let pid = dbus_proxy
        .get_connection_unix_process_id(bus_name)
        .await
        .map_err(|e| fdo::Error::Failed(format!("failed to get caller PID: {e}")))?;
    Ok(pid)
}

struct RootStateInterface {
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
    config_path: PathBuf,
    battery_ui_config: Arc<RwLock<BatteryUiConfig>>,
    registration: Arc<RwLock<Option<RegisteredSession>>>,
    registered: Arc<AtomicBool>,
    ack_tx: broadcast::Sender<AckEvent>,
    apply_guard: Arc<Mutex<DisplayApplyGuard>>,
}

impl RootStateInterface {
    fn new(
        state_manager: KeyboardStateManager,
        activity_notifier: ActivityNotifier,
        config_path: PathBuf,
        battery_ui_config: Arc<RwLock<BatteryUiConfig>>,
        registration: Arc<RwLock<Option<RegisteredSession>>>,
        registered: Arc<AtomicBool>,
        ack_tx: broadcast::Sender<AckEvent>,
        apply_guard: Arc<Mutex<DisplayApplyGuard>>,
    ) -> Self {
        Self {
            state_manager,
            activity_notifier,
            config_path,
            battery_ui_config,
            registration,
            registered,
            ack_tx,
            apply_guard,
        }
    }

    async fn ensure_registered_sender(&self, header: &Header<'_>) -> fdo::Result<()> {
        let Some(sender) = header.sender() else {
            return Err(fdo::Error::AccessDenied("missing D-Bus sender".into()));
        };

        let registration = self.registration.read().await;
        match registration.as_ref() {
            Some(session) if session.owner == sender.as_str() => Ok(()),
            Some(session) => Err(fdo::Error::AccessDenied(format!(
                "sender {} does not match registered session {}",
                sender.as_str(),
                session.owner
            ))),
            None => Err(fdo::Error::AccessDenied(
                "no session daemon is currently registered".into(),
            )),
        }
    }

    async fn mark_session_activity(&self) {
        let now = session_now_usec();
        let mut registration = self.registration.write().await;
        if let Some(session) = registration.as_mut() {
            session.last_seen_usec = now;
        }
    }

    /// Operators (CLI / scripts): Polkit action [`polkit::ACTION_OPERATOR`] (active local session or root per system rules).
    async fn require_operator(
        &self,
        header: &Header<'_>,
        connection: &Connection,
    ) -> fdo::Result<()> {
        let Some(sender) = header.sender() else {
            return Err(fdo::Error::AccessDenied("missing D-Bus sender".into()));
        };
        let pid = dbus_caller_pid(connection, sender.as_str()).await?;
        polkit::check_authorization(connection, pid, polkit::ACTION_OPERATOR).await
    }

    async fn persist_battery_ui_config(&self, config: BatteryUiConfig) -> fdo::Result<()> {
        config.validate().map_err(fdo::Error::Failed)?;
        let mut file_config = Config::read(&self.config_path).await;
        file_config.battery_ui = config.clone();
        file_config.write(&self.config_path).await.map_err(fdo::Error::Failed)?;
        *self.battery_ui_config.write().await = config;
        notify_battery_ui_config_changed()
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    async fn persist_ambient_keyboard_backlight_enabled(&self, enabled: bool) -> fdo::Result<()> {
        let mut file_config = Config::read(&self.config_path).await;
        file_config.ambient_keyboard_backlight_enabled = enabled;
        file_config.write(&self.config_path).await.map_err(fdo::Error::Failed)?;
        self.state_manager
            .set_ambient_keyboard_backlight_enabled(enabled)
            .await;
        notify_ambient_keyboard_backlight_changed()
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }

    async fn persist_idle_timeout_seconds(&self, seconds: u64) -> fdo::Result<()> {
        let mut file_config = Config::read(&self.config_path).await;
        file_config.idle_timeout_seconds = seconds;
        file_config.write(&self.config_path).await.map_err(fdo::Error::Failed)?;
        self.state_manager.set_idle_timeout_seconds(seconds).await;
        self.activity_notifier.set_idle_timeout_seconds(seconds);
        notify_idle_timeout_changed()
            .await
            .map_err(|e| fdo::Error::Failed(e.to_string()))?;
        Ok(())
    }
}

/// One-shot commands forwarded to the running root daemon (see CLI `control` / legacy shell scripts).
#[derive(Debug, Clone)]
pub enum OperatorCommand {
    ToggleMicMute,
    SetMicMute(bool),
    ToggleKeyboardBacklight,
    /// 0 = off, 1 = low, 2 = medium, 3 = high
    SetKeyboardBacklightLevel(u8),
    SetDesiredPrimary(String),
    SetTabletMappingEnabled(bool),
    SetTabletMappingMode(String),
    ToggleTabletMappingMode,
    ApplyTabletMapping,
    ToggleSecondaryDisplay,
    SetSecondaryDisplay(bool),
}

#[zbus::interface(name = "asus.zenbook.duo.State")]
impl RootStateInterface {
    #[zbus(property)]
    fn keyboard_usb_connected(&self) -> bool {
        self.state_manager.is_usb_keyboard_connected()
    }

    #[zbus(property)]
    fn keyboard_pogo_docked(&self) -> bool {
        self.state_manager.is_keyboard_pogo_docked()
    }

    /// Backward-compatible alias: **pogo dock on bottom panel**, not side USB charge.
    #[zbus(property)]
    fn keyboard_attached(&self) -> bool {
        self.state_manager.is_keyboard_pogo_docked()
    }

    #[zbus(property)]
    fn bluetooth_connected(&self) -> bool {
        self.state_manager.is_bluetooth_connected()
    }

    /// Debounced effective connection state used by UI and status clients.
    #[zbus(property)]
    fn keyboard_connected(&self) -> bool {
        self.state_manager.is_keyboard_connected()
    }

    /// Debounced effective connection kind: `pogo`, `usb`, `bt`, or `detached`.
    #[zbus(property)]
    fn keyboard_connection_type(&self) -> String {
        self.state_manager.keyboard_connection_kind().as_str().to_string()
    }

    /// `true` when battery level is known (USB vendor report or BlueZ `Battery1`).
    #[zbus(property)]
    fn keyboard_battery_present(&self) -> bool {
        self.state_manager.keyboard_battery_percentage().is_some()
    }

    /// 0–100 when [`Self::keyboard_battery_present`] is true; otherwise 0.
    #[zbus(property)]
    fn keyboard_battery_percentage(&self) -> u8 {
        self.state_manager
            .keyboard_battery_percentage()
            .unwrap_or(0)
    }

    /// Side-USB charge in progress (from vendor `0x5a 0x3d` status byte); always false on Bluetooth.
    #[zbus(property)]
    fn keyboard_battery_charging(&self) -> bool {
        self.state_manager.is_keyboard_battery_charging()
    }

    /// `true` when any last-known battery percentage is cached in memory (persisted across detach).
    #[zbus(property)]
    fn keyboard_battery_last_known_present(&self) -> bool {
        self.state_manager
            .keyboard_battery_last_known_percentage()
            .is_some()
    }

    /// Last known battery percentage from USB or Bluetooth; 0 when unknown.
    #[zbus(property)]
    fn keyboard_battery_last_known_percentage(&self) -> u8 {
        self.state_manager
            .keyboard_battery_last_known_percentage()
            .unwrap_or(0)
    }

    /// UI-facing charging state: true while live firmware reports charging or the keyboard is on USB/POGO power.
    #[zbus(property)]
    fn keyboard_battery_effective_charging(&self) -> bool {
        self.state_manager.is_keyboard_battery_effective_charging()
    }

    /// Full on USB (firmware flag or SOC ≥ threshold) or on Bluetooth when SOC ≥ threshold.
    #[zbus(property)]
    fn keyboard_battery_full(&self) -> bool {
        self.state_manager.is_keyboard_battery_full()
    }

    /// Microphone mute LED on the keyboard (synced with PulseAudio default source mute when available).
    #[zbus(property)]
    fn mic_mute_led(&self) -> bool {
        self.state_manager.get_mic_mute_led()
    }

    /// Requested keyboard backlight level: 0 = off, 1 = low, 2 = medium, 3 = high.
    #[zbus(property)]
    fn keyboard_backlight_level(&self) -> u8 {
        self.state_manager.requested_keyboard_backlight().level()
    }

    /// Effective keyboard backlight level after idle / suspend / ambient overrides.
    #[zbus(property)]
    fn keyboard_backlight_effective_level(&self) -> u8 {
        self.state_manager.get_keyboard_backlight().level()
    }

    #[zbus(property)]
    async fn battery_ui_config_json(&self) -> String {
        serde_json::to_string(&*self.battery_ui_config.read().await)
            .unwrap_or_else(|_| serde_json::to_string(&BatteryUiConfig::default()).unwrap())
    }

    /// GNOME pen → panel remapping master switch (see `src/session/tablet_mapping.rs`).
    #[zbus(property)]
    fn tablet_mapping_enabled(&self) -> bool {
        self.state_manager.get_tablet_mapping_config().enable
    }

    /// `one_to_one` or `all_to_primary`.
    #[zbus(property)]
    fn tablet_mapping_mode(&self) -> String {
        self.state_manager.tablet_mapping_mode_string()
    }

    /// Bumped by [`Self::apply_tablet_mapping`] so the session re-runs gsettings remap.
    #[zbus(property)]
    fn tablet_mapping_apply_nonce(&self) -> u32 {
        self.state_manager.tablet_mapping_apply_nonce()
    }

    #[zbus(property)]
    fn desired_primary(&self) -> String {
        self.state_manager
            .get_desired_primary()
            .unwrap_or_else(|| "eDP-1".to_string())
    }

    #[zbus(property)]
    fn desired_secondary_enabled(&self) -> bool {
        self.state_manager.is_secondary_display_desired_enabled()
    }

    #[zbus(property)]
    fn ambient_light_enabled(&self) -> bool {
        self.state_manager.get_ambient_light_enabled()
    }

    #[zbus(property)]
    fn ambient_light_available(&self) -> bool {
        self.state_manager.ambient_light_available()
    }

    #[zbus(property)]
    fn ambient_keyboard_backlight_enabled(&self) -> bool {
        self.state_manager.ambient_keyboard_backlight_enabled()
    }

    #[zbus(property)]
    fn idle_timeout_seconds(&self) -> u32 {
        self.state_manager.idle_timeout_seconds().min(u64::from(u32::MAX)) as u32
    }

    #[zbus(property)]
    fn desired_display_attachment(&self) -> String {
        self.state_manager.get_desired_display_mode().0
    }

    #[zbus(property)]
    fn desired_display_layout(&self) -> String {
        self.state_manager.get_desired_display_mode().1
    }

    #[zbus(property)]
    fn display_rotation(&self) -> String {
        self.state_manager.display_rotation()
    }

    async fn report_observed_display_mode(
        &self,
        attachment: String,
        layout: String,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        self.mark_session_activity().await;
        if let Err(e) =
            crate::state::validate_desired_display_mode_strings(&attachment, &layout)
        {
            return Err(fdo::Error::Failed(e));
        }
        self.state_manager
            .persist_desired_display_mode(&attachment, &layout)
            .await;
        info!(
            "ROOT DBus: persisted observed display mode attachment={attachment} layout={layout}"
        );
        if let Err(e) = emit_desired_display_mode_changed().await {
            warn!("report_observed_display_mode: failed to emit property change: {e}");
        }
        Ok(())
    }

    async fn report_display_rotation(
        &self,
        rotation: String,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        self.mark_session_activity().await;
        let normalized = match rotation.as_str() {
            "normal" | "left-up" | "right-up" | "inverted" => rotation,
            "bottom-up" => "inverted".to_string(),
            other => {
                return Err(fdo::Error::Failed(format!(
                    "display rotation must be normal, left-up, right-up, or inverted; got {other}"
                )));
            }
        };
        self.state_manager.set_display_rotation(normalized);
        Ok(())
    }

    /// Persist the session's current display layout (serialized monitors.xml) to the
    /// system-wide and GDM-greeter locations, so the login screen and pre-session boot
    /// come up with the user's layout (no flicker). The session daemon writes its own
    /// ~/.config/monitors.xml; this privileged path covers the root-owned targets.
    async fn persist_display_layout_xml(
        &self,
        xml: String,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        self.mark_session_activity().await;
        write_gdm_monitors_xml(&xml)
            .await
            .map_err(fdo::Error::Failed)?;
        Ok(())
    }

    #[zbus(property)]
    fn display_brightness(&self) -> u32 {
        self.state_manager.get_display_brightness_value().unwrap_or(0)
    }

    #[zbus(property)]
    async fn display_apply_paused(&self) -> bool {
        self.apply_guard.lock().await.paused
    }

    /// `true` while a `zenbook-duo-session` peer has called `RegisterSession` and remains on the bus.
    #[zbus(property)]
    fn session_registered(&self) -> bool {
        self.registered.load(Ordering::SeqCst)
    }

    /// Unique bus name of the registered session (empty when none).
    #[zbus(property)]
    async fn session_owner(&self) -> String {
        self.registration
            .read()
            .await
            .as_ref()
            .map(|s| s.owner.clone())
            .unwrap_or_default()
    }

    /// Last accepted `register_session` id (`0` when none).
    #[zbus(property)]
    async fn session_id(&self) -> u64 {
        self.registration
            .read()
            .await
            .as_ref()
            .map(|s| s.session_id)
            .unwrap_or(0)
    }

        /// Last successful session heartbeat / RPC timestamp (µs since epoch); read in UI for
        /// liveness checks.
        #[zbus(property)]
        async fn session_last_seen_usec(&self) -> u64 {
        self.registration
            .read()
            .await
            .as_ref()
            .map(|s| s.last_seen_usec)
            .unwrap_or(0)
    }

    #[zbus(property)]
    async fn session_quiet(&self) -> bool {
        let Some(session) = self.registration.read().await.as_ref().cloned() else {
            return false;
        };

        let now = session_now_usec();
        now.saturating_sub(session.last_seen_usec) > SESSION_HEALTH_STALE_USEC
    }

    async fn ping_session(&self, #[zbus(header)] header: Header<'_>) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        self.mark_session_activity().await;
        Ok(())
    }

    async fn register_session(
        &self,
        session_id: u64,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        let Some(sender) = header.sender() else {
            return Err(fdo::Error::Failed(
                "session registration is missing sender information".into(),
            ));
        };

        let pid = dbus_caller_pid(connection, sender.as_str()).await?;
        polkit::check_authorization(connection, pid, polkit::ACTION_REGISTER_SESSION).await?;

        let replacing_existing_session = self.registration.read().await.is_some();
        if self.state_manager.is_secondary_display_enabled() && !replacing_existing_session {
            crate::secondary_display::arm_sysfs_fallback_once();
            self.state_manager.emit_secondary_display_state();
            if crate::secondary_display::wait_for_secondary_display_state(true, Duration::from_secs(8))
                .await
            {
                info!("ROOT DBus: secondary display reported enabled before session registration sync");
            } else {
                warn!("ROOT DBus: timed out waiting for secondary display enable during session registration");
            }
        } else if self.state_manager.is_secondary_display_enabled() && replacing_existing_session {
            info!(
                "ROOT DBus: replacing existing session registration while secondary is already enabled; skipping pre-registration secondary sync"
            );
        }

        let now = session_now_usec();
        let new_registration = RegisteredSession {
            session_id,
            owner: sender.as_str().to_string(),
            last_seen_usec: now,
        };

        {
            let mut registration = self.registration.write().await;
            if let Some(previous) = registration.as_ref() {
                if previous.owner != new_registration.owner || previous.session_id != session_id {
                    info!(
                        "ROOT DBus: replacing registered session owner={} session_id={} with owner={} session_id={}",
                        previous.owner, previous.session_id, new_registration.owner, session_id
                    );
                }
            } else {
                info!(
                    "ROOT DBus: registered session owner={} session_id={}",
                    new_registration.owner, session_id
                );
            }
            *registration = Some(new_registration);
        }
        self.registered.store(true, Ordering::SeqCst);
        if let Err(e) = emit_session_registration_changed().await {
            warn!("register_session: failed to emit session registration properties: {e}");
        }

        Ok(())
    }

    async fn acknowledge_keyboard_pogo_docked(
        &self,
        value: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        self.mark_session_activity().await;
        let _ = self.ack_tx.send(AckEvent::KeyboardPogoDocked(value));
        Ok(())
    }

    /// Deprecated alias for [`Self::acknowledge_keyboard_pogo_docked`].
    async fn acknowledge_keyboard_attached(
        &self,
        value: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.acknowledge_keyboard_pogo_docked(value, header).await
    }

    async fn acknowledge_desired_secondary_applied(
        &self,
        value: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        self.mark_session_activity().await;
        let _ = self.ack_tx.send(AckEvent::DesiredSecondaryApplied(value));
        Ok(())
    }

    /// Session calls this after a **successful** debounced display reconcile when Mutter's layout
    /// has no logical output on eDP-2 (secondary logically off). Root writes sysfs `off` when
    /// `desired_secondary_enabled` is false, or when it is still true but the USB keyboard is
    /// docked (clamshell power save until undock).
    async fn acknowledge_secondary_sysfs_poweroff_ready(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        self.mark_session_activity().await;
        info!(
            "ROOT [secondary sysfs] Moment B (session-stable D-Bus ack): registered session called acknowledge_secondary_sysfs_poweroff_ready — applying session-stable sysfs policy (see next lines for write / no-op)"
        );
        crate::secondary_display::apply_secondary_sysfs_poweroff_when_desired_off(&self.state_manager)
            .await;
        Ok(())
    }

    async fn report_ambient_light_enabled(
        &self,
        value: bool,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<()> {
        self.ensure_registered_sender(&header).await?;
        self.mark_session_activity().await;
        if self.state_manager.get_ambient_light_enabled() != value {
            info!("ROOT DBus: ambient light setting updated to {}", value);
            self.state_manager.set_ambient_light_enabled(value).await;
        }
        Ok(())
    }

    async fn register_display_apply_attempt(
        &self,
        #[zbus(header)] header: Header<'_>,
    ) -> fdo::Result<bool> {
        self.ensure_registered_sender(&header).await?;
        self.mark_session_activity().await;

        let mut guard = self.apply_guard.lock().await;
        if guard.paused {
            return Ok(false);
        }

        guard.attempts = guard.attempts.saturating_add(1);
        if guard.attempts > DISPLAY_APPLY_GUARD_MAX_ATTEMPTS {
            guard.paused = true;
            warn!(
                "ROOT DBus: pausing display applies (attempt counter {} exceeded max {})",
                guard.attempts, DISPLAY_APPLY_GUARD_MAX_ATTEMPTS
            );
            return Ok(false);
        }
        drop(guard);

        let apply_guard = Arc::clone(&self.apply_guard);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_secs(DISPLAY_APPLY_GUARD_DECAY_SECS)).await;
            let mut guard = apply_guard.lock().await;
            guard.attempts = guard.attempts.saturating_sub(1);
        });

        Ok(true)
    }

    async fn resume_display_applies(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        let Some(sender) = header.sender() else {
            return Err(fdo::Error::AccessDenied("missing D-Bus sender".into()));
        };

        let pid = dbus_caller_pid(connection, sender.as_str()).await?;
        polkit::check_authorization(connection, pid, polkit::ACTION_RESUME_DISPLAY_APPLIES).await?;

        let mut guard = self.apply_guard.lock().await;
        guard.attempts = 0;
        guard.paused = false;
        info!("ROOT DBus: display apply guard resumed (Polkit-authorized caller, pid {pid})");
        Ok(())
    }

    async fn toggle_mic_mute(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        let muted = crate::mute_state::toggle_default_source_mute()
            .map_err(|e| fdo::Error::Failed(format!("toggle microphone mute: {e}")))?;
        self.state_manager.set_mic_mute_led(muted);
        Ok(())
    }

    async fn set_mic_mute(
        &self,
        enabled: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        crate::mute_state::set_default_source_mute(enabled)
            .map_err(|e| fdo::Error::Failed(format!("set microphone mute: {e}")))?;
        self.state_manager.set_mic_mute_led(enabled);
        Ok(())
    }

    async fn toggle_keyboard_backlight(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        self.state_manager.toggle_keyboard_backlight().await;
        self.activity_notifier.notify();
        Ok(())
    }

    async fn set_keyboard_backlight_level(
        &self,
        level: u8,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        let state = match level {
            0 => KeyboardBacklightState::Off,
            1 => KeyboardBacklightState::Low,
            2 => KeyboardBacklightState::Medium,
            3 => KeyboardBacklightState::High,
            _ => {
                return Err(fdo::Error::Failed(format!(
                    "keyboard backlight level must be 0..=3, got {level}"
                )));
            }
        };
        self.state_manager.set_keyboard_backlight(state).await;
        self.activity_notifier.notify();
        Ok(())
    }

    async fn set_ambient_keyboard_backlight_enabled(
        &self,
        enabled: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        self.persist_ambient_keyboard_backlight_enabled(enabled).await?;
        info!("Operator: ambient_keyboard_backlight_enabled={enabled}");
        Ok(())
    }

    async fn set_idle_timeout_seconds(
        &self,
        seconds: u32,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        self.persist_idle_timeout_seconds(u64::from(seconds)).await?;
        info!("Operator: idle_timeout_seconds={seconds}");
        Ok(())
    }

    async fn toggle_secondary_display(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        crate::secondary_coordinator::coordinate_secondary_display_toggle(&self.state_manager).await;
        Ok(())
    }

    async fn set_tablet_mapping_enabled(
        &self,
        enabled: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        self.state_manager
            .set_tablet_mapping_enabled(enabled)
            .await;
        info!("Operator: tablet_mapping_enabled={enabled}");
        Ok(())
    }

    async fn set_battery_ui_config_json(
        &self,
        config_json: String,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        let parsed: BatteryUiConfig =
            serde_json::from_str(&config_json).map_err(|e| fdo::Error::Failed(e.to_string()))?;
        self.persist_battery_ui_config(parsed).await?;
        info!("Operator: updated battery_ui settings");
        Ok(())
    }

    async fn set_tablet_mapping_mode(
        &self,
        mode: String,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        let parsed = tablet_mode_from_str(&mode).map_err(fdo::Error::Failed)?;
        self.state_manager.set_tablet_mapping_mode(parsed).await;
        info!("Operator: tablet_mapping_mode={mode}");
        Ok(())
    }

    async fn toggle_tablet_mapping_mode(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        match self.state_manager.toggle_tablet_mapping_mode().await {
            Some(mode) => {
                info!(
                    "Operator: tablet_mapping_mode toggled to {}",
                    crate::config::tablet_mode_to_str(mode)
                );
            }
            None => {
                info!("Operator: tablet_mapping_mode toggle ignored (mapping disabled)");
            }
        }
        Ok(())
    }

    async fn apply_tablet_mapping(
        &self,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        let nonce = self.state_manager.bump_tablet_mapping_apply_nonce().await;
        info!("Operator: apply_tablet_mapping nonce={nonce}");
        Ok(())
    }

    async fn set_desired_primary(
        &self,
        primary: String,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        if let Err(e) = crate::state::validate_desired_primary(&primary) {
            return Err(fdo::Error::Failed(e));
        }
        if self.state_manager.get_desired_primary().as_deref() == Some(primary.as_str()) {
            return Ok(());
        }
        crate::secondary_display::pause_brightness_sync_for(std::time::Duration::from_secs(4));
        self.state_manager.set_desired_primary(&primary).await;
        info!("Operator: desired_primary set to {primary}");
        match notify_desired_primary_changed().await {
            Ok(true) => {
                info!("Published desired_primary={primary} (session registered)");
            }
            Ok(false) => {
                warn!(
                    "Persisted desired_primary={primary}; no session registered yet — apply when GNOME session connects"
                );
            }
            Err(e) => {
                warn!("Failed to publish desired_primary update: {e}");
            }
        }
        Ok(())
    }

    async fn set_secondary_display_desired(
        &self,
        enabled: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.require_operator(&header, connection).await?;
        crate::secondary_coordinator::coordinate_secondary_display_to_state(&self.state_manager, enabled)
            .await;
        Ok(())
    }

    /// Same as [`Self::set_secondary_display_desired`]; name matches D-Bus property `DesiredSecondaryEnabled`.
    async fn set_desired_secondary_enabled(
        &self,
        enabled: bool,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] connection: &Connection,
    ) -> fdo::Result<()> {
        self.set_secondary_display_desired(enabled, header, connection)
            .await
    }
}

#[proxy(
    interface = "asus.zenbook.duo.State",
    default_service = "asus.zenbook.duo",
    default_path = "/asus/zenbook/duo/State"
)]
trait RootState {
    fn register_session(&self, session_id: u64) -> zbus::Result<()>;
    fn ping_session(&self) -> zbus::Result<()>;
    fn acknowledge_keyboard_pogo_docked(&self, value: bool) -> zbus::Result<()>;
    fn acknowledge_keyboard_attached(&self, value: bool) -> zbus::Result<()>;
    fn acknowledge_desired_secondary_applied(&self, value: bool) -> zbus::Result<()>;
    fn acknowledge_secondary_sysfs_poweroff_ready(&self) -> zbus::Result<()>;
    fn report_ambient_light_enabled(&self, value: bool) -> zbus::Result<()>;
    fn report_observed_display_mode(&self, attachment: String, layout: String) -> zbus::Result<()>;
    fn report_display_rotation(&self, rotation: String) -> zbus::Result<()>;
    fn persist_display_layout_xml(&self, xml: String) -> zbus::Result<()>;
    fn register_display_apply_attempt(&self) -> zbus::Result<bool>;
    fn resume_display_applies(&self) -> zbus::Result<()>;
    fn toggle_mic_mute(&self) -> zbus::Result<()>;
    fn set_mic_mute(&self, enabled: bool) -> zbus::Result<()>;
    fn toggle_keyboard_backlight(&self) -> zbus::Result<()>;
    fn set_keyboard_backlight_level(&self, level: u8) -> zbus::Result<()>;
    fn set_ambient_keyboard_backlight_enabled(&self, enabled: bool) -> zbus::Result<()>;
    fn set_idle_timeout_seconds(&self, seconds: u32) -> zbus::Result<()>;
    fn set_desired_primary(&self, primary: String) -> zbus::Result<()>;
    fn set_battery_ui_config_json(&self, config_json: String) -> zbus::Result<()>;
    fn set_tablet_mapping_enabled(&self, enabled: bool) -> zbus::Result<()>;
    fn set_tablet_mapping_mode(&self, mode: String) -> zbus::Result<()>;
    fn toggle_tablet_mapping_mode(&self) -> zbus::Result<()>;
    fn apply_tablet_mapping(&self) -> zbus::Result<()>;
    fn toggle_secondary_display(&self) -> zbus::Result<()>;
    fn set_secondary_display_desired(&self, enabled: bool) -> zbus::Result<()>;
    fn set_desired_secondary_enabled(&self, enabled: bool) -> zbus::Result<()>;

    #[zbus(property)]
    fn keyboard_usb_connected(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_pogo_docked(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_attached(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn bluetooth_connected(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_connected(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_connection_type(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn keyboard_battery_present(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_battery_percentage(&self) -> zbus::Result<u8>;

    #[zbus(property)]
    fn keyboard_battery_charging(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_battery_full(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn mic_mute_led(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn keyboard_backlight_level(&self) -> zbus::Result<u8>;

    #[zbus(property)]
    fn keyboard_backlight_effective_level(&self) -> zbus::Result<u8>;

    #[zbus(property)]
    fn battery_ui_config_json(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn tablet_mapping_enabled(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn tablet_mapping_mode(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn tablet_mapping_apply_nonce(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn desired_primary(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn desired_secondary_enabled(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn ambient_light_enabled(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn ambient_light_available(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn ambient_keyboard_backlight_enabled(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn idle_timeout_seconds(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn desired_display_attachment(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn desired_display_layout(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn display_rotation(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn display_brightness(&self) -> zbus::Result<u32>;

    #[zbus(property)]
    fn display_apply_paused(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn session_registered(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn session_owner(&self) -> zbus::Result<String>;

    #[zbus(property)]
    fn session_id(&self) -> zbus::Result<u64>;

    #[zbus(property)]
    fn session_last_seen_usec(&self) -> zbus::Result<u64>;

    #[zbus(property)]
    fn session_quiet(&self) -> zbus::Result<bool>;
}

async fn emit_keyboard_usb_connected_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .keyboard_usb_connected_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn emit_keyboard_pogo_docked_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .keyboard_pogo_docked_changed(iface_ref.signal_emitter())
        .await?;
    iface_ref
        .get()
        .await
        .keyboard_attached_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_keyboard_usb_connected_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_keyboard_usb_connected_changed().await
}

pub async fn notify_keyboard_pogo_docked_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_keyboard_pogo_docked_changed().await
}

async fn emit_keyboard_connection_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    let iface = iface_ref.get().await;
    let emitter = iface_ref.signal_emitter();
    iface.keyboard_connected_changed(&emitter).await?;
    iface.keyboard_connection_type_changed(&emitter).await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_keyboard_connection_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_keyboard_connection_changed().await
}

async fn emit_display_rotation_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .display_rotation_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_display_rotation_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_display_rotation_changed().await
}

async fn emit_keyboard_battery_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    let iface = iface_ref.get().await;
    let emitter = iface_ref.signal_emitter();
    iface
        .keyboard_battery_present_changed(&emitter)
        .await?;
    iface
        .keyboard_battery_percentage_changed(&emitter)
        .await?;
    iface
        .keyboard_battery_charging_changed(&emitter)
        .await?;
    iface
        .keyboard_battery_last_known_present_changed(&emitter)
        .await?;
    iface
        .keyboard_battery_last_known_percentage_changed(&emitter)
        .await?;
    iface
        .keyboard_battery_effective_charging_changed(&emitter)
        .await?;
    iface.keyboard_battery_full_changed(&emitter).await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_keyboard_battery_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_keyboard_battery_changed().await
}

async fn emit_mic_mute_led_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .mic_mute_led_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_mic_mute_led_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_mic_mute_led_changed().await
}

async fn emit_keyboard_backlight_changed(
    requested_changed: bool,
    effective_changed: bool,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    if !requested_changed && !effective_changed {
        return Ok(false);
    }
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    let iface = iface_ref.get().await;
    let emitter = iface_ref.signal_emitter();
    if requested_changed {
        iface.keyboard_backlight_level_changed(&emitter).await?;
    }
    if effective_changed {
        iface.keyboard_backlight_effective_level_changed(&emitter).await?;
    }

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_keyboard_backlight_changes(
    requested_changed: bool,
    effective_changed: bool,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_keyboard_backlight_changed(requested_changed, effective_changed).await
}

async fn emit_battery_ui_config_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .battery_ui_config_json_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_battery_ui_config_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_battery_ui_config_changed().await
}

async fn emit_ambient_keyboard_backlight_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    let iface = iface_ref.get().await;
    let emitter = iface_ref.signal_emitter();
    iface.ambient_light_available_changed(&emitter).await?;
    iface
        .ambient_keyboard_backlight_enabled_changed(&emitter)
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_ambient_keyboard_backlight_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_ambient_keyboard_backlight_changed().await
}

async fn emit_idle_timeout_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .idle_timeout_seconds_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_idle_timeout_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_idle_timeout_changed().await
}

async fn emit_tablet_mapping_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    let iface = iface_ref.get().await;
    let emitter = iface_ref.signal_emitter();
    iface
        .tablet_mapping_enabled_changed(&emitter)
        .await?;
    iface.tablet_mapping_mode_changed(&emitter).await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_tablet_mapping_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_tablet_mapping_changed().await
}

async fn emit_tablet_mapping_apply_nonce_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .tablet_mapping_apply_nonce_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

pub async fn notify_tablet_mapping_apply_nonce_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_tablet_mapping_apply_nonce_changed().await
}

async fn emit_bluetooth_connected_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .bluetooth_connected_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn emit_desired_display_mode_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .desired_display_attachment_changed(iface_ref.signal_emitter())
        .await?;
    iface_ref
        .get()
        .await
        .desired_display_layout_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn emit_desired_primary_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .desired_primary_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn emit_desired_secondary_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .desired_secondary_enabled_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn emit_display_brightness_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    iface_ref
        .get()
        .await
        .display_brightness_changed(iface_ref.signal_emitter())
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn emit_session_registration_changed() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let Some(handle) = get_root_handle().await else {
        return Ok(false);
    };

    let iface_ref = handle
        .connection
        .object_server()
        .interface::<_, RootStateInterface>(ROOT_DBUS_PATH)
        .await?;
    let iface = iface_ref.get().await;
    let emitter = iface_ref.signal_emitter();
    iface.session_registered_changed(&emitter).await?;
    iface.session_owner_changed(&emitter).await?;
    iface.session_id_changed(&emitter).await?;
    iface
        .session_last_seen_usec_changed(&emitter)
        .await?;

    Ok(handle.registered.load(Ordering::SeqCst))
}

async fn clear_registration_for_owner(owner: &str) {
    let Some(handle) = get_root_handle().await else {
        return;
    };

    let cleared = {
        let mut registration = handle.registration.write().await;
        if registration
            .as_ref()
            .is_some_and(|registered| registered.owner == owner)
        {
            info!("ROOT DBus: registered session owner {} disappeared", owner);
            *registration = None;
            handle.registered.store(false, Ordering::SeqCst);
            true
        } else {
            false
        }
    };
    if cleared {
        if let Err(e) = emit_session_registration_changed().await {
            warn!(
                "clear_registration_for_owner: failed to emit session registration properties: {e}"
            );
        }
    }
}

async fn watch_registered_session(connection: Connection) {
    let proxy = match DBusProxy::new(&connection).await {
        Ok(proxy) => proxy,
        Err(e) => {
            error!("ROOT DBus: failed to create org.freedesktop.DBus proxy: {e}");
            return;
        }
    };

    let mut changes = match proxy.receive_name_owner_changed().await {
        Ok(changes) => changes,
        Err(e) => {
            error!("ROOT DBus: failed to subscribe to NameOwnerChanged: {e}");
            return;
        }
    };

    while let Some(signal) = changes.next().await {
        match signal.args() {
            Ok(args) if args.new_owner().is_none() => {
                clear_registration_for_owner(args.name().as_str()).await;
            }
            Ok(_) => {}
            Err(e) => warn!("ROOT DBus: failed to parse NameOwnerChanged args: {e}"),
        }
    }
}

async fn watch_prepare_for_sleep(
    config: Config,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
) {
    const RULE: &str = "type='signal',interface='org.freedesktop.login1.Manager',member='PrepareForSleep',path='/org/freedesktop/login1'";
    let rule = match MatchRule::try_from(RULE) {
        Ok(r) => r,
        Err(e) => {
            warn!("logind PrepareForSleep: invalid match rule: {e}");
            return;
        }
    };

    let connection = match Connection::system().await {
        Ok(c) => c,
        Err(e) => {
            warn!("logind PrepareForSleep: system bus failed: {e}");
            return;
        }
    };

    let mut stream = match MessageStream::for_match_rule(rule, &connection, None).await {
        Ok(s) => s,
        Err(e) => {
            warn!("logind PrepareForSleep: subscribe failed: {e}");
            return;
        }
    };

    info!("logind: subscribed to PrepareForSleep");
    while let Some(item) = stream.next().await {
        let msg = match item {
            Ok(m) => m,
            Err(e) => {
                warn!("logind PrepareForSleep stream: {e}");
                continue;
            }
        };
        if msg.header().message_type() != Type::Signal {
            continue;
        }

        let start: bool = match msg.body().deserialize_unchecked() {
            Ok(v) => v,
            Err(e) => {
                warn!("logind PrepareForSleep: bad signal body: {e}");
                continue;
            }
        };

        if start {
            info!("PrepareForSleep(start=true): suspending");
            state_manager.suspend_start();
        } else {
            info!("PrepareForSleep(start=false): resumed");
            state_manager.suspend_end();
            activity_notifier.notify();

            match crate::keyboard_usb::find_wired_keyboard(&config).await {
                Some(keyboard) => {
                    let pogo = crate::usb_keyboard_ports::keyboard_on_pogo_dock_async(
                        keyboard.bus_id(),
                        keyboard.device_address(),
                    )
                    .await;
                    let usb = state_manager.is_usb_keyboard_connected();
                    let docked = state_manager.is_keyboard_pogo_docked();
                    if !usb || pogo != docked {
                        info!(
                            "Post-resume keyboard state mismatch: usb_connected=true pogo_docked={pogo} (was usb={usb} pogo={docked}) — updating"
                        );
                        state_manager.set_usb_keyboard_connection(true, pogo);
                    }
                }
                None => {
                    if state_manager.is_usb_keyboard_connected()
                        || state_manager.is_keyboard_pogo_docked()
                    {
                        info!("Post-resume: no USB keyboard enumerated — clearing connection state");
                        state_manager.set_usb_keyboard_connection(false, false);
                    }
                }
            }
        }
    }

    warn!("logind PrepareForSleep stream ended");
}

pub async fn start_root_service(
    state_manager: KeyboardStateManager,
    config_path: PathBuf,
    config: Config,
    activity_notifier: ActivityNotifier,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let registration = Arc::new(RwLock::new(None));
    let registered = Arc::new(AtomicBool::new(false));
    let (ack_tx, _) = broadcast::channel(16);
    let apply_guard = Arc::new(Mutex::new(DisplayApplyGuard::default()));
    let battery_ui_config = Arc::new(RwLock::new(config.battery_ui.clone()));

    let connection = connection::Builder::system()?
        .name(ROOT_DBUS_SERVICE)?
        .serve_at(
            ROOT_DBUS_PATH,
            RootStateInterface::new(
                state_manager.clone(),
                activity_notifier.clone(),
                config_path,
                battery_ui_config,
                registration.clone(),
                registered.clone(),
                ack_tx.clone(),
                apply_guard,
            ),
        )?
        .build()
        .await?;

    {
        let mut slot = dbus_handle_cell().lock().await;
        *slot = Some(RootDbusHandle {
            connection: connection.clone(),
            ack_tx,
            registration,
            registered,
        });
    }

    tokio::spawn(watch_registered_session(connection.clone()));
    let cfg = config.clone();
    let sm = state_manager.clone();
    let an = activity_notifier.clone();
    tokio::spawn(async move {
        watch_prepare_for_sleep(cfg, sm, an).await;
    });
    info!("ROOT DBus: exported {}", ROOT_DBUS_SERVICE);
    Ok(())
}

pub fn is_session_registered() -> bool {
    ROOT_DBUS_HANDLE
        .get()
        .and_then(|cell| cell.try_lock().ok())
        .and_then(|guard| guard.as_ref().cloned())
        .is_some_and(|handle| handle.registered.load(Ordering::SeqCst))
}

async fn recv_matching_ack(
    rx: &mut broadcast::Receiver<AckEvent>,
    expected: AckEvent,
    timeout: Duration,
) -> bool {
    tokio::time::timeout(timeout, async {
        loop {
            match rx.recv().await {
                Ok(ack) if ack == expected => return true,
                Ok(_) => continue,
                Err(_) => return false,
            }
        }
    })
    .await
    .unwrap_or(false)
}

/// Subscribe to the root ACK broadcast **before** emitting `keyboard_pogo_docked` so a fast session
/// `acknowledge_keyboard_pogo_docked` is not missed (late `broadcast` subscribers do not see past sends).
///
/// Returns `(registered_with_root, ack_received)`.
pub async fn notify_keyboard_pogo_docked_changed_wait(
    docked: bool,
    timeout: Duration,
) -> (bool, bool) {
    let mut rx = match get_root_handle().await {
        Some(h) => h.ack_tx.subscribe(),
        None => return (false, false),
    };
    let registered = match emit_keyboard_pogo_docked_changed().await {
        Ok(v) => v,
        Err(e) => {
            warn!("KeyboardPogoDocked: emit keyboard_pogo_docked_changed failed: {e}");
            false
        }
    };
    if !registered {
        return (false, false);
    }
    let acked = recv_matching_ack(&mut rx, AckEvent::KeyboardPogoDocked(docked), timeout).await;
    (true, acked)
}

/// Same ordering guarantee as [`notify_keyboard_attached_changed_wait`] for secondary display.
///
/// Returns `(registered_with_root, ack_received)`.
pub async fn notify_desired_secondary_changed_wait(
    enabled: bool,
    timeout: Duration,
) -> (bool, bool) {
    let mut rx = match get_root_handle().await {
        Some(h) => h.ack_tx.subscribe(),
        None => return (false, false),
    };
    let registered = match emit_desired_secondary_changed().await {
        Ok(v) => v,
        Err(e) => {
            warn!("SecondaryDisplay: emit desired_secondary_enabled_changed failed: {e}");
            false
        }
    };
    if !registered {
        return (false, false);
    }
    let acked =
        recv_matching_ack(&mut rx, AckEvent::DesiredSecondaryApplied(enabled), timeout).await;
    (true, acked)
}

pub async fn notify_bluetooth_connected_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_bluetooth_connected_changed().await
}

pub async fn notify_desired_primary_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_desired_primary_changed().await
}

pub async fn notify_desired_secondary_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_desired_secondary_changed().await
}

/// Tell the root daemon that Mutter's layout is stable with no logical output on eDP-2 (session
/// must be registered on the system bus connection used for display applies). Root may sysfs-off
/// when `desired_secondary_enabled` is false, or when it remains true but the USB keyboard is
/// docked (clamshell power save).
pub async fn notify_secondary_sysfs_poweroff_ready() -> Result<(), String> {
    let connection = {
        let guard = session_registered_system_bus_cell().lock().await;
        guard
            .as_ref()
            .map(|(c, _)| c.clone())
            .ok_or_else(|| {
                "session system bus not registered (cannot reach root for secondary sysfs poweroff ready)"
                    .to_string()
            })?
    };
    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("root state proxy: {e}"))?;
    proxy
        .acknowledge_secondary_sysfs_poweroff_ready()
        .await
        .map_err(|e| format!("acknowledge_secondary_sysfs_poweroff_ready: {e}"))
}

pub async fn notify_display_brightness_changed(
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    emit_display_brightness_changed().await
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DisplayApplyPermit {
    /// Root cleared this apply; session may call Mutter `apply_monitors_config`.
    Allowed,
    /// Root guard is paused or over budget; session must not apply.
    Paused,
}

/// Ask the root daemon for permission before each `apply_monitors_config`.
/// Fail-closed: if the system bus or method call fails, returns `Err` and the session must not apply.
///
/// Uses the same system bus connection as [`run_session_client`]'s successful `register_session`
/// so the D-Bus sender matches the registered session owner.
///
/// If the root daemon still has an older unique name (e.g. identity drift without triggering our
/// reconnect loop), we call [`RootState::register_session`] once and retry the guard call.
pub async fn register_display_apply_attempt() -> Result<DisplayApplyPermit, String> {
    let (connection, session_id) = {
        let guard = session_registered_system_bus_cell().lock().await;
        guard
            .as_ref()
            .map(|(c, sid)| (c.clone(), *sid))
            .ok_or_else(|| {
                "session system bus not registered yet (is the session daemon running and connected to root?)"
                    .to_string()
            })?
    };

    let mut re_registered = false;
    loop {
        let proxy = RootStateProxy::new(&connection)
            .await
            .map_err(|e| format!("root state proxy for display apply guard: {e}"))?;

        match proxy.register_display_apply_attempt().await {
            Ok(true) => return Ok(DisplayApplyPermit::Allowed),
            Ok(false) => return Ok(DisplayApplyPermit::Paused),
            Err(e) => {
                let msg = e.to_string();
                let sender_mismatch = msg.contains("does not match registered session");
                if sender_mismatch && !re_registered {
                    warn!(
                        "SESSION DBus: display apply guard reports sender mismatch vs root registration; \
                         calling register_session again (session_id={session_id})"
                    );
                    proxy
                        .register_session(session_id)
                        .await
                        .map_err(|e2| {
                            format!(
                                "register_session after display guard sender mismatch: {e2} (was: {msg})"
                            )
                        })?;
                    re_registered = true;
                    continue;
                }
                return Err(format!("register_display_apply_attempt: {e}"));
            }
        }
    }
}

/// Read persisted desired display attachment/layout from the root daemon (same bus registration as
/// [`register_display_apply_attempt`]).
pub async fn read_persisted_display_mode_from_root() -> Result<(String, String), String> {
    let (connection, _) = {
        let guard = session_registered_system_bus_cell().lock().await;
        guard
            .as_ref()
            .map(|(c, sid)| (c.clone(), *sid))
            .ok_or_else(|| {
                "session system bus not registered yet (is the session daemon running and connected to root?)"
                    .to_string()
            })?
    };

    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("root state proxy for persisted display mode: {e}"))?;
    let attachment = proxy
        .desired_display_attachment()
        .await
        .map_err(|e| format!("desired_display_attachment: {e}"))?;
    let layout = proxy
        .desired_display_layout()
        .await
        .map_err(|e| format!("desired_display_layout: {e}"))?;
    Ok((attachment, layout))
}

/// Push compositor-observed attachment/layout into root persistence (validates on root side).
pub async fn report_observed_display_mode_from_session(
    attachment: String,
    layout: String,
) -> Result<(), String> {
    let (connection, _) = {
        let guard = session_registered_system_bus_cell().lock().await;
        guard
            .as_ref()
            .map(|(c, sid)| (c.clone(), *sid))
            .ok_or_else(|| {
                "session system bus not registered yet (is the session daemon running and connected to root?)"
                    .to_string()
            })?
    };

    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("root state proxy for report_observed_display_mode: {e}"))?;
    proxy
        .report_observed_display_mode(attachment, layout)
        .await
        .map_err(|e| format!("report_observed_display_mode: {e}"))?;
    Ok(())
}

pub async fn report_display_rotation_from_session(rotation: String) -> Result<(), String> {
    let (connection, _) = {
        let guard = session_registered_system_bus_cell().lock().await;
        guard
            .as_ref()
            .map(|(c, sid)| (c.clone(), *sid))
            .ok_or_else(|| {
                "session system bus not registered yet (is the session daemon running and connected to root?)"
                    .to_string()
            })?
    };

    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("root state proxy for report_display_rotation: {e}"))?;
    proxy
        .report_display_rotation(rotation)
        .await
        .map_err(|e| format!("report_display_rotation: {e}"))?;
    Ok(())
}

/// Ask the root daemon to persist the serialized `monitors.xml` to the root-owned GDM
/// greeter / system-wide locations (`/etc/xdg` + `/var/lib/gdm/seat0/config`).
pub async fn persist_display_layout_xml_to_root(xml: String) -> Result<(), String> {
    let (connection, _) = {
        let guard = session_registered_system_bus_cell().lock().await;
        guard
            .as_ref()
            .map(|(c, sid)| (c.clone(), *sid))
            .ok_or_else(|| {
                "session system bus not registered yet (is the session daemon running and connected to root?)"
                    .to_string()
            })?
    };

    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("root state proxy for persist_display_layout_xml: {e}"))?;
    proxy
        .persist_display_layout_xml(xml)
        .await
        .map_err(|e| format!("persist_display_layout_xml: {e}"))?;
    Ok(())
}

/// GDM greeter / system-wide monitors.xml targets written by the root daemon.
const GDM_MONITORS_TARGETS: &[&str] = &[
    "/etc/xdg/monitors.xml",
    "/var/lib/gdm/seat0/config/monitors.xml",
];

/// Write the serialized layout to the root-owned GDM/system targets. Best-effort per
/// target: a failure on one (e.g. the greeter dir does not exist yet) is logged but does
/// not abort the others. The `/etc/xdg` target is the robust one (root-owned, world
/// readable, immune to GDM's drifting dynamic uid); the seat0 target is greeter-specific.
async fn write_gdm_monitors_xml(xml: &str) -> Result<(), String> {
    let mut wrote_any = false;
    let mut last_err: Option<String> = None;

    for target in GDM_MONITORS_TARGETS {
        let path = std::path::Path::new(target);
        if let Some(dir) = path.parent() {
            if !dir.exists() {
                // Only /etc/xdg is guaranteed; the GDM seat0 dir is created by GDM itself.
                if target.starts_with("/etc/xdg") {
                    if let Err(e) = tokio::fs::create_dir_all(dir).await {
                        last_err = Some(format!("create {}: {e}", dir.display()));
                        continue;
                    }
                } else {
                    info!("persist GDM layout: skipping {target} (greeter dir not present yet)");
                    continue;
                }
            }
        }

        let tmp = format!("{target}.tmp.{}", std::process::id());
        if let Err(e) = tokio::fs::write(&tmp, xml).await {
            last_err = Some(format!("write {tmp}: {e}"));
            continue;
        }
        if let Err(e) = tokio::fs::rename(&tmp, target).await {
            let _ = tokio::fs::remove_file(&tmp).await;
            last_err = Some(format!("rename to {target}: {e}"));
            continue;
        }
        let _ = tokio::fs::set_permissions(
            target,
            std::os::unix::fs::PermissionsExt::from_mode(0o644),
        )
        .await;

        // The greeter-specific file must be readable by the gdm-owned seat dir.
        if target.starts_with("/var/lib/gdm") {
            chown_to_gdm(target);
        }
        best_effort_restorecon(target);

        info!("persist GDM layout: wrote {target} ({} bytes)", xml.len());
        wrote_any = true;
    }

    if wrote_any {
        Ok(())
    } else {
        Err(last_err.unwrap_or_else(|| "no GDM monitors.xml target was writable".to_string()))
    }
}

/// Best-effort chown of a path to the `gdm` user/group (greeter file ownership).
fn chown_to_gdm(path: &str) {
    if let Ok(Some(user)) = nix::unistd::User::from_name("gdm") {
        let uid = user.uid;
        let gid = user.gid;
        if let Err(e) = nix::unistd::chown(path, Some(uid), Some(gid)) {
            warn!("persist GDM layout: chown gdm {path} failed: {e}");
        }
    }
}

/// Best-effort SELinux relabel so the greeter (confined) can read the file on Fedora.
fn best_effort_restorecon(path: &str) {
    let _ = std::process::Command::new("restorecon")
        .arg("-F")
        .arg(path)
        .status();
}

pub async fn resume_display_applies() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let connection = Connection::system().await?;
    let proxy = RootStateProxy::new(&connection).await?;
    proxy.resume_display_applies().await?;
    Ok(())
}

fn push_root_state_property<T: std::fmt::Display>(
    rows: &mut Vec<(String, String)>,
    key: &'static str,
    res: zbus::Result<T>,
) {
    match res {
        Ok(v) => rows.push((key.to_string(), v.to_string())),
        Err(e) => rows.push((key.to_string(), format!("(error: {e})"))),
    }
}

/// Read all [`RootState`] D-Bus properties for the CLI (`zenbook-duo-daemon state`).
///
/// Uses the same [`RootStateProxy`] as operator commands. If some properties show `(error: …
/// UnknownProperty…)`, the **running** root daemon is older than this binary — restart the root
/// service after installing a build that exports those members.
pub async fn query_root_state_for_cli() -> Result<Vec<(String, String)>, String> {
    let connection = Connection::system()
        .await
        .map_err(|e| format!("cannot connect to system D-Bus: {e}"))?;
    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("cannot attach to {}: {e}", ROOT_DBUS_SERVICE))?;

    let mut rows = Vec::new();
    push_root_state_property(
        &mut rows,
        "keyboard_usb_connected",
        proxy.keyboard_usb_connected().await,
    );
    push_root_state_property(
        &mut rows,
        "keyboard_pogo_docked",
        proxy.keyboard_pogo_docked().await,
    );
    push_root_state_property(&mut rows, "keyboard_attached", proxy.keyboard_attached().await);
    push_root_state_property(
        &mut rows,
        "bluetooth_connected",
        proxy.bluetooth_connected().await,
    );
    push_root_state_property(
        &mut rows,
        "keyboard_connected",
        proxy.keyboard_connected().await,
    );
    push_root_state_property(
        &mut rows,
        "keyboard_connection_type",
        proxy.keyboard_connection_type().await,
    );
    push_root_state_property(
        &mut rows,
        "keyboard_battery_present",
        proxy.keyboard_battery_present().await,
    );
    push_root_state_property(
        &mut rows,
        "keyboard_battery_percentage",
        proxy.keyboard_battery_percentage().await,
    );
    push_root_state_property(
        &mut rows,
        "keyboard_battery_charging",
        proxy.keyboard_battery_charging().await,
    );
    push_root_state_property(
        &mut rows,
        "keyboard_battery_full",
        proxy.keyboard_battery_full().await,
    );
    push_root_state_property(&mut rows, "mic_mute_led", proxy.mic_mute_led().await);
    push_root_state_property(
        &mut rows,
        "keyboard_backlight_level",
        proxy.keyboard_backlight_level().await,
    );
    push_root_state_property(
        &mut rows,
        "keyboard_backlight_effective_level",
        proxy.keyboard_backlight_effective_level().await,
    );
    push_root_state_property(
        &mut rows,
        "tablet_mapping_enabled",
        proxy.tablet_mapping_enabled().await,
    );
    push_root_state_property(
        &mut rows,
        "tablet_mapping_mode",
        proxy.tablet_mapping_mode().await,
    );
    push_root_state_property(
        &mut rows,
        "tablet_mapping_apply_nonce",
        proxy.tablet_mapping_apply_nonce().await,
    );
    push_root_state_property(&mut rows, "desired_primary", proxy.desired_primary().await);
    push_root_state_property(
        &mut rows,
        "desired_secondary_enabled",
        proxy.desired_secondary_enabled().await,
    );
    push_root_state_property(
        &mut rows,
        "ambient_light_enabled",
        proxy.ambient_light_enabled().await,
    );
    push_root_state_property(
        &mut rows,
        "ambient_light_available",
        proxy.ambient_light_available().await,
    );
    push_root_state_property(
        &mut rows,
        "ambient_keyboard_backlight_enabled",
        proxy.ambient_keyboard_backlight_enabled().await,
    );
    push_root_state_property(
        &mut rows,
        "idle_timeout_seconds",
        proxy.idle_timeout_seconds().await,
    );
    push_root_state_property(
        &mut rows,
        "desired_display_attachment",
        proxy.desired_display_attachment().await,
    );
    push_root_state_property(
        &mut rows,
        "desired_display_layout",
        proxy.desired_display_layout().await,
    );
    push_root_state_property(
        &mut rows,
        "display_rotation",
        proxy.display_rotation().await,
    );
    push_root_state_property(&mut rows, "display_brightness", proxy.display_brightness().await);
    push_root_state_property(
        &mut rows,
        "display_apply_paused",
        proxy.display_apply_paused().await,
    );
    push_root_state_property(
        &mut rows,
        "session_registered",
        proxy.session_registered().await,
    );
    push_root_state_property(&mut rows, "session_owner", proxy.session_owner().await);
    push_root_state_property(&mut rows, "session_id", proxy.session_id().await);
    push_root_state_property(
        &mut rows,
        "session_last_seen_usec",
        proxy.session_last_seen_usec().await,
    );
    push_root_state_property(&mut rows, "session_quiet", proxy.session_quiet().await);

    Ok(rows)
}

pub async fn run_operator_command(cmd: OperatorCommand) -> Result<(), String> {
    let connection = Connection::system()
        .await
        .map_err(|e| format!("system bus: {e}"))?;
    let proxy = RootStateProxy::new(&connection)
        .await
        .map_err(|e| format!("Connect to {}: {e}", ROOT_DBUS_SERVICE))?;

    match cmd {
        OperatorCommand::ToggleMicMute => proxy.toggle_mic_mute().await,
        OperatorCommand::SetMicMute(enabled) => proxy.set_mic_mute(enabled).await,
        OperatorCommand::ToggleKeyboardBacklight => proxy.toggle_keyboard_backlight().await,
        OperatorCommand::SetKeyboardBacklightLevel(level) => {
            proxy.set_keyboard_backlight_level(level).await
        }
        OperatorCommand::SetDesiredPrimary(primary) => proxy.set_desired_primary(primary).await,
        OperatorCommand::SetTabletMappingEnabled(enabled) => {
            proxy.set_tablet_mapping_enabled(enabled).await
        }
        OperatorCommand::SetTabletMappingMode(mode) => proxy.set_tablet_mapping_mode(mode).await,
        OperatorCommand::ToggleTabletMappingMode => proxy.toggle_tablet_mapping_mode().await,
        OperatorCommand::ApplyTabletMapping => proxy.apply_tablet_mapping().await,
        OperatorCommand::ToggleSecondaryDisplay => proxy.toggle_secondary_display().await,
        OperatorCommand::SetSecondaryDisplay(enabled) => {
            proxy.set_desired_secondary_enabled(enabled).await
        }
    }
    .map_err(|e| e.to_string())
}

async fn wait_for_internal_ack(
    rx: &Arc<Mutex<mpsc::Receiver<()>>>,
    timeout: Duration,
    what: &str,
) -> bool {
    match tokio::time::timeout(timeout, async { rx.lock().await.recv().await }).await {
        Ok(Some(_)) => true,
        Ok(None) => {
            warn!("SESSION DBus: {what} channel closed before handler completed");
            false
        }
        Err(_) => {
            warn!("SESSION DBus: timed out waiting for {what}");
            false
        }
    }
}

async fn apply_keyboard_usb_update(kb_usb_tx: &broadcast::Sender<bool>, value: bool) {
    kb_usb_tx.send(value).ok();
}

async fn apply_keyboard_pogo_update(
    proxy: &RootStateProxy<'_>,
    kb_pogo_tx: &broadcast::Sender<bool>,
    kb_result_rx: &Arc<Mutex<mpsc::Receiver<()>>>,
    value: bool,
) {
    kb_pogo_tx.send(value).ok();
    if wait_for_internal_ack(kb_result_rx, Duration::from_secs(5), "keyboard pogo handler").await {
        if let Err(e) = proxy.acknowledge_keyboard_pogo_docked(value).await {
            warn!("SESSION DBus: failed to acknowledge keyboard_pogo_docked={value}: {e}");
        }
    }
}

async fn apply_desired_secondary_update(
    proxy: &RootStateProxy<'_>,
    desired_secondary: &Arc<RwLock<bool>>,
    desired_secondary_tx: &broadcast::Sender<bool>,
    secondary_result_rx: &Arc<Mutex<mpsc::Receiver<()>>>,
    value: bool,
) {
    *desired_secondary.write().await = value;
    desired_secondary_tx.send(value).ok();
    // ACK means "received by session daemon", not "fully applied".
    if let Err(e) = proxy.acknowledge_desired_secondary_applied(value).await {
        warn!("SESSION DBus: failed to acknowledge desired_secondary_enabled={value}: {e}");
    }

    // Keep draining internal completion ACKs for observability/backpressure, but do not
    // block root-facing ACK flow on full display application.
    let secondary_result_rx = Arc::clone(secondary_result_rx);
    tokio::spawn(async move {
        let _ = wait_for_internal_ack(
            &secondary_result_rx,
            Duration::from_secs(8),
            "desired secondary handler",
        )
        .await;
    });
}

async fn apply_desired_primary_update(
    desired_primary: &Arc<RwLock<String>>,
    desired_primary_tx: &broadcast::Sender<String>,
    value: String,
) {
    *desired_primary.write().await = value.clone();
    desired_primary_tx.send(value).ok();
}

fn coalesce_latest_ambient_value(
    ambient_report_rx: &mut mpsc::Receiver<bool>,
    latest_ambient_value: &mut Option<bool>,
) -> bool {
    loop {
        match ambient_report_rx.try_recv() {
            Ok(value) => *latest_ambient_value = Some(value),
            Err(tokio::sync::mpsc::error::TryRecvError::Empty) => return true,
            Err(tokio::sync::mpsc::error::TryRecvError::Disconnected) => return false,
        }
    }
}

async fn sync_tablet_mapping_from_root(
    proxy: &RootStateProxy<'_>,
    tablet_config: &Arc<RwLock<TabletMappingConfig>>,
    tablet_remap_tx: &broadcast::Sender<()>,
) {
    let Ok(enabled) = proxy.tablet_mapping_enabled().await else {
        return;
    };
    let Ok(mode_s) = proxy.tablet_mapping_mode().await else {
        return;
    };
    let mode = tablet_mode_from_str(&mode_s).unwrap_or(TabletMapMode::OneToOne);
    let new_cfg = TabletMappingConfig { enable: enabled, mode };
    let should_remap = {
        let mut guard = tablet_config.write().await;
        if *guard == new_cfg {
            false
        } else {
            *guard = new_cfg;
            true
        }
    };
    if should_remap {
        info!(
            "SESSION DBus: tablet mapping synced enabled={enabled} mode={}",
            tablet_mode_to_str(mode)
        );
        let _ = tablet_remap_tx.send(());
    }
}

fn request_tablet_remap(tablet_remap_tx: &broadcast::Sender<()>) {
    if tablet_remap_tx.send(()).is_ok() {
        info!("SESSION DBus: tablet mapping immediate reapply requested");
    }
}

pub async fn run_session_client(
    session_id: u64,
    desired_primary: Arc<RwLock<String>>,
    desired_secondary: Arc<RwLock<bool>>,
    desired_primary_tx: broadcast::Sender<String>,
    desired_secondary_tx: broadcast::Sender<bool>,
    kb_usb_tx: broadcast::Sender<bool>,
    kb_pogo_tx: broadcast::Sender<bool>,
    secondary_result_rx: Arc<Mutex<mpsc::Receiver<()>>>,
    kb_result_rx: Arc<Mutex<mpsc::Receiver<()>>>,
    mut ambient_report_rx: mpsc::Receiver<bool>,
    tablet_config: Arc<RwLock<TabletMappingConfig>>,
    tablet_remap_tx: broadcast::Sender<()>,
) {
    let mut latest_ambient_value = None;

    loop {
        if !coalesce_latest_ambient_value(&mut ambient_report_rx, &mut latest_ambient_value) {
            replace_session_registered_system_bus(None).await;
            return;
        }

        let connection = match Connection::system().await {
            Ok(connection) => connection,
            Err(e) => {
                error!("SESSION DBus: failed to connect to system bus: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let proxy = match RootStateProxy::new(&connection).await {
            Ok(proxy) => proxy,
            Err(e) => {
                error!("SESSION DBus: failed to create root state proxy: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        if let Err(e) = proxy.register_session(session_id).await {
            error!("SESSION DBus: failed to register with root daemon: {e}");
            tokio::time::sleep(Duration::from_secs(2)).await;
            continue;
        }
        info!("SESSION DBus: registered with root daemon");
        // Publish immediately so display reconcile can use the guard before stream setup finishes;
        // keep the same `Connection` as `register_session` for matching bus unique name.
        replace_session_registered_system_bus(Some((connection.clone(), session_id))).await;

        if !coalesce_latest_ambient_value(&mut ambient_report_rx, &mut latest_ambient_value) {
            replace_session_registered_system_bus(None).await;
            return;
        }

        if let Some(value) = latest_ambient_value {
            if let Err(e) = proxy.report_ambient_light_enabled(value).await {
                warn!("SESSION DBus: failed to replay ambient_light_enabled={value} after registration: {e}");
                replace_session_registered_system_bus(None).await;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        }

        let dbus_proxy = match DBusProxy::new(&connection).await {
            Ok(proxy) => proxy,
            Err(e) => {
                error!("SESSION DBus: failed to create org.freedesktop.DBus proxy: {e}");
                replace_session_registered_system_bus(None).await;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let mut owner_changes = match dbus_proxy
            .receive_name_owner_changed_with_args(&[(0, ROOT_DBUS_SERVICE)])
            .await
        {
            Ok(stream) => stream,
            Err(e) => {
                error!("SESSION DBus: failed to subscribe to root owner changes: {e}");
                replace_session_registered_system_bus(None).await;
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let mut keyboard_usb_changes = proxy
            .inner()
            .receive_property_changed("KeyboardUsbConnected")
            .await;
        let mut keyboard_pogo_changes = proxy
            .inner()
            .receive_property_changed("KeyboardPogoDocked")
            .await;
        let mut desired_secondary_changes = proxy
            .inner()
            .receive_property_changed("DesiredSecondaryEnabled")
            .await;
        let mut desired_primary_changes = proxy.inner().receive_property_changed("DesiredPrimary").await;
        let mut display_brightness_changes = proxy.inner().receive_property_changed("DisplayBrightness").await;
        let mut tablet_enabled_changes = proxy
            .inner()
            .receive_property_changed::<bool>("TabletMappingEnabled")
            .await;
        let mut tablet_mode_changes = proxy
            .inner()
            .receive_property_changed::<String>("TabletMappingMode")
            .await;
        let mut tablet_apply_nonce_changes = proxy
            .inner()
            .receive_property_changed::<u32>("TabletMappingApplyNonce")
            .await;

        match proxy.keyboard_usb_connected().await {
            Ok(value) => apply_keyboard_usb_update(&kb_usb_tx, value).await,
            Err(e) => warn!(
                "SESSION DBus: failed to read initial keyboard_usb_connected property: {e}"
            ),
        }
        match proxy.keyboard_pogo_docked().await {
            Ok(value) => {
                apply_keyboard_pogo_update(&proxy, &kb_pogo_tx, &kb_result_rx, value).await;
            }
            Err(e) => warn!(
                "SESSION DBus: failed to read initial keyboard_pogo_docked property: {e}"
            ),
        }

        match proxy.desired_secondary_enabled().await {
            Ok(value) => {
                apply_desired_secondary_update(
                    &proxy,
                    &desired_secondary,
                    &desired_secondary_tx,
                    &secondary_result_rx,
                    value,
                )
                .await;
            }
            Err(e) => warn!("SESSION DBus: failed to read initial desired_secondary_enabled property: {e}"),
        }

        match proxy.desired_primary().await {
            Ok(value) => apply_desired_primary_update(&desired_primary, &desired_primary_tx, value).await,
            Err(e) => warn!("SESSION DBus: failed to read initial desired_primary property: {e}"),
        }

        match proxy.display_brightness().await {
            Ok(value) if value > 0 => {
                if let Err(e) = crate::session::display::apply_display_brightness_value(value).await {
                    warn!("SESSION DBus: failed to apply initial display_brightness={value}: {e}");
                }
            }
            Ok(_) => {}
            Err(e) => warn!("SESSION DBus: failed to read initial display_brightness property: {e}"),
        }

        sync_tablet_mapping_from_root(&proxy, &tablet_config, &tablet_remap_tx).await;

        let mut reconnect = false;
        let mut heartbeat = tokio::time::interval(Duration::from_secs(10));
        heartbeat.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        while !reconnect {
            tokio::select! {
                _ = heartbeat.tick() => {
                    if let Err(e) = proxy.ping_session().await {
                        warn!("SESSION DBus: heartbeat to root daemon failed: {e}");
                        reconnect = true;
                    }
                }
                change = owner_changes.next() => {
                    match change {
                        Some(signal) => match signal.args() {
                            Ok(args) if args.new_owner().is_none() => {
                                info!("SESSION DBus: root daemon disappeared from system bus");
                                reconnect = true;
                            }
                            Ok(_) => {}
                            Err(e) => {
                                warn!("SESSION DBus: failed to parse root owner change: {e}");
                                reconnect = true;
                            }
                        },
                        None => reconnect = true,
                    }
                }
                change = keyboard_usb_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(value) => apply_keyboard_usb_update(&kb_usb_tx, value).await,
                            Err(e) => warn!("SESSION DBus: failed to decode KeyboardUsbConnected change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                change = keyboard_pogo_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(value) => apply_keyboard_pogo_update(&proxy, &kb_pogo_tx, &kb_result_rx, value).await,
                            Err(e) => warn!("SESSION DBus: failed to decode KeyboardPogoDocked change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                change = desired_secondary_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(value) => {
                                apply_desired_secondary_update(
                                    &proxy,
                                    &desired_secondary,
                                    &desired_secondary_tx,
                                    &secondary_result_rx,
                                    value,
                                )
                                .await;
                            }
                            Err(e) => warn!("SESSION DBus: failed to decode DesiredSecondaryEnabled change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                change = desired_primary_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(value) => apply_desired_primary_update(&desired_primary, &desired_primary_tx, value).await,
                            Err(e) => warn!("SESSION DBus: failed to decode DesiredPrimary change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                change = display_brightness_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(value) if value > 0 => {
                                if let Err(e) = crate::session::display::apply_display_brightness_value(value).await {
                                    warn!("SESSION DBus: failed to apply display_brightness={value}: {e}");
                                }
                            }
                            Ok(_) => {}
                            Err(e) => warn!("SESSION DBus: failed to decode DisplayBrightness change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                change = tablet_enabled_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(_) => {
                                sync_tablet_mapping_from_root(
                                    &proxy,
                                    &tablet_config,
                                    &tablet_remap_tx,
                                )
                                .await;
                            }
                            Err(e) => warn!("SESSION DBus: failed to decode TabletMappingEnabled change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                change = tablet_mode_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(_) => {
                                sync_tablet_mapping_from_root(
                                    &proxy,
                                    &tablet_config,
                                    &tablet_remap_tx,
                                )
                                .await;
                            }
                            Err(e) => warn!("SESSION DBus: failed to decode TabletMappingMode change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                change = tablet_apply_nonce_changes.next() => {
                    match change {
                        Some(change) => match change.get().await {
                            Ok(_) => request_tablet_remap(&tablet_remap_tx),
                            Err(e) => warn!("SESSION DBus: failed to decode TabletMappingApplyNonce change: {e}"),
                        },
                        None => reconnect = true,
                    }
                }
                ambient_value = ambient_report_rx.recv() => {
                    match ambient_value {
                        Some(value) => {
                            latest_ambient_value = Some(value);
                            if let Err(e) = proxy.report_ambient_light_enabled(value).await {
                                warn!("SESSION DBus: failed to report ambient_light_enabled={value}: {e}");
                                reconnect = true;
                            }
                        }
                        None => reconnect = true,
                    }
                }
            }
        }

        replace_session_registered_system_bus(None).await;
        info!("SESSION DBus: reconnecting to root daemon");
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
}
