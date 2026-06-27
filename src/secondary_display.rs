use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::OnceLock;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::StreamExt;

use inotify::{Inotify, WatchMask};
use log::{info, warn};
use tokio::fs;
use tokio::sync::broadcast;

use crate::config::Config;
use crate::events::Event;
use crate::state::KeyboardStateManager;

const ENFORCER_COOLDOWN_SECS: u64 = 8;
static FORCE_SYSFS_FALLBACK_ONCE: AtomicBool = AtomicBool::new(false);

/// Where an **immediate** sysfs secondary on/off write originates (not the session-stable D-Bus ack path).
#[derive(Clone, Copy, Debug)]
enum SecondarySysfsImmediatePhase {
    /// First sync when the root secondary-display task starts.
    RootTaskStartup,
    /// `Event::SecondaryDisplay` from internal state (keyboard attach policy, coordinator emit, etc.).
    BroadcastEventSecondaryDisplay,
    /// Periodic reconcile when no GNOME session is registered on the system bus.
    EnforcerNoSession,
}
static SECONDARY_STATUS_PATH: OnceLock<String> = OnceLock::new();
/// Same `Arc` as the per-task `last_change` for sysfs writes (enforcer cooldown + session-stable off).
static SECONDARY_LAST_SYSFS_CHANGE: OnceLock<Arc<AtomicU64>> = OnceLock::new();
static BRIGHTNESS_SYNC_PAUSED_UNTIL: AtomicU64 = AtomicU64::new(0);

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn brightness_sync_paused() -> bool {
    now_secs() < BRIGHTNESS_SYNC_PAUSED_UNTIL.load(Ordering::Relaxed)
}

pub fn pause_brightness_sync_for(duration: Duration) {
    let until = now_secs().saturating_add(duration.as_secs().max(1));
    BRIGHTNESS_SYNC_PAUSED_UNTIL.store(until, Ordering::Relaxed);
}

/// After the session daemon reports KMS layout stable with the secondary logically off, turn off
/// the platform sysfs gate. Normally only when root `desired_secondary_enabled` is false; when
/// the USB keyboard is docked, root still allows sysfs off for power save even if the user keeps
/// "secondary desired" true for when they undock.
pub async fn apply_secondary_sysfs_poweroff_when_desired_off(
    state_manager: &KeyboardStateManager,
) {
    if state_manager.is_secondary_display_desired_enabled() {
        if !state_manager.is_keyboard_pogo_docked() {
            info!(
                "ROOT [secondary sysfs] Moment B (session-stable D-Bus ack): NOT writing `off` — desired_secondary_enabled is true and keyboard is not pogo-docked (policy: do not cut platform power while undocked with secondary still wanted)"
            );
            return;
        }
        info!(
            "ROOT [secondary sysfs] Moment B (session-stable D-Bus ack): desired_secondary_enabled is still true but keyboard is pogo-docked — will write sysfs `off` if needed (clamshell / docked power save)"
        );
    }
    let Some(status_path) = SECONDARY_STATUS_PATH.get() else {
        warn!(
            "ROOT [secondary sysfs] Moment B (session-stable D-Bus ack): status path not initialised; cannot write"
        );
        return;
    };
    if !is_secondary_display_enabled_actual(status_path).await {
        info!(
            "ROOT [secondary sysfs] Moment B (session-stable D-Bus ack): NOT writing `off` to {} — sysfs already reads as powered-down for this daemon (not `on`/`connected`); connector may already be disconnected or gate already clear (no redundant write)",
            status_path
        );
        return;
    }
    if let Some(last_change) = SECONDARY_LAST_SYSFS_CHANGE.get() {
        last_change.store(now_secs(), Ordering::Relaxed);
    }
    info!(
        "ROOT [secondary sysfs] Moment B (session-stable D-Bus ack): writing sysfs `off` now to {} (session reported stable layout without driving eDP-2)",
        status_path
    );
    if let Err(e) = fs::write(status_path, b"off").await {
        warn!(
            "ROOT [secondary sysfs] Moment B (session-stable D-Bus ack): sysfs write `off` failed: {}",
            e
        );
    } else {
        info!(
            "ROOT [secondary sysfs] Moment B (session-stable D-Bus ack): sysfs write `off` completed successfully"
        );
    }
}

