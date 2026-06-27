mod ambient;
pub(crate) mod display;
mod display_mode;
mod notifications;
mod orientation;
mod tablet_mapping;
// mod osk; // OSK support not yet implemented, kept for future use

use log::{error, warn};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::broadcast;

use crate::config::{Config, TabletMappingConfig};

pub async fn run() {
    let (kb_usb_tx, _) = broadcast::channel::<bool>(8);
    let (kb_pogo_tx, _) = broadcast::channel::<bool>(8);
    let (orient_tx, _) = broadcast::channel::<String>(8);
    let (desired_secondary_tx, _) = broadcast::channel::<bool>(8);
    let (desired_primary_tx, _) = broadcast::channel::<String>(8);
    let (desired_ambient_tx, _) = broadcast::channel::<bool>(8);

    let (kb_result_tx, kb_result_rx) = tokio::sync::mpsc::channel(8);
    let kb_result_rx = Arc::new(tokio::sync::Mutex::new(kb_result_rx));

    let (secondary_result_tx, secondary_result_rx) = tokio::sync::mpsc::channel(8);
    let secondary_result_rx = Arc::new(tokio::sync::Mutex::new(secondary_result_rx));
    let (ambient_report_tx, ambient_report_rx) = tokio::sync::mpsc::channel(8);

    let desired_primary = Arc::new(tokio::sync::RwLock::new(String::from("eDP-1")));
    let desired_secondary = Arc::new(tokio::sync::RwLock::new(true));

    let tablet_defaults = match Config::try_read(&PathBuf::from(crate::config::DEFAULT_CONFIG_PATH)).await
    {
        Ok(c) => c.tablet,
        Err(e) => {
            warn!(
                "Session: could not read {} for [tablet] settings (using defaults): {e}",
                crate::config::DEFAULT_CONFIG_PATH
            );
            TabletMappingConfig::default()
        }
    };
    let tablet_config = Arc::new(tokio::sync::RwLock::new(tablet_defaults));
    let (tablet_remap_tx, tablet_remap_rx) = broadcast::channel::<()>(8);

    let session_id = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos() as u64;

    {
        let tx = orient_tx.clone();
        tokio::spawn(async move {
            loop {
                if let Err(e) = orientation::run(tx.clone()).await {
                    error!("Orientation monitor error: {e}");
                }
                tokio::time::sleep(std::time::Duration::from_secs(5)).await;
            }
        });
    }

    tokio::spawn(display::run(
        orient_tx.subscribe(),
        kb_pogo_tx.subscribe(),
        desired_secondary_tx.subscribe(),
        desired_primary_tx.subscribe(),
        secondary_result_tx.clone(),
        kb_result_tx.clone(),
        desired_primary.clone(),
        desired_secondary.clone(),
        Arc::clone(&tablet_config),
        tablet_remap_rx,
    ));
    tokio::spawn(notifications::run(kb_usb_tx.subscribe(), kb_pogo_tx.subscribe()));
    tokio::spawn(notifications::run_battery_monitor());
    tokio::spawn(ambient::run(
        desired_ambient_tx.subscribe(),
        ambient_report_tx,
    ));

    {
        let desired_primary_clone = Arc::clone(&desired_primary);
        let desired_secondary_clone = Arc::clone(&desired_secondary);
        let desired_primary_tx = desired_primary_tx.clone();
        let desired_secondary_tx = desired_secondary_tx.clone();
        let kb_usb_tx_clone = kb_usb_tx.clone();
        let kb_pogo_tx_clone = kb_pogo_tx.clone();
        let secondary_result_rx = Arc::clone(&secondary_result_rx);
        let kb_result_rx = Arc::clone(&kb_result_rx);
        let tablet_config_clone = Arc::clone(&tablet_config);
        let tablet_remap_tx_clone = tablet_remap_tx.clone();
        tokio::spawn(async move {
            crate::dbus_state::run_session_client(
                session_id,
                desired_primary_clone,
                desired_secondary_clone,
                desired_primary_tx,
                desired_secondary_tx,
                kb_usb_tx_clone,
                kb_pogo_tx_clone,
                secondary_result_rx,
                kb_result_rx,
                ambient_report_rx,
                tablet_config_clone,
                tablet_remap_tx_clone,
            )
            .await;
        });
    }

    std::future::pending::<()>().await;
}
