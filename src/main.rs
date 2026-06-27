use std::panic;
use std::time::Duration;
use std::{path::PathBuf, process, sync::Arc};
use tokio::fs;
use tokio::signal::unix::{SignalKind, signal};
use tokio::sync::{Mutex, broadcast};
use crate::dbus_state::OperatorCommand;
use crate::{
    config::{Config, DEFAULT_CONFIG_PATH},
    events::Event,
    idle_detection::start_idle_detection_task,
    keyboard_usb::{find_wired_keyboard, start_usb_keyboard_monitor_task, start_usb_keyboard_task},
    mute_state::start_listen_mute_state_thread,
    secondary_display::start_secondary_display_task,
    state::KeyboardStateManager,
    virtual_keyboard::VirtualKeyboard,
};
use clap::{Parser, Subcommand, ValueEnum};
use keyboard_bt::start_bt_keyboard_monitor_task;
use keyboard_battery_bt::start_bt_battery_monitor_task;
use log::{error, info, warn};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
enum Args {
    /// Run the root daemon (requires root privileges)
    Run {
        /// Path to the config file, defaults to /etc/zenbook-duo-daemon/config.toml
        #[arg(short, long)]
        config_path: Option<PathBuf>,
    },
    /// Run the session daemon (runs as the logged-in user, started by systemd user service)
    Session,
    /// Migrate config file - backs up old config and writes new default if read fails
    MigrateConfig {
        /// Path to the config file, defaults to /etc/zenbook-duo-daemon/config.toml
        #[arg(short, long)]
        config_path: Option<PathBuf>,
    },
    /// Resume display applies after root-side safety guard paused them (Polkit: active local session or root).
    ResumeDisplayApplies,
    /// Print root daemon state from system D-Bus (`asus.zenbook.duo.State` properties).
    ///
    /// Uses the same property names as the running service. If a field shows `(error: UnknownProperty)`,
    /// restart `zenbook-duo-daemon.service` after installing a binary that exports that property.
    State,
    /// Operator command (D-Bus to running root daemon; Polkit must allow `org.zenbook.duo.operator` for the caller process).
    Control {
        #[command(subcommand)]
        cmd: ControlCmd,
    },
}

/// Internal panel for `control desired-primary-set`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum DesiredPrimaryArg {
    #[value(name = "eDP-1")]
    Edp1,
    #[value(name = "eDP-2")]
    Edp2,
}

impl DesiredPrimaryArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::Edp1 => "eDP-1",
            Self::Edp2 => "eDP-2",
        }
    }
}

#[derive(Clone, Copy, Debug, ValueEnum)]
enum TabletMappingModeArg {
    #[value(name = "one-to-one")]
    OneToOne,
    #[value(name = "all-to-primary")]
    AllToPrimary,
}

impl TabletMappingModeArg {
    fn as_str(self) -> &'static str {
        match self {
            Self::OneToOne => "one_to_one",
            Self::AllToPrimary => "all_to_primary",
        }
    }
}

/// Backlight level for `control keyboard-backlight set`.
#[derive(Clone, Copy, Debug, ValueEnum)]
enum BacklightLevelArg {
    Off,
    Low,
    Medium,
    High,
}

impl BacklightLevelArg {
    fn to_u8(self) -> u8 {
        match self {
            Self::Off => 0,
            Self::Low => 1,
            Self::Medium => 2,
            Self::High => 3,
        }
    }
}

