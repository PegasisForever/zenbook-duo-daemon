use log::{info, warn};
use tokio::sync::broadcast;

/// Toggle the GNOME on-screen keyboard via gsettings.
pub async fn set_osk_enabled(enabled: bool) {
    let value = if enabled { "true" } else { "false" };
    match tokio::process::Command::new("gsettings")
        .args(["set", "org.gnome.desktop.a11y.applications", "screen-keyboard-enabled", value])
        .output()
        .await
    {
        Ok(out) if out.status.success() => {
            info!("OSK {}", if enabled { "enabled" } else { "disabled" });
        }
        Ok(out) => {
            warn!("gsettings OSK toggle failed: {}", String::from_utf8_lossy(&out.stderr).trim());
        }
        Err(e) => {
            warn!("Failed to run gsettings: {e}");
        }
    }
}

pub async fn run(mut kb_rx: broadcast::Receiver<bool>) {
    loop {
        match kb_rx.recv().await {
            // OSK enabled when no keyboard is attached
            Ok(attached) => set_osk_enabled(!attached).await,
            Err(broadcast::error::RecvError::Lagged(_)) => continue,
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
}
