use std::panic;
use std::{
    path::PathBuf,
    process,
    sync::{Arc, Mutex},
    thread,
};

use crate::{
    config::{Config, DEFAULT_CONFIG_PATH},
    consumers::start_suspend_resume_control_thread,
    events::EventBus,
    secondary_display::start_secondary_display_thread,
    state::{BacklightState, KeyboardStateManager},
    unix_pipe::start_receive_commands_thread,
    virtual_keyboard::VirtualKeyboard,
    keyboard_usb::{
        find_wired_keyboard, start_usb_keyboard_monitor_thread, start_wired_keyboard_thread,
    },
};
use keyboard_bt::start_bt_keyboard_monitor_thread;
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

mod keyboard_bt;
mod config;
mod consumers;
mod events;
mod secondary_display;
mod state;
mod unix_pipe;
mod virtual_keyboard;
mod keyboard_usb;

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

    // Create event bus
    let event_bus = EventBus::new();

    // Create virtual keyboard
    let virtual_keyboard = Arc::new(Mutex::new(VirtualKeyboard::new(&config)));

    let state_manager = if let Some(keyboard) = find_wired_keyboard(&config) {
        let state_manager = KeyboardStateManager::new(true);
        start_wired_keyboard_thread(
            &config,
            keyboard,
            event_bus.sender(),
            event_bus.receiver(),
            virtual_keyboard.clone(),
            state_manager.clone(),
        );
        state_manager
    } else {
        KeyboardStateManager::new(false)
    };

    start_suspend_resume_control_thread(
        state_manager.clone(),
        event_bus.receiver(),
        event_bus.sender(),
    );
    start_secondary_display_thread(config.clone(), state_manager.clone(), event_bus.receiver());

    start_bt_keyboard_monitor_thread(
        &config,
        event_bus.sender(),
        event_bus.receiver(),
        virtual_keyboard.clone(),
        state_manager.clone(),
    );

    start_usb_keyboard_monitor_thread(
        &config,
        event_bus.sender(),
        event_bus.receiver(),
        virtual_keyboard.clone(),
        state_manager.clone(),
    );

    start_receive_commands_thread(&config, event_bus.sender());

    panic::set_hook(Box::new(|info| {
        error!("Thread panicked: {info}");
        process::exit(1);
    }));

    info!("Daemon started");

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