#[derive(Subcommand, Debug, Clone)]
enum ControlCmd {
    /// Toggle microphone mute
    MicMuteToggle,
    /// Set microphone mute on or off (`true` / `false`)
    MicMute {
        #[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
        on: bool,
    },
    /// Cycle keyboard backlight
    KeyboardBacklightToggle,
    /// Set keyboard backlight level
    KeyboardBacklightSet {
        #[arg(value_enum)]
        level: BacklightLevelArg,
    },
    /// Set desired primary internal panel (`eDP-1` or `eDP-2`); session reconciles with Mutter
    DesiredPrimarySet {
        #[arg(value_enum)]
        primary: DesiredPrimaryArg,
    },
    /// Enable or disable GNOME stylus → panel mapping
    TabletMappingEnable {
        #[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
        on: bool,
    },
    /// Stylus mapping mode: `one-to-one` (4447→eDP-1, 4448→eDP-2) or `all-to-primary`
    TabletMappingMode {
        #[arg(value_enum)]
        mode: TabletMappingModeArg,
    },
    /// Cycle stylus mapping mode (no-op when mapping disabled)
    TabletMappingModeToggle,
    /// Re-apply gsettings pen mapping now (session)
    TabletMappingApply,
    /// Toggle desired secondary (bottom) internal display
    SecondaryDisplayToggle,
    /// Enable or disable desired secondary display (`true` / `false`)
    SecondaryDisplay {
        #[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
        on: bool,
    },
    /// Same as `secondary-display`; name matches D-Bus `DesiredSecondaryEnabled`
    DesiredSecondaryEnabled {
        #[arg(action = clap::ArgAction::Set, value_parser = clap::value_parser!(bool))]
        on: bool,
    },
}

mod config;
mod ambient_light;
mod dbus_state;
mod polkit;
mod events;
mod idle_detection;
mod keyboard_bt;
mod keyboard_battery;
mod keyboard_battery_bt;
mod keyboard_usb;
mod mute_state;
mod secondary_display;
mod secondary_coordinator;
mod session;
mod state;
mod usb_keyboard_ports;
mod virtual_keyboard;

#[tokio::main(flavor = "current_thread")]
async fn main() {
    env_logger::init();

    let args = Args::parse();

    match args {
        Args::MigrateConfig { config_path } => {
            migrate_config(config_path.unwrap_or(PathBuf::from(DEFAULT_CONFIG_PATH))).await;
            return;
        }
        Args::Session => {
            session::run().await;
        }
        Args::Run { config_path } => {
            run_daemon(config_path.unwrap_or(PathBuf::from(DEFAULT_CONFIG_PATH))).await;
        }
        Args::ResumeDisplayApplies => {
            match dbus_state::resume_display_applies().await {
                Ok(()) => info!("Display apply guard resumed"),
                Err(e) => {
                    error!("Failed to resume display applies: {e}");
                    process::exit(1);
                }
            }
        }
        Args::State => {
            match dbus_state::query_root_state_for_cli().await {
                Ok(rows) => {
                    let width = rows
                        .iter()
                        .map(|(k, _)| k.len())
                        .max()
                        .unwrap_or(0);
                    for (k, v) in rows {
                        println!("{k:width$}  {v}", width = width);
                    }
                }
                Err(e) => {
                    error!("{e}");
                    process::exit(1);
                }
            }
        }
        Args::Control { cmd } => {
            let op = match cmd {
                ControlCmd::MicMuteToggle => OperatorCommand::ToggleMicMute,
                ControlCmd::MicMute { on } => OperatorCommand::SetMicMute(on),
                ControlCmd::KeyboardBacklightToggle => OperatorCommand::ToggleKeyboardBacklight,
                ControlCmd::KeyboardBacklightSet { level } => {
                    OperatorCommand::SetKeyboardBacklightLevel(level.to_u8())
                }
                ControlCmd::DesiredPrimarySet { primary } => {
                    OperatorCommand::SetDesiredPrimary(primary.as_str().to_string())
                }
                ControlCmd::TabletMappingEnable { on } => {
                    OperatorCommand::SetTabletMappingEnabled(on)
                }
                ControlCmd::TabletMappingMode { mode } => {
                    OperatorCommand::SetTabletMappingMode(mode.as_str().to_string())
                }
                ControlCmd::TabletMappingModeToggle => OperatorCommand::ToggleTabletMappingMode,
                ControlCmd::TabletMappingApply => OperatorCommand::ApplyTabletMapping,
                ControlCmd::SecondaryDisplayToggle => OperatorCommand::ToggleSecondaryDisplay,
                ControlCmd::SecondaryDisplay { on } | ControlCmd::DesiredSecondaryEnabled { on } => {
                    OperatorCommand::SetSecondaryDisplay(on)
                }
            };
            if let Err(e) = dbus_state::run_operator_command(op).await {
                error!("{e}");
                process::exit(1);
            }
        }
    }
}