async fn control_secondary_display(
    status_path: &str,
    enable: bool,
    last_change: &Arc<AtomicU64>,
    phase: SecondarySysfsImmediatePhase,
) {
    // Check current state first to avoid redundant sysfs writes (which cause flicker)
    let actual_enabled = is_secondary_display_enabled_actual(status_path).await;
    if actual_enabled == enable {
        // Already in the desired state, no need to write
        return;
    }
    
    let data: &[u8] = if enable { b"on" } else { b"off" };
    last_change.store(now_secs(), Ordering::Relaxed);
    if !enable {
        info!(
            "ROOT [secondary sysfs] Moment A (immediate path, {:?}): writing sysfs `off` to {} — this is NOT the session-stable D-Bus ack path; it runs here because either no session is registered or sysfs fallback was armed",
            phase, status_path
        );
    }
    if let Err(e) = fs::write(status_path, data).await {
        warn!(
            "ROOT [secondary sysfs] immediate path {:?}: sysfs write failed: {}",
            phase, e
        );
    }
}

pub fn arm_sysfs_fallback_once() {
    FORCE_SYSFS_FALLBACK_ONCE.store(true, Ordering::SeqCst);
}

/// Write sysfs `on` for the secondary display connector if it is not already on.
/// Must be called before notifying the session daemon so that GNOME can see the
/// connector when it applies the DisplayConfig change.
pub async fn ensure_secondary_display_on() {
    let Some(status_path) = SECONDARY_STATUS_PATH.get() else {
        warn!("ensure_secondary_display_on: status path not initialised yet");
        return;
    };
    let actual = is_secondary_display_enabled_actual(status_path).await;
    if actual {
        return; // already on, nothing to write
    }
    if let Err(e) = fs::write(status_path, b"on").await {
        warn!("ensure_secondary_display_on: failed to write sysfs: {e}");
    }
}

async fn is_secondary_display_enabled_actual(status_path: &str) -> bool {
    if let Ok(contents) = fs::read_to_string(status_path).await {
        let status = contents.trim();
        // Display is enabled if status is "on" or "connected" (when enabled)
        status == "on" || status == "connected"
    } else {
        false
    }
}

/// Wait until sysfs reflects `enable`, or until `deadline`, using async inotify on the parent
/// directory. Each loop iteration re-checks the deadline before awaiting the next event so an
/// overall timeout does not leave a blocking inotify read on the worker pool.
async fn wait_for_secondary_display_state_until(
    status_path: String,
    enable: bool,
    deadline: Instant,
) -> bool {
    if is_secondary_display_enabled_actual(&status_path).await == enable {
        return true;
    }

    let path = Path::new(&status_path);
    let Some(parent) = path.parent() else {
        return false;
    };
    let target_name = path.file_name().map(|name| name.to_os_string());

    let inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(_) => return false,
    };

    if inotify
        .watches()
        .add(
            parent,
            WatchMask::MODIFY
                | WatchMask::ATTRIB
                | WatchMask::CLOSE_WRITE
                | WatchMask::CREATE
                | WatchMask::MOVED_TO
                | WatchMask::MOVE_SELF
                | WatchMask::DELETE_SELF,
        )
        .is_err()
    {
        return false;
    }

    let mut buffer = [0u8; 1024];
    let mut stream = match inotify.into_event_stream(&mut buffer) {
        Ok(stream) => stream,
        Err(_) => return false,
    };

    loop {
        if Instant::now() >= deadline {
            return false;
        }

        if is_secondary_display_enabled_actual(&status_path).await == enable {
            return true;
        }

        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return false;
        }

        tokio::select! {
            biased;
            event_opt = stream.next() => {
                match event_opt {
                    Some(Ok(event)) => {
                        let saw_status_change = target_name
                            .as_ref()
                            .map(|name| {
                                event
                                    .name
                                    .as_ref()
                                    .map(|event_name| event_name == name)
                                    .unwrap_or(true)
                            })
                            .unwrap_or(true);

                        if saw_status_change
                            && is_secondary_display_enabled_actual(&status_path).await == enable
                        {
                            return true;
                        }
                    }
                    Some(Err(_)) => return false,
                    None => return false,
                }
            }
            _ = tokio::time::sleep(remaining) => {
                return false;
            }
        }
    }
}

pub async fn wait_for_secondary_display_state(enable: bool, timeout: Duration) -> bool {
    let Some(status_path) = SECONDARY_STATUS_PATH.get().cloned() else {
        return false;
    };

    if is_secondary_display_enabled_actual(&status_path).await == enable {
        return true;
    }

    let deadline = Instant::now() + timeout;
    wait_for_secondary_display_state_until(status_path, enable, deadline).await
}

