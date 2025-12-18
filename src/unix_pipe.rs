use log::{info, warn};
use nix::sys::stat;
use nix::unistd;
use std::fs::File;
use std::io::{BufRead, BufReader, Lines};
use std::path::PathBuf;
use std::thread;
use std::sync::mpmc;

use crate::config::Config;
use crate::events::Event;
use crate::state::BacklightState;

pub struct UnixPipe {
    lines: Lines<BufReader<File>>,
}

impl UnixPipe {
    pub fn new(path: &PathBuf) -> Self {
        if path.exists() {
            std::fs::remove_file(path).unwrap();
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

        let file = File::open(path).unwrap();
        let reader = BufReader::new(file);
        let lines = reader.lines();
        Self { lines }
    }

    /// Blocks until a command is received.
    /// If returns None, the pipe has been closed.
    pub fn receive_next_command(&mut self) -> Option<String> {
        self.lines.next().and_then(|r| r.ok())
    }
}

pub fn start_receive_commands_thread(
    config: &Config,
    event_sender: mpmc::Sender<Event>,
) {
    let path = PathBuf::from(&config.pipe_path);
    thread::spawn(move || {
        let mut pipe = UnixPipe::new(&path);
        loop {
            if let Some(line) = pipe.receive_next_command() {
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
                pipe = UnixPipe::new(&path);
            }
        }
    });
}
