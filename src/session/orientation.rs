use futures::StreamExt;
use log::{error, info};
use tokio::sync::broadcast;
use zbus::{Connection, proxy};

#[proxy(
    interface = "net.hadess.SensorProxy",
    default_service = "net.hadess.SensorProxy",
    default_path = "/net/hadess/SensorProxy"
)]
trait SensorProxy {
    fn claim_accelerometer(&self) -> zbus::Result<()>;
    fn release_accelerometer(&self) -> zbus::Result<()>;
    #[zbus(property)]
    fn has_accelerometer(&self) -> zbus::Result<bool>;
    #[zbus(property)]
    fn accelerometer_orientation(&self) -> zbus::Result<String>;
}

pub async fn current_orientation() -> Result<Option<String>, Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::system().await?;
    let sensor = SensorProxyProxy::new(&conn).await?;

    if !sensor.has_accelerometer().await? {
        return Ok(None);
    }

    Ok(Some(sensor.accelerometer_orientation().await?))
}

/// Connect to the system D-Bus, claim the accelerometer, and stream orientation changes.
/// Sends each new orientation string to `orient_tx`. Returns when the property stream ends.
///
/// Waits 10 seconds before claiming so Mutter's input mapper is fully initialized
/// before any ApplyMonitorsConfig calls are made.
///
/// Deduplicates: only sends when orientation actually changes, preventing double
/// ApplyMonitorsConfig calls that corrupt Mutter's transitional state.
pub async fn run(orient_tx: broadcast::Sender<String>) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Give Mutter time to fully initialize before we start issuing ApplyMonitorsConfig calls.
    // Without this, the input mapper inside libmutter crashes (SIGSEGV in
    // mapper_input_info_set_output) when called during early startup.
    tokio::time::sleep(std::time::Duration::from_secs(10)).await;
    let conn = Connection::system().await?;
    let sensor = SensorProxyProxy::new(&conn).await?;

    if !sensor.has_accelerometer().await? {
        info!("No accelerometer found — auto-rotate disabled");
        std::future::pending::<()>().await;
        return Ok(());
    }

    sensor.claim_accelerometer().await?;

    let initial = sensor.accelerometer_orientation().await?;
    info!("Accelerometer claimed; initial orientation: {initial}");
    // Send initial orientation only if non-trivial (normal is Mutter's default; sending it
    // immediately risks a double-apply if PropertyChanged fires right after).
    // We track last_sent to deduplicate rapid PropertyChanged events.
    let mut last_sent = initial.clone();
    if initial != "normal" {
        orient_tx.send(initial).ok();
    }

    let mut changes = sensor.receive_accelerometer_orientation_changed().await;
    while let Some(change) = changes.next().await {
        match change.get().await {
            Ok(orientation) => {
                if orientation == last_sent {
                    continue; // deduplicate: don't apply the same orientation twice
                }
                info!("Orientation: {orientation}");
                last_sent = orientation.clone();
                orient_tx.send(orientation).ok();
            }
            Err(e) => error!("Failed to read orientation change: {e}"),
        }
    }

    sensor.release_accelerometer().await.ok();
    info!("Accelerometer released");
    Ok(())
}
