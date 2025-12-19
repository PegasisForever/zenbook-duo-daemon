use std::panic;
use std::{path::PathBuf, process, sync::Arc};

use tokio::fs;
use tokio::sync::{Mutex, broadcast};

use crate::{
    config::{Config, DEFAULT_CONFIG_PATH},
    events::Event,
    idle_detection::start_idle_detection_task,
    keyboard_usb::{find_wired_keyboard, start_usb_keyboard_monitor_task, start_usb_keyboard_task},
    secondary_display::start_secondary_display_task,
    state::{KeyboardBacklightState, KeyboardStateManager},
    unix_pipe::start_receive_commands_task,
    virtual_keyboard::VirtualKeyboard,
};
use clap::Parser;
use keyboard_bt::start_bt_keyboard_monitor_task;
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

mod config;
mod events;
mod idle_detection;
mod keyboard_bt;
mod keyboard_usb;
mod secondary_display;
mod state;
mod unix_pipe;
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
        Args::Run { config_path } => {
            run_daemon(config_path.unwrap_or(PathBuf::from(DEFAULT_CONFIG_PATH))).await;
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
                info!("Backed up old config to: {}", backup_path.display());
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

    // Create event channel
    let (event_sender, _) = broadcast::channel::<Event>(64);

    // Create virtual keyboard
    let virtual_keyboard = Arc::new(Mutex::new(VirtualKeyboard::new(&config)));

    let (state_manager, activity_notifier, current_usb_keyboard) =
        if let Some(keyboard) = find_wired_keyboard(&config).await {
            let state_manager = KeyboardStateManager::new(true, event_sender.clone());
            let activity_notifier = start_idle_detection_task(&config, state_manager.clone());

            let current_usb_keyboard = start_usb_keyboard_task(
                &config,
                keyboard,
                event_sender.subscribe(),
                virtual_keyboard.clone(),
                state_manager.clone(),
                activity_notifier.clone(),
            )
            .await;
            (state_manager, activity_notifier, Some(current_usb_keyboard))
        } else {
            let state_manager = KeyboardStateManager::new(false, event_sender.clone());
            let activity_notifier = start_idle_detection_task(&config, state_manager.clone());

            (state_manager, activity_notifier, None)
        };

    start_secondary_display_task(
        config.clone(),
        state_manager.clone(),
        event_sender.subscribe(),
    )
    .await;

    start_bt_keyboard_monitor_task(
        &config,
        event_sender.clone(),
        virtual_keyboard.clone(),
        state_manager.clone(),
        activity_notifier.clone(),
    );

    start_usb_keyboard_monitor_task(
        &config,
        current_usb_keyboard,
        event_sender.clone(),
        virtual_keyboard.clone(),
        state_manager.clone(),
        activity_notifier.clone(),
    );

    start_receive_commands_task(&config, state_manager.clone(), activity_notifier.clone());

    panic::set_hook(Box::new(|info| {
        error!("Thread panicked: {info}");
        process::exit(1);
    }));

    info!("Daemon started");

    // Keep the main task alive
    loop {
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
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
