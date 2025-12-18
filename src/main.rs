use std::panic;
use std::{
    path::PathBuf,
    process,
    sync::{Arc, Mutex},
    thread,
};

use crate::{
    config::{Config, DEFAULT_CONFIG_PATH},
    consumers::{suspend_resume_consumer, virtual_keyboard_consumer},
    events::{EventBus, KeyPressEventBus},
    secondary_display::{secondary_display_consumer, sync_secondary_display_brightness_thread},
    state::{BacklightState, KeyboardStateManager},
    unix_pipe::{DEFAULT_PIPE_PATH, receive_commands_thread},
    virtual_keyboard::VirtualKeyboard,
    wired_keyboard_thread::{
        find_wired_keyboard, monitor_usb_keyboard_hotplug, wired_keyboard_thread,
    },
};
use bt_keyboard_thread::bt_input_monitor_thread;
use clap::Parser;
use log::{error, info};

#[derive(Parser, Debug)]
#[command(version, about, long_about = None)]
enum Args {
    /// Run the daemon
    Run {
        /// Path to the config file, defaults to /etc/zenbook-duo-daemon/config.toml
        #[arg(short, long)]
        config_path: Option<PathBuf>,
    },
    /// Migrate config file - backs up old config and writes new default if read fails
    MigrateConfig {
        /// Path to the config file, defaults to /etc/zenbook-duo-daemon/config.toml
        #[arg(short, long)]
        config_path: Option<PathBuf>,
    },
}

mod bt_keyboard_thread;
mod config;
mod consumers;
mod events;
mod secondary_display;
mod state;
mod unix_pipe;
mod virtual_keyboard;
mod wired_keyboard_thread;

fn main() {
    env_logger::init();

    let args = Args::parse();

    match args {
        Args::MigrateConfig { config_path } => {
            migrate_config(config_path.unwrap_or(PathBuf::from(DEFAULT_CONFIG_PATH)));
            return;
        }
        Args::Run { config_path } => {
            run_daemon(config_path.unwrap_or(PathBuf::from(DEFAULT_CONFIG_PATH)));
        }
    }
}

fn migrate_config(config_path: PathBuf) {
    use log::{info, warn};
    use std::fs;

    // Try to read the config
    match Config::try_read(&config_path) {
        Ok(_) => {
            info!("Config file is valid, no migration needed");
        }
        Err(e) => {
            warn!("Failed to read config file: {}", e);

            // Backup the old config file if it exists
            if config_path.exists() {
                let backup_path = config_path.with_file_name(format!(
                    "{}.bak",
                    config_path.file_name().unwrap().to_string_lossy()
                ));
                fs::rename(&config_path, &backup_path).unwrap();
                info!("Backed up old config to: {}", backup_path.display());
            }

            // Write new default config
            Config::write_default_config(&config_path);
            info!(
                "Created new default config file at: {}",
                config_path.display()
            );
        }
    }
}

fn run_daemon(config_path: PathBuf) {
    let config = Config::read(&config_path);

    // Create event buses
    let key_press_event_bus = KeyPressEventBus::new();
    let event_bus = EventBus::new();

    // Create virtual keyboard
    let virtual_keyboard = Arc::new(Mutex::new(VirtualKeyboard::new(&config)));

    let state_manager = if let Some(keyboard) = find_wired_keyboard(&config) {
        let state_manager = KeyboardStateManager::new(true);
        wired_keyboard_thread(
            &config,
            keyboard,
            event_bus.sender(),
            key_press_event_bus.sender(),
            event_bus.receiver(),
            state_manager.clone(),
        );
        state_manager
    } else {
        KeyboardStateManager::new(false)
    };

    // Start secondary display brightness sync thread
    sync_secondary_display_brightness_thread(config.clone());

    // Start event consumers
    suspend_resume_consumer(
        state_manager.clone(),
        event_bus.receiver(),
        event_bus.sender(),
    );
    virtual_keyboard_consumer(
        config.clone(),
        virtual_keyboard.clone(),
        key_press_event_bus.receiver(),
        event_bus.sender(),
    );
    secondary_display_consumer(config.clone(), state_manager.clone(), event_bus.receiver());

    // Start Bluetooth keyboard monitor thread (producer)
    {
        let config = config.clone();
        let key_press_event_sender = key_press_event_bus.sender();
        let event_receiver = event_bus.receiver();
        let state_manager = state_manager.clone();
        thread::spawn(move || {
            bt_input_monitor_thread(
                &config,
                key_press_event_sender,
                event_receiver,
                state_manager,
            );
        });
    }

    {
        let config = config.clone();
        let event_sender = event_bus.sender();
        let event_receiver = event_bus.receiver();
        let key_press_event_sender = key_press_event_bus.sender();
        let state_manager = state_manager.clone();
        thread::spawn(move || {
            monitor_usb_keyboard_hotplug(
                config,
                event_sender,
                event_receiver,
                key_press_event_sender,
                state_manager,
            );
        });
    }

    receive_commands_thread(&PathBuf::from(DEFAULT_PIPE_PATH), event_bus.sender());

    panic::set_hook(Box::new(|info| {
        error!("Thread panicked: {info}");
        process::exit(1);
    }));

    loop {
        thread::park();
    }
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
    thread::spawn(
        move || match process::Command::new("sh").arg("-c").arg(&command).output() {
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
        },
    );
}