async fn migrate_config(config_path: PathBuf) {
    use log::{info, warn};

    // Try to read the config
    match Config::try_read(&config_path).await {
        Ok(_) => {
            info!("Config file is valid, no migration needed");
        }
        Err(e) => {
            warn!("Failed to read config file: {}", e);

            // Backup the old config file if it exists
            if fs::try_exists(&config_path).await.unwrap_or(false) {
                let backup_path = config_path.with_file_name(format!(
                    "{}.bak",
                    config_path.file_name().unwrap().to_string_lossy()
                ));
                fs::rename(&config_path, &backup_path).await.unwrap();
                info!(
                    "\x1b[31mBacked up old config to: {} because it was incompatible with the new version\x1b[0m",
                    backup_path.display()
                );
            }

            // Write new default config
            Config::write_default_config(&config_path).await;
            info!(
                "Created new default config file at: {}",
                config_path.display()
            );
        }
    }
}

async fn run_daemon(config_path: PathBuf) {
    let config = Config::read(&config_path).await;
    usb_keyboard_ports::init_pogo_devpaths(&config.usb_keyboard_ports);
    usb_keyboard_ports::init_usb_keyboard_ids(config.vendor_id(), config.product_id());

    // Create event channel
    let (event_sender, _) = broadcast::channel::<Event>(64);
    
    // Create virtual keyboard
    let virtual_keyboard = Arc::new(Mutex::new(VirtualKeyboard::new(&config)));

    let (state_manager, activity_notifier, current_usb_keyboard) =
        if let Some(keyboard) = find_wired_keyboard(&config).await {
            let pogo_docked = usb_keyboard_ports::keyboard_on_pogo_dock_async(
                keyboard.bus_id(),
                keyboard.device_address(),
            )
            .await;
            let state_manager = KeyboardStateManager::new(
                true,
                pogo_docked,
                config.idle_timeout_seconds,
                config.ambient_keyboard_backlight_enabled,
                config.tablet.clone(),
                event_sender.clone(),
            )
            .await;
            let activity_notifier = start_idle_detection_task(&config, state_manager.clone());

            let current_usb_keyboard = start_usb_keyboard_task(
                &config,
                keyboard,
                event_sender.subscribe(),
                virtual_keyboard.clone(),
                state_manager.clone(),
                activity_notifier.clone(),
                Some(pogo_docked),
            )
            .await;
            (state_manager, activity_notifier, Some(current_usb_keyboard))
        } else {
            let state_manager = KeyboardStateManager::new(
                false,
                false,
                config.idle_timeout_seconds,
                config.ambient_keyboard_backlight_enabled,
                config.tablet.clone(),
                event_sender.clone(),
            )
            .await;
            let activity_notifier = start_idle_detection_task(&config, state_manager.clone());

            (state_manager, activity_notifier, None)
        };

    start_secondary_display_task(
        config.clone(),
        state_manager.clone(),
        event_sender.subscribe(),
    )
    .await;

    if let Err(e) = dbus_state::start_root_service(
        state_manager.clone(),
        config_path.clone(),
        config.clone(),
        activity_notifier.clone(),
    )
    .await
    {
        error!("Failed to start root D-Bus service: {e}");
        process::exit(1);
    }

    {
        let state_manager = state_manager.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = ambient_light::run(state_manager.clone()).await {
                    warn!("Ambient light monitor error: {e}");
                    state_manager.set_ambient_light_sensor_available(false).await;
                    state_manager.set_ambient_light_lux(None).await;
                }
                tokio::time::sleep(Duration::from_secs(5)).await;
            }
        });
    }

    if state_manager.keyboard_battery_percentage().is_some()
        && let Err(e) = dbus_state::notify_keyboard_battery_changed().await
    {
        warn!("Failed to publish keyboard battery loaded at startup: {e}");
    }

    start_bt_keyboard_monitor_task(
        &config,
        event_sender.clone(),
        virtual_keyboard.clone(),
        state_manager.clone(),
        activity_notifier.clone(),
    );

    start_bt_battery_monitor_task(state_manager.clone());

    start_usb_keyboard_monitor_task(
        &config,
        current_usb_keyboard,
        event_sender.clone(),
        virtual_keyboard.clone(),
        state_manager.clone(),
        activity_notifier.clone(),
    );

    start_listen_mute_state_thread(state_manager.clone());

    // Forward keyboard attachment state changes over D-Bus.
    // If no session is registered, or if it does not acknowledge in time, fall back to sysfs.
    {
        let mut event_rx = event_sender.subscribe();
        let state_manager_clone = state_manager.clone();
        tokio::spawn(async move {
            loop {
                match event_rx.recv().await {
                    Ok(Event::KeyboardPogoDocked(docked)) => {
                        info!("KeyboardPogoDocked event: {docked}");
                        secondary_display::pause_brightness_sync_for(Duration::from_secs(4));

                        // On undock, write sysfs `on` BEFORE telling the session daemon.
                        if !docked {
                            secondary_display::ensure_secondary_display_on().await;
                        }

                        let (session_registered, acked) =
                            dbus_state::notify_keyboard_pogo_docked_changed_wait(
                                docked,
                                Duration::from_secs(5),
                            )
                            .await;

                        if session_registered {
                            if acked {
                                info!("KeyboardPogoDocked: session daemon acknowledged request");
                            } else {
                                warn!(
                                    "KeyboardPogoDocked: no session daemon ack, falling back to sysfs"
                                );
                                secondary_display::arm_sysfs_fallback_once();
                                state_manager_clone.emit_secondary_display_state();
                            }
                        } else {
                            warn!(
                                "KeyboardPogoDocked: no registered session daemon, falling back to sysfs"
                            );
                            secondary_display::arm_sysfs_fallback_once();
                            state_manager_clone.emit_secondary_display_state();
                        }
                    }
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                }
            }
        });
    }

    panic::set_hook(Box::new(|info| {
        error!("Thread panicked: {info}");
        process::exit(1);
    }));

    info!("Daemon started");

    // Gracefully shutdown
    let mut sigterm = signal(SignalKind::terminate()).unwrap();
    let mut sigint = signal(SignalKind::interrupt()).unwrap();
    tokio::select! {
        _ = sigterm.recv() => {
            info!("SIGTERM received, shutting down");
        }
        _ = sigint.recv() => {
            info!("SIGINT received, shutting down");
        }
    }
    state_manager.suspend_start();
    tokio::time::sleep(Duration::from_millis(500)).await;
    process::exit(0);
}

pub fn parse_hex_string(hex_string: &str) -> Vec<u8> {
    let mut bytes = Vec::new();
    for i in (0..hex_string.len()).step_by(2) {
        bytes.push(u8::from_str_radix(&hex_string[i..i + 2], 16).unwrap());
    }
    bytes
}

pub fn execute_command(command: &str) {
    info!("Executing command: {}", command);
    let command = command.to_owned();
    tokio::spawn(async move {
        match tokio::process::Command::new("sh")
            .arg("-c")
            .arg(&command)
            .output()
            .await
        {
            Ok(output) => {
                info!(
                    "Command '{}' exited with status {}.\nstdout:\n{}\nstderr:\n{}",
                    command,
                    output.status,
                    String::from_utf8_lossy(&output.stdout).trim(),
                    String::from_utf8_lossy(&output.stderr).trim()
                );
            }
            Err(e) => {
                log::warn!("Failed to execute command '{}': {}", command, e);
            }
        }
    });
}
