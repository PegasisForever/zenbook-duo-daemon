use std::process::Stdio;
use std::time::Duration;

use log::{info, warn};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::Command;
use tokio::sync::{broadcast, mpsc};

const AMBIENT_SCHEMA: &str = "org.gnome.settings-daemon.plugins.power";
const AMBIENT_KEY: &str = "ambient-enabled";

fn parse_bool(value: &str) -> Option<bool> {
    match value.trim().trim_matches('\'') {
        "true" => Some(true),
        "false" => Some(false),
        _ => None,
    }
}

async fn read_ambient_enabled() -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    let output = Command::new("gsettings")
        .args(["get", AMBIENT_SCHEMA, AMBIENT_KEY])
        .output()
        .await?;
    if !output.status.success() {
        return Err(format!("gsettings get failed with status {}", output.status).into());
    }

    parse_bool(&String::from_utf8_lossy(&output.stdout))
        .ok_or_else(|| "failed to parse gsettings ambient-enabled value".into())
}

async fn set_ambient_enabled(enabled: bool) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let value = if enabled { "true" } else { "false" };
    let status = Command::new("gsettings")
        .args(["set", AMBIENT_SCHEMA, AMBIENT_KEY, value])
        .status()
        .await?;
    if !status.success() {
        return Err(format!("gsettings set failed with status {}", status).into());
    }
    Ok(())
}

pub async fn run(
    mut desired_rx: broadcast::Receiver<bool>,
    ambient_report_tx: mpsc::Sender<bool>,
) {
    let mut current = match read_ambient_enabled().await {
        Ok(value) => {
            let _ = ambient_report_tx.send(value).await;
            Some(value)
        }
        Err(e) => {
            warn!("Ambient: failed to read initial ambient-enabled value: {e}");
            None
        }
    };

    loop {
        let mut child = match Command::new("gsettings")
            .args(["monitor", AMBIENT_SCHEMA, AMBIENT_KEY])
            .stdout(Stdio::piped())
            .spawn()
        {
            Ok(child) => child,
            Err(e) => {
                warn!("Ambient: failed to start gsettings monitor: {e}");
                tokio::time::sleep(Duration::from_secs(2)).await;
                continue;
            }
        };

        let Some(stdout) = child.stdout.take() else {
            warn!("Ambient: gsettings monitor stdout unavailable");
            let _ = child.kill().await;
            tokio::time::sleep(Duration::from_secs(1)).await;
            continue;
        };

        let mut lines = BufReader::new(stdout).lines();

        loop {
            tokio::select! {
                line = lines.next_line() => {
                    match line {
                        Ok(Some(line)) => {
                            if let Some(value) = parse_bool(line.split_whitespace().last().unwrap_or_default()) {
                                current = Some(value);
                                let _ = ambient_report_tx.send(value).await;
                                info!("Ambient: GNOME ambient-enabled changed to {}", value);
                            }
                        }
                        Ok(None) => break,
                        Err(e) => {
                            warn!("Ambient: failed to read gsettings monitor output: {e}");
                            break;
                        }
                    }
                }
                msg = desired_rx.recv() => match msg {
                    Ok(desired) => {
                        if current != Some(desired) {
                            match set_ambient_enabled(desired).await {
                                Ok(()) => {
                                    current = Some(desired);
                                    info!("Ambient: set GNOME ambient-enabled to {}", desired);
                                }
                                Err(e) => warn!("Ambient: failed to set GNOME ambient-enabled to {}: {}", desired, e),
                            }
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(_)) => {}
                    Err(broadcast::error::RecvError::Closed) => return,
                }
            }
        }

        let _ = child.kill().await;
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}
