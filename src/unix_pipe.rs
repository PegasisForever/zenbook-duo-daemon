use log::{info, warn};
use nix::sys::stat;
use nix::unistd;
use std::path::PathBuf;
use tokio::fs;
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::sync::broadcast;

use crate::config::Config;
use crate::events::Event;
use crate::state::BacklightState;

pub struct UnixPipe {
    reader: BufReader<File>,
}

impl UnixPipe {
    pub async fn new(path: &PathBuf) -> Self {
        if fs::try_exists(path).await.unwrap_or(false) {
            fs::remove_file(path).await.unwrap();
            info!("Removed existing pipe file");
        }

        // Create the FIFO (mode 0666)
        unistd::mkfifo(
            path,
            stat::Mode::S_IRUSR
                | stat::Mode::S_IWUSR
                | stat::Mode::S_IRGRP
                | stat::Mode::S_IWGRP
                | stat::Mode::S_IROTH
                | stat::Mode::S_IWOTH,
        )
        .unwrap();

        let file = File::open(path).await.unwrap();
        let reader = BufReader::new(file);
        Self { reader }
    }

    /// Blocks until a command is received.
    /// If returns None, the pipe has been closed.
    pub async fn receive_next_command(&mut self) -> Option<String> {
        loop {
            let mut line = String::new();
            match self.reader.read_line(&mut line).await {
                Ok(0) => {
                    // EOF
                    continue;
                }
                Ok(_) => return Some(line.trim_end().to_string()),
                Err(_) => return None,
            }
        }
    }
}

pub fn start_receive_commands_task(config: &Config, event_sender: broadcast::Sender<Event>) {
    let path = PathBuf::from(&config.pipe_path);
    tokio::spawn(async move {
        let mut pipe = UnixPipe::new(&path).await;
        loop {
            if let Some(line) = pipe.receive_next_command().await {
                match line.as_str() {
                    "suspend" => {
                        event_sender.send(Event::LaptopSuspend).ok();
                    }
                    "resume" => {
                        event_sender.send(Event::LaptopResume).ok();
                    }
                    "mic_mute_led_toggle" => {
                        event_sender.send(Event::MicMuteLedToggle).ok();
                    }
                    "mic_mute_led_on" => {
                        event_sender.send(Event::MicMuteLed(true)).ok();
                    }
                    "mic_mute_led_off" => {
                        event_sender.send(Event::MicMuteLed(false)).ok();
                    }
                    "backlight_toggle" => {
                        event_sender.send(Event::BacklightToggle).ok();
                    }
                    "backlight_off" => {
                        event_sender
                            .send(Event::Backlight(BacklightState::Off))
                            .ok();
                    }
                    "backlight_low" => {
                        event_sender
                            .send(Event::Backlight(BacklightState::Low))
                            .ok();
                    }
                    "backlight_medium" => {
                        event_sender
                            .send(Event::Backlight(BacklightState::Medium))
                            .ok();
                    }
                    "backlight_high" => {
                        event_sender
                            .send(Event::Backlight(BacklightState::High))
                            .ok();
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