async fn sync_secondary_brightness_once_async(
    source_path: &str,
    target_path: &str,
    status_path: &str,
    state_manager: &KeyboardStateManager,
) {
    let Some(trimmed) = persist_primary_brightness_value_async(source_path, state_manager).await
    else {
        return;
    };

    if brightness_sync_paused() {
        return;
    }

    if !state_manager.is_secondary_display_desired_enabled() {
        return;
    }

    if !is_secondary_display_enabled_actual(status_path).await {
        return;
    }

    if let Err(e) = fs::write(target_path, trimmed.as_bytes()).await {
        warn!("Failed to sync secondary display brightness: {}", e);
    }
}

async fn persist_primary_brightness_value_async(
    source_path: &str,
    state_manager: &KeyboardStateManager,
) -> Option<String> {
    let brightness = fs::read_to_string(source_path).await.ok()?;
    let trimmed = brightness.trim();
    let value = trimmed.parse::<u32>().ok()?;
    state_manager.set_display_brightness_value(value).await;
    Some(trimmed.to_string())
}

async fn apply_persisted_display_brightness_async(
    primary_path: &str,
    secondary_path: &str,
    state_manager: &KeyboardStateManager,
) {
    let Some(brightness) = state_manager.get_display_brightness_value() else {
        return;
    };

    for path in [primary_path, secondary_path] {
        if let Err(e) = fs::write(path, brightness.to_string()).await {
            warn!("Failed to restore persisted display brightness on {}: {}", path, e);
        }
    }
}

async fn restore_secondary_brightness_after_enable(
    primary_path: String,
    secondary_path: String,
    state_manager: KeyboardStateManager,
) {
    let brightness = persist_primary_brightness_value_async(&primary_path, &state_manager)
        .await
        .or_else(|| {
            state_manager
                .get_display_brightness_value()
                .map(|value| value.to_string())
        });

    if !wait_for_secondary_display_state(true, Duration::from_secs(8)).await {
        return;
    }

    let Some(brightness) = brightness else {
        return;
    };

    if let Err(e) = fs::write(&secondary_path, brightness).await {
        warn!(
            "Failed to restore secondary display brightness after enable on {}: {}",
            secondary_path, e
        );
    }
}

async fn watch_primary_brightness_loop(
    source_path: String,
    target_path: String,
    status_path: String,
    state_manager: KeyboardStateManager,
) {
    sync_secondary_brightness_once_async(
        &source_path,
        &target_path,
        &status_path,
        &state_manager,
    )
    .await;

    let path = Path::new(&source_path);
    let Some(parent) = path.parent() else {
        return;
    };
    let target_name = path.file_name().map(|name| name.to_os_string());

    let inotify = match Inotify::init() {
        Ok(inotify) => inotify,
        Err(e) => {
            warn!("Failed to initialize brightness inotify watcher: {}", e);
            return;
        }
    };

    if let Err(e) = inotify.watches().add(
        parent,
        WatchMask::MODIFY
            | WatchMask::ATTRIB
            | WatchMask::CLOSE_WRITE
            | WatchMask::CREATE
            | WatchMask::MOVED_TO
            | WatchMask::MOVE_SELF
            | WatchMask::DELETE_SELF,
    ) {
        warn!("Failed to watch primary brightness path: {}", e);
        return;
    }

    let mut buffer = [0u8; 1024];
    let mut stream = match inotify.into_event_stream(&mut buffer) {
        Ok(stream) => stream,
        Err(e) => {
            warn!("Brightness watcher: failed to create event stream: {}", e);
            return;
        }
    };

    while let Some(event_result) = stream.next().await {
        match event_result {
            Ok(event) => {
                let saw_brightness_change = target_name
                    .as_ref()
                    .map(|name| {
                        event
                            .name
                            .as_ref()
                            .map(|event_name| event_name == name)
                            .unwrap_or(true)
                    })
                    .unwrap_or(true);

                if saw_brightness_change {
                    sync_secondary_brightness_once_async(
                        &source_path,
                        &target_path,
                        &status_path,
                        &state_manager,
                    )
                    .await;
                }
            }
            Err(e) => {
                warn!("Brightness watcher failed to read inotify events: {}", e);
                return;
            }
        }
    }
}

