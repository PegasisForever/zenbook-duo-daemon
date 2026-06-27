//! GNOME Wayland tablet-to-output mapping for Zenbook Duo integrated pen devices.
//!
//! On **UX8406**-class machines the integrated Elan pen stack is typically exposed as two USB
//! tablet nodes (**`04f3:4447`**, **`04f3:4448`**). GNOME stores per-device mapping under the
//! relocatable schema
//! `org.gnome.desktop.peripherals.tablet:/org/gnome/desktop/peripherals/tablets/<vid>:<pid>/`
//! key **`output`**, as an `as` GVariant:
//! `['VENDOR','PRODUCT','SERIAL','CONNECTOR']` from Mutter `GetCurrentState`.
//! Do **not** rewrite tablet geometry keys (`area`, `keep-aspect`) here; those may include
//! user calibration from GNOME Settings.

use log::{info, warn};

use crate::config::{TabletMapMode, TabletMappingConfig};
use crate::session::display::PhysicalMonitor;

const TABLET_A: &str = "04f3:4447";
const TABLET_B: &str = "04f3:4448";

pub(crate) fn output_target_for_connector(
    physical: &[PhysicalMonitor],
    connector: &str,
) -> Option<(String, String, String, String)> {
    for ((conn, vendor, product, serial), _, _) in physical {
        if conn == connector {
            return Some((vendor.clone(), product.clone(), serial.clone(), conn.clone()));
        }
    }
    None
}

fn gvariant_output(va: &str, vb: &str, vc: &str, vd: &str) -> String {
    let esc = |s: &str| s.replace('\\', "\\\\").replace('\'', "\\'");
    format!("['{}','{}','{}','{}']", esc(va), esc(vb), esc(vc), esc(vd))
}

async fn gsettings_set_tablet_key(usb_id: &str, key: &str, value: &str) -> Result<(), String> {
    let schema_path = format!(
        "org.gnome.desktop.peripherals.tablet:/org/gnome/desktop/peripherals/tablets/{usb_id}/"
    );
    let out = tokio::process::Command::new("gsettings")
        .args(["set", &schema_path, key, value])
        .output()
        .await
        .map_err(|e| format!("gsettings spawn: {e}"))?;
    if !out.status.success() {
        let stderr = String::from_utf8_lossy(&out.stderr);
        return Err(format!(
            "gsettings exit {:?}: {}",
            out.status.code(),
            stderr.trim()
        ));
    }
    Ok(())
}

async fn apply_output(usb_id: &str, output: &str) -> Result<(), String> {
    gsettings_set_tablet_key(usb_id, "output", output).await?;
    Ok(())
}

/// Reapply GNOME tablet `output` keys from Mutter physical monitor EDIDs.
///
/// Call only after a **successful** display reconcile when topology should match Mutter.
pub(crate) async fn reapply_after_reconcile(
    cfg: &TabletMappingConfig,
    desired_primary_connector: &str,
    physical: &[PhysicalMonitor],
) {
    if !cfg.enable {
        return;
    }

    let Some(edp1) = output_target_for_connector(physical, "eDP-1") else {
        warn!("Tablet mapping: no Mutter physical row for eDP-1; skipping pen remap");
        return;
    };

    let edp2 = output_target_for_connector(physical, "eDP-2");
    let primary_target: &(String, String, String, String) = match desired_primary_connector {
        "eDP-2" => edp2.as_ref().unwrap_or(&edp1),
        _ => &edp1,
    };

    match cfg.mode {
        TabletMapMode::OneToOne => {
            if let Some(ref t2) = edp2 {
                let v_top = gvariant_output(&edp1.0, &edp1.1, &edp1.2, &edp1.3);
                match apply_output(TABLET_A, &v_top).await {
                    Ok(()) => info!("Tablet mapping: {TABLET_A} → eDP-1 ({})", edp1.1),
                    Err(e) => warn!("Tablet mapping: {TABLET_A}: {e}"),
                }
                let v_bot = gvariant_output(&t2.0, &t2.1, &t2.2, &t2.3);
                match apply_output(TABLET_B, &v_bot).await {
                    Ok(()) => info!("Tablet mapping: {TABLET_B} → eDP-2 ({})", t2.1),
                    Err(e) => warn!("Tablet mapping: {TABLET_B}: {e}"),
                }
            } else {
                warn!(
                    "Tablet mapping: no Mutter physical row for eDP-2; left {TABLET_B} unchanged"
                );
            }
        }
        TabletMapMode::AllToPrimary => {
            let v = gvariant_output(
                &primary_target.0,
                &primary_target.1,
                &primary_target.2,
                &primary_target.3,
            );
            match apply_output(TABLET_A, &v).await {
                Ok(()) => info!(
                    "Tablet mapping: {TABLET_A} → primary {desired_primary_connector} ({})",
                    primary_target.1
                ),
                Err(e) => warn!("Tablet mapping: {TABLET_A}: {e}"),
            }
            match apply_output(TABLET_B, &v).await {
                Ok(()) => info!(
                    "Tablet mapping: {TABLET_B} → primary {desired_primary_connector} ({})",
                    primary_target.1
                ),
                Err(e) => warn!("Tablet mapping: {TABLET_B}: {e}"),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::gvariant_output;

    #[test]
    fn gvariant_escapes_single_quote_in_edid_field() {
        assert_eq!(
            gvariant_output("x", "y", "a'b", "eDP-2"),
            r"['x','y','a\'b','eDP-2']"
        );
    }
}
