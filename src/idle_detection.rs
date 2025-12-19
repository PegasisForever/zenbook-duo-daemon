use std::{path::PathBuf, sync::Arc, time::Duration};

use evdev_rs::{Device, DeviceWrapper as _, ReadFlag};
use futures::stream::StreamExt;
use inotify::{Inotify, WatchMask};
use log::{debug, info, warn};
use nix::libc;
use tokio::{
    fs,
    sync::mpsc,
    task::spawn_blocking,
    time::{Instant, sleep},
};

use crate::{config::Config, state::KeyboardStateManager};

/// Handle to notify the idle detection system of activity.
/// Clone this to share across multiple components.
#[derive(Clone)]
pub struct ActivityNotifier {
    tx: mpsc::UnboundedSender<()>,
}

impl ActivityNotifier {
    /// Notify that activity occurred, resetting the idle timer.
    /// If the system was idle, this will trigger `idle_end`.
    pub fn notify(&self) {
        let _ = self.tx.send(());
    }
}

/// Starts the idle detection task that monitors keyboard activity.
/// Returns an `ActivityNotifier` that can be used to reset the idle timer from other code.
/// Returns `None` if idle detection is disabled (idle_timeout_seconds = 0).
pub fn start_idle_detection_task(
    config: &Config,
    state_manager: KeyboardStateManager,
) -> ActivityNotifier {
    let idle_timeout = Duration::from_secs(config.idle_timeout_seconds);

    // Channel for activity notifications
    let (activity_tx, activity_rx) = mpsc::unbounded_channel::<()>();

    let notifier = ActivityNotifier {
        tx: activity_tx.clone(),
    };

    if config.idle_timeout_seconds == 0 {
        info!("Idle detection disabled (idle_timeout_seconds = 0)");
        return notifier;
    }

    // Spawn the idle state manager task
    tokio::spawn(async move {
        idle_state_task(idle_timeout, activity_rx, state_manager).await;
    });

    // Spawn the device monitor task
    tokio::spawn(async move {
        device_monitor_task(activity_tx).await;
    });

    notifier
}

/// Task that manages idle state based on activity events
async fn idle_state_task(
    idle_timeout: Duration,
    mut activity_rx: mpsc::UnboundedReceiver<()>,
    state_manager: KeyboardStateManager,
) {
    let mut is_idle = false;
    let mut last_activity = Instant::now();

    loop {
        let time_until_idle = idle_timeout.saturating_sub(last_activity.elapsed());

        tokio::select! {
            // Wait for activity notification
            result = activity_rx.recv() => {
                match result {
                    Some(()) => {
                        last_activity = Instant::now();
                        if is_idle {
                            debug!("Idle ended");
                            state_manager.idle_end();
                            is_idle = false;
                        }
                    }
                    None => {
                        // Channel closed, all senders dropped
                        info!("Activity channel closed, stopping idle detection");
                        return;
                    }
                }
            }
            // Wait for idle timeout
            _ = sleep(time_until_idle), if !is_idle => {
                debug!("Idle detected");
                state_manager.idle_start();
                is_idle = true;
            }
        }
    }
}

/// Task that monitors /dev/input/ for keyboard devices and spawns listeners
async fn device_monitor_task(activity_tx: mpsc::UnboundedSender<()>) {
    // Check existing devices
    let mut entries = match fs::read_dir("/dev/input").await {
        Ok(entries) => entries,
        Err(e) => {
            warn!("Failed to read /dev/input: {}", e);
            return;
        }
    };

    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        try_start_keyboard_listener(&path, activity_tx.clone()).await;
    }

    // Watch for new devices using inotify
    let inotify = Inotify::init().expect("Failed to initialize inotify for idle detection");
    inotify
        .watches()
        .add("/dev/input/", WatchMask::CREATE)
        .expect("Failed to add inotify watch for idle detection");

    let mut buffer = [0; 1024];
    let mut stream = inotify.into_event_stream(&mut buffer).unwrap();

    while let Some(event_result) = stream.next().await {
        if let Ok(event) = event_result {
            if let Some(name) = event.name {
                if event.mask.contains(inotify::EventMask::CREATE) {
                    if name.to_str().unwrap_or("").starts_with("event") {
                        let path = PathBuf::from("/dev/input/").join(name);
                        try_start_keyboard_listener(&path, activity_tx.clone()).await;
                    }
                }
            }
        }
    }
}

/// Attempts to start a keyboard listener for the given device path
async fn try_start_keyboard_listener(path: &PathBuf, activity_tx: mpsc::UnboundedSender<()>) {
    // Check if path is a directory
    if let Ok(metadata) = fs::metadata(&path).await {
        if metadata.is_dir() {
            return;
        }
    } else {
        return;
    }

    // Only process event files
    if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
        if !fname.starts_with("event") {
            return;
        }
    } else {
        return;
    }

    // Open the device in a blocking context
    let path_clone = path.clone();
    let device_result = spawn_blocking(move || {
        let file = match std::fs::File::open(&path_clone) {
            Ok(f) => f,
            Err(_) => return None,
        };
        match Device::new_from_file(file) {
            Ok(d) => Some(d),
            Err(_) => None,
        }
    })
    .await;

    let device = match device_result {
        Ok(Some(d)) => d,
        _ => return,
    };

    // Check if device name contains "ASUS Zenbook Duo Keyboard"
    let device_name = device.name().unwrap_or("");
    if !device_name.contains("ASUS Zenbook Duo Keyboard") {
        return;
    }

    info!(
        "Starting idle detection listener on {} ({})",
        path.display(),
        device_name
    );

    start_keyboard_listener(path.clone(), device, activity_tx);
}

/// Spawns a task that listens to events from a keyboard device
fn start_keyboard_listener(path: PathBuf, device: Device, activity_tx: mpsc::UnboundedSender<()>) {
    let device = Arc::new(std::sync::Mutex::new(device));

    tokio::spawn(async move {
        loop {
            let device_clone = device.clone();

            // Run the blocking evdev read in a blocking thread
            let result = spawn_blocking(move || {
                let dev = device_clone.lock().unwrap();
                dev.next_event(ReadFlag::NORMAL | ReadFlag::BLOCKING)
            })
            .await;

            match result {
                Ok(Ok((_status, _event))) => {
                    // Notify of activity
                    if activity_tx.send(()).is_err() {
                        // Receiver dropped, stop listening
                        return;
                    }
                    debug!("Activity detected on {}", path.display());
                }
                Ok(Err(e)) => {
                    if let Some(libc::ENODEV) = e.raw_os_error() {
                        info!(
                            "Keyboard device {} disconnected. Stopping idle listener.",
                            path.display()
                        );
                        return;
                    } else {
                        warn!("Failed to read event from {}: {:?}", path.display(), e);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
                Err(e) => {
                    warn!("Spawn blocking failed for {}: {:?}", path.display(), e);
                    return;
                }
            }
        }
    });
}