/// Secondary display consumer - manages secondary display state and syncs with hardware
pub async fn start_secondary_display_task(
    config: Config,
    state_manager: KeyboardStateManager,
    mut event_receiver: broadcast::Receiver<Event>,
) {
    let status_path = config.secondary_display_status_path.clone();
    let _ = SECONDARY_STATUS_PATH.set(status_path.clone());
    let last_change = Arc::new(AtomicU64::new(0));
    let _ = SECONDARY_LAST_SYSFS_CHANGE.set(last_change.clone());

    control_secondary_display(
        &status_path,
        state_manager.is_secondary_display_enabled(),
        &last_change,
        SecondarySysfsImmediatePhase::RootTaskStartup,
    )
    .await;
    apply_persisted_display_brightness_async(
        &config.primary_backlight_path,
        &config.secondary_backlight_path,
        &state_manager,
    )
    .await;

    // Task to handle events
    {
        let status_path = status_path.clone();
        let last_change = last_change.clone();
        let primary_path = config.primary_backlight_path.clone();
        let secondary_path = config.secondary_backlight_path.clone();
        let state_manager = state_manager.clone();
        tokio::spawn(async move {
            loop {
                match event_receiver.recv().await {
                    Ok(Event::SecondaryDisplay(new_state)) => {
                        let forced_fallback = FORCE_SYSFS_FALLBACK_ONCE.swap(false, Ordering::SeqCst);
                        // For enable: sysfs is usually written before the session is notified; this
                        // call is a no-op if already on.
                        // For disable with a registered session: skip immediate sysfs off; the
                        // session daemon calls `acknowledge_secondary_sysfs_poweroff_ready` after a
                        // successful debounced reconcile (stable KMS), and root then writes sysfs
                        // `off`. Forced fallback still routes through `control_secondary_display`.
                        if !new_state && crate::dbus_state::is_session_registered() && !forced_fallback {
                            info!(
                                "ROOT [secondary sysfs] Moment A (`Event::SecondaryDisplay(false)`): NOT writing sysfs `off` immediately — GNOME session daemon is connected. Reason: avoid racing Mutter/KMS; platform `off` is deferred to Moment B after the session calls `acknowledge_secondary_sysfs_poweroff_ready` (post-reconcile). This is expected — not a failure and not \"giving up\" on power-down."
                            );
                            continue;
                        }
                        if !new_state && crate::dbus_state::is_session_registered() && forced_fallback {
                            info!(
                                "ROOT [secondary sysfs] Moment A (`Event::SecondaryDisplay(false)`): sysfs fallback armed — performing immediate sysfs `off` despite session (keyboard-attached policy / timeout path)"
                            );
                        }
                        control_secondary_display(
                            &status_path,
                            new_state,
                            &last_change,
                            SecondarySysfsImmediatePhase::BroadcastEventSecondaryDisplay,
                        )
                        .await;
                        if new_state {
                            tokio::spawn(restore_secondary_brightness_after_enable(
                                primary_path.clone(),
                                secondary_path.clone(),
                                state_manager.clone(),
                            ));
                        }
                    }
                    Ok(_) => {}
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        continue;
                    }
                    Err(broadcast::error::RecvError::Closed) => {
                        break;
                    }
                }
            }
        });
    }

    // Task to periodically verify and enforce secondary display state
    // For some reason the secondary display always gets enabled when resuming from suspend.
    // A cooldown prevents the enforcer from fighting the kernel while it processes a state change.
    {
        let state_manager = state_manager.clone();
        let status_path = status_path.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_millis(500));
            loop {
                interval.tick().await;
                if crate::dbus_state::is_session_registered() {
                    continue;
                }
                if now_secs().saturating_sub(last_change.load(Ordering::Relaxed)) < ENFORCER_COOLDOWN_SECS {
                    continue;
                }
                let actual_enabled = is_secondary_display_enabled_actual(&status_path).await;
                let desired_enabled = state_manager.is_secondary_display_enabled();
                if actual_enabled != desired_enabled {
                    warn!(
                        "Secondary display is not in the desired state, actual: {}, desired: {}",
                        actual_enabled, desired_enabled
                    );
                    control_secondary_display(
                        &status_path,
                        desired_enabled,
                        &last_change,
                        SecondarySysfsImmediatePhase::EnforcerNoSession,
                    )
                    .await;
                }
            }
        });
    }

    // Task to sync secondary display brightness from primary brightness events.
    // Only mirror brightness while the secondary is desired and actually present.
    {
        let source = config.primary_backlight_path.clone();
        let target = config.secondary_backlight_path.clone();
        let status_path = status_path.clone();
        let state_manager = state_manager.clone();
        tokio::spawn(watch_primary_brightness_loop(
            source,
            target,
            status_path,
            state_manager,
        ));
    }
}
