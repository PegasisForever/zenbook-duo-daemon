use log::{info, warn};
use nix::sys::stat;
use nix::unistd;
use std::os::unix::fs::PermissionsExt as _;
use std::path::PathBuf;
use tokio::fs;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::Config;
use crate::idle_detection::ActivityNotifier;
use crate::state::{KeyboardBacklightState, KeyboardStateManager};

pub struct UnixPipe {
    reader: BufReader<File>,
    path: PathBuf,
}

impl UnixPipe {
    pub async fn new(path: &PathBuf) -> Self {
        if fs::try_exists(path).await.unwrap_or(false) {
            fs::remove_file(path).await.unwrap();
            info!("Removed existing pipe file");
        }

        // Create the FIFO
        unistd::mkfifo(path, stat::Mode::from_bits_truncate(0o666)).unwrap();

        // For some reason the permissions are not set correctly by mkfifo, so we set them manually
        let metadata = fs::metadata(path).await.unwrap();
        let mut permissions = metadata.permissions();
        permissions.set_mode(0o666);
        fs::set_permissions(path, permissions).await.unwrap();

        let file = File::open(path).await.unwrap();
        let reader = BufReader::new(file);
        Self {
            reader,
            path: path.clone(),
        }
    }

    /// Re-open the pipe file (called after EOF to wait for new writers)
    async fn reopen(&mut self) {
        let file = File::open(&self.path).await.unwrap();
        self.reader = BufReader::new(file);
    }

    /// Blocks until a command is received.
    /// If returns None, the pipe has been closed due to an error.
    pub async fn receive_next_command(&mut self) -> Option<String> {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line).await {
                Ok(0) => {
                    // EOF - all writers closed, re-open to wait for new writers
                    self.reopen().await;
                    continue;
                }
                Ok(_) => return Some(line.trim_end().to_string()),
                Err(_) => return None,
            }
        }
    }
}

pub fn start_receive_commands_task(
    config: &Config,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
) {
    let path = PathBuf::from(&config.pipe_path);
    tokio::spawn(async move {
        let mut pipe = UnixPipe::new(&path).await;
        loop {
            if let Some(line) = pipe.receive_next_command().await {
                info!("Received command: {}", line);
                match line.as_str() {
                    "suspend_start" => {
                        state_manager.suspend_start();
                    }
                    "suspend_end" => {
                        state_manager.suspend_end();
                        activity_notifier.notify();
                    }
                    "mic_mute_led_toggle" => {
                        state_manager.toggle_mic_mute_led();
                    }
                    "mic_mute_led_on" => {
                        state_manager.set_mic_mute_led(true);
                    }
                    "mic_mute_led_off" => {
                        state_manager.set_mic_mute_led(false);
                    }
                    "backlight_toggle" => {
                        state_manager.toggle_keyboard_backlight();
                    }
                    "backlight_off" => {
                        state_manager.set_keyboard_backlight(KeyboardBacklightState::Off);
                    }
                    "backlight_low" => {
                        state_manager.set_keyboard_backlight(KeyboardBacklightState::Low);
                    }
                    "backlight_medium" => {
                        state_manager.set_keyboard_backlight(KeyboardBacklightState::Medium);
                    }
                    "backlight_high" => {
                        state_manager.set_keyboard_backlight(KeyboardBacklightState::High);
                    }
                    "secondary_display_toggle" => {
                        state_manager.toggle_secondary_display();
                    }
                    "secondary_display_on" => {
                        state_manager.set_secondary_display(true);
                    }
                    "secondary_display_off" => {
                        state_manager.set_secondary_display(false);
                    }
                    _ => {
                        warn!("Unknown pipe command: {}", line);
                    }
                }
            } else {
                warn!("Pipe closed unexpectedly, recreating...");
                pipe = UnixPipe::new(&path).await;
            }
        }
    });
}
