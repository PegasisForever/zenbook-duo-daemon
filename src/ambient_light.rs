use futures::StreamExt;
use log::{info, warn};
use tokio::time::{Duration, Instant, MissedTickBehavior, interval_at, sleep_until, timeout};
use zbus::{Connection, proxy};

use crate::state::{KeyboardStateManager, LOW_LIGHT_KEYBOARD_BACKLIGHT_LUX_THRESHOLD};

const AMBIENT_REFRESH_SECS: u64 = 5;
const AMBIENT_CLAIM_TIMEOUT: Duration = Duration::from_secs(2);
const AMBIENT_STABILITY_WINDOW: Duration = Duration::from_secs(20);
const AMBIENT_STABILITY_DRIFT_RATIO: f64 = 0.10;
const AMBIENT_MIN_STABLE_DRIFT_LUX: f64 = 1.0;

#[proxy(
    interface = "net.hadess.SensorProxy",
    default_service = "net.hadess.SensorProxy",
    default_path = "/net/hadess/SensorProxy"
)]
trait SensorProxy {
    fn claim_light(&self) -> zbus::Result<()>;
    fn release_light(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn has_ambient_light(&self) -> zbus::Result<bool>;

    #[zbus(property)]
    fn light_level(&self) -> zbus::Result<f64>;
}

struct AmbientLightSmoother {
    stable_lux: f64,
    window_started_at: Option<Instant>,
    samples: Vec<f64>,
}

impl AmbientLightSmoother {
    fn new(initial_lux: f64) -> Self {
        Self {
            stable_lux: initial_lux,
            window_started_at: None,
            samples: Vec::new(),
        }
    }

    fn required_drift_lux(&self) -> f64 {
        (self.stable_lux.abs() * AMBIENT_STABILITY_DRIFT_RATIO).max(AMBIENT_MIN_STABLE_DRIFT_LUX)
    }

    fn should_promote_dark_immediately(&self, lux: f64) -> bool {
        self.stable_lux > LOW_LIGHT_KEYBOARD_BACKLIGHT_LUX_THRESHOLD
            && lux <= LOW_LIGHT_KEYBOARD_BACKLIGHT_LUX_THRESHOLD
    }

    fn observe_level(&mut self, lux: f64, now: Instant) -> Option<f64> {
        if self.should_promote_dark_immediately(lux) {
            self.window_started_at = None;
            self.samples.clear();
            self.stable_lux = lux;
            return Some(lux);
        }

        let meaningful_drift = (lux - self.stable_lux).abs() >= self.required_drift_lux();
        if self.has_pending_window() || meaningful_drift {
            self.record_sample(lux, now);
        }
        None
    }

    fn record_sample(&mut self, lux: f64, now: Instant) {
        if self.window_started_at.is_none() {
            self.window_started_at = Some(now);
        }
        self.samples.push(lux);
    }

    fn deadline(&self) -> Option<Instant> {
        self.window_started_at.map(|started| started + AMBIENT_STABILITY_WINDOW)
    }

    fn has_pending_window(&self) -> bool {
        self.window_started_at.is_some()
    }

    fn finish_window(&mut self, current_lux: f64) -> Option<f64> {
        if !self.has_pending_window() {
            return None;
        }

        self.samples.push(current_lux);
        let average = self.samples.iter().sum::<f64>() / self.samples.len() as f64;
        self.window_started_at = None;
        self.samples.clear();

        if (average - self.stable_lux).abs() < self.required_drift_lux() {
            return None;
        }

        self.stable_lux = average;
        Some(average)
    }
}

pub async fn run(state_manager: KeyboardStateManager) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::system().await?;
    let sensor = SensorProxyProxy::new(&conn).await?;

    if !sensor.has_ambient_light().await? {
        state_manager.set_ambient_light_sensor_available(false).await;
        info!("Ambient light sensor not available");
        std::future::pending::<()>().await;
        return Ok(());
    }

    match timeout(AMBIENT_CLAIM_TIMEOUT, sensor.claim_light()).await {
        Ok(Ok(())) => {}
        Ok(Err(e)) => warn!("Ambient light claim failed, continuing with direct reads: {e}"),
        Err(_) => warn!("Ambient light claim timed out, continuing with direct reads"),
    }
    state_manager.set_ambient_light_sensor_available(true).await;

    let initial = sensor.light_level().await?;
    state_manager.set_ambient_light_lux(Some(initial)).await;
    info!("Ambient light claimed; initial light level: {initial:.3} lux");

    let mut smoother = AmbientLightSmoother::new(initial);
    let mut light_level_changes = sensor.receive_light_level_changed().await;
    let mut refresh = interval_at(
        Instant::now() + Duration::from_secs(AMBIENT_REFRESH_SECS),
        Duration::from_secs(AMBIENT_REFRESH_SECS),
    );
    refresh.set_missed_tick_behavior(MissedTickBehavior::Delay);

    loop {
        if let Some(deadline) = smoother.deadline() {
            tokio::select! {
                change = light_level_changes.next() => {
                    if change.is_none() {
                        return Err("ambient light change stream ended".into());
                    }
                    let level = sensor.light_level().await?;
                    if let Some(stable_level) = smoother.observe_level(level, Instant::now()) {
                        state_manager.set_ambient_light_lux(Some(stable_level)).await;
                    }
                }
                _ = refresh.tick() => {
                    let level = sensor.light_level().await?;
                    if let Some(stable_level) = smoother.observe_level(level, Instant::now()) {
                        state_manager.set_ambient_light_lux(Some(stable_level)).await;
                    }
                }
                _ = sleep_until(deadline) => {
                    let level = sensor.light_level().await?;
                    if let Some(stable_level) = smoother.finish_window(level) {
                        state_manager.set_ambient_light_lux(Some(stable_level)).await;
                    }
                }
            }
        } else {
            tokio::select! {
                change = light_level_changes.next() => {
                    if change.is_none() {
                        return Err("ambient light change stream ended".into());
                    }
                    let level = sensor.light_level().await?;
                    if let Some(stable_level) = smoother.observe_level(level, Instant::now()) {
                        state_manager.set_ambient_light_lux(Some(stable_level)).await;
                    }
                }
                _ = refresh.tick() => {
                    let level = sensor.light_level().await?;
                    if let Some(stable_level) = smoother.observe_level(level, Instant::now()) {
                        state_manager.set_ambient_light_lux(Some(stable_level)).await;
                    }
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn smoother_waits_for_full_window() {
        let mut smoother = AmbientLightSmoother::new(100.0);
        assert_eq!(smoother.observe_level(85.0, Instant::now()), None);

        assert!(smoother.has_pending_window());
    }

    #[test]
    fn smoother_emits_after_sparse_large_drift() {
        let mut smoother = AmbientLightSmoother::new(100.0);
        smoother.record_sample(1.0, Instant::now());

        let stable = smoother.finish_window(1.0);

        assert_eq!(stable, Some(1.0));
    }

    #[test]
    fn smoother_promotes_dark_immediately() {
        let mut smoother = AmbientLightSmoother::new(100.0);

        let stable = smoother.observe_level(1.0, Instant::now());

        assert_eq!(stable, Some(1.0));
        assert!(!smoother.has_pending_window());
    }

    #[test]
    fn smoother_ignores_small_drift() {
        let mut smoother = AmbientLightSmoother::new(100.0);
        assert_eq!(smoother.observe_level(96.0, Instant::now()), None);

        let stable = smoother.finish_window(96.0);

        assert_eq!(stable, None);
    }
}
