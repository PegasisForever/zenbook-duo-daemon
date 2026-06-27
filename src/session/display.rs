use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use log::{debug, error, info, warn};
use tokio::sync::broadcast;
use zbus::{Connection, proxy, zvariant::OwnedValue};
use futures::stream::StreamExt;


// ── Mutter D-Bus type aliases ─────────────────────────────────────────────────

type ModeProps = HashMap<String, OwnedValue>;
type MonitorMode = (String, i32, i32, f64, f64, Vec<f64>, ModeProps);
pub(crate) type PhysicalMonitor = ((String, String, String, String), Vec<MonitorMode>, HashMap<String, OwnedValue>);
type LmRef = (String, String, String, String);
pub(crate) type LogicalMonitor = (i32, i32, f64, u32, bool, Vec<LmRef>, HashMap<String, OwnedValue>);
type CurrentState = (u32, Vec<PhysicalMonitor>, Vec<LogicalMonitor>, HashMap<String, OwnedValue>);
type ApplyMonSpec = (String, String, HashMap<String, OwnedValue>);
type ApplyLm = (i32, i32, f64, u32, bool, Vec<ApplyMonSpec>);
type BacklightState = (u32, Vec<HashMap<String, OwnedValue>>);

const DEFAULT_DUO_SCALE: f64 = 5.0 / 3.0;
const RECONCILE_DEBOUNCE_MS: u64 = 1200;
const ORIENTATION_STABILIZATION_MS: u64 = 1800;
/// After `MonitorsChanged`, wait before classifying so Mutter and the “Keep these settings?” dialog settle.
const GNOME_TOPOLOGY_DEBOUNCE_MS: u64 = 600;
/// Reconcile debounce for availability nudges (MonitorsChanged path) — shorter because the
/// MonitorsChanged handler already debounced and verified serial stability.
const AVAILABILITY_RECONCILE_DEBOUNCE_MS: u64 = 200;
/// On session daemon startup, gnome-shell has already reset the display (it always does on a new
/// shell process). Using the full 1200ms debounce means the bottom screen stays off for ~2s.
/// A short startup debounce recovers faster; by the time our daemon starts, Mutter is already up
/// (graphical-session.target fired before us), so there is no risk of racing an unready compositor.
const STARTUP_RECONCILE_DEBOUNCE_MS: u64 = 200;
/// After phase 1 of the eDP-2-primary two-step apply, wait before phase 2 so KMS/Mutter can finish
/// committing the dual layout. See README "Dual display / Mutter apply ordering".
const EDPTWO_PRIMARY_PHASE1_STABILITY_MS: u64 = 300;
/// Sleep between display recovery attempts.
const DISPLAY_RECOVERY_ATTEMPT_DELAY_SECS: u64 = 1;
/// Transient “attempt N/20” toasts: long enough to read, but gone before the next attempt.
const DISPLAY_RECOVERY_RETRY_TOAST_EXPIRE_MS: i32 = {
    let interval_ms = (DISPLAY_RECOVERY_ATTEMPT_DELAY_SECS * 1000) as i32;
    let minus_100 = interval_ms.saturating_sub(100);
    if minus_100 > 500 {
        minus_100
    } else {
        500
    }
};

#[proxy(
    interface = "org.gnome.Mutter.DisplayConfig",
    default_service = "org.gnome.Mutter.DisplayConfig",
    default_path = "/org/gnome/Mutter/DisplayConfig"
)]
pub trait DisplayConfig {
    fn get_current_state(&self) -> zbus::Result<CurrentState>;
    fn set_backlight(&self, serial: u32, connector: String, value: i32) -> zbus::Result<()>;
    fn apply_monitors_config(
        &self,
        serial: u32,
        method: u32,
        logical_monitors: Vec<ApplyLm>,
        properties: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;
    
    #[zbus(signal)]
    fn monitors_changed(&self) -> zbus::Result<()>;

    #[zbus(property)]
    fn backlight(&self) -> zbus::Result<BacklightState>;
}

// ── Layout helpers ────────────────────────────────────────────────────────────

pub(crate) fn logical_size(pw: i32, ph: i32, scale: f64, transform: u32) -> (i32, i32) {
    // Use floor() not round(): Mutter validates adjacency using floor division internally.
    // Using round() can produce a position 1px past the edge, causing "displays not adjacent" errors
    // at non-integer scales (e.g. scale=5/3: 2880/1.6667 rounds to 1728, floor also gives 1728,
    // but at other scales they can diverge).
    let lw = (pw as f64 / scale).floor() as i32;
    let lh = (ph as f64 / scale).floor() as i32;
    if transform == 1 || transform == 3 { (lh, lw) } else { (lw, lh) }
}

/// True if any logical monitor in Mutter's current state drives connector `eDP-2`.
fn logical_layout_includes_edp2(current: &CurrentState) -> bool {
    current
        .2
        .iter()
        .any(|lm| lm.5.iter().any(|connector_ref| connector_ref.0 == "eDP-2"))
}

fn extract_all_modes(physical: &[PhysicalMonitor]) -> HashMap<String, (String, i32, i32)> {
    let mut map = HashMap::new();
    for (info, modes, _) in physical {
        let has_flag = |key: &str, m: &MonitorMode| {
            m.6.get(key)
                .and_then(|v: &OwnedValue| bool::try_from(v.clone()).ok())
                .unwrap_or(false)
        };
        let chosen = modes.iter().find(|m| has_flag("is-current", m))
            .or_else(|| modes.iter().find(|m| has_flag("is-preferred", m)))
            .or_else(|| modes.first());
        if let Some((mode_id, w, h, ..)) = chosen {
            map.insert(info.0.clone(), (mode_id.clone(), *w, *h));
        }
    }
    map
}

fn find_mode_matching_size(
    physical: &[PhysicalMonitor],
    connector: &str,
    width: i32,
    height: i32,
) -> Option<(String, i32, i32)> {
    physical
        .iter()
        .find(|(info, _, _)| info.0 == connector)
        .and_then(|(_, modes, _)| {
            modes
                .iter()
                .find(|(_, w, h, ..)| *w == width && *h == height)
                .or_else(|| modes.first())
                .map(|(mode_id, w, h, ..)| (mode_id.clone(), *w, *h))
        })
}

pub async fn apply_display_brightness_value(
    value: u32,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let conn = Connection::session().await?;
    let display = DisplayConfigProxy::new(&conn).await?;
    let (serial, entries) = display.backlight().await?;

    for entry in entries {
        let connector = entry
            .get("connector")
            .and_then(|value| String::try_from(value.clone()).ok());
        let active = entry
            .get("active")
            .and_then(|value| bool::try_from(value.clone()).ok())
            .unwrap_or(false);
        let Some(connector) = connector else {
            continue;
        };
        if active && matches!(connector.as_str(), "eDP-1" | "eDP-2") {
            display.set_backlight(serial, connector, value as i32).await?;
        }
    }

    Ok(())
}

/// iio orientation string → Mutter transform u32.
/// Verified: normal=0, left-up=1, bottom-up=2, right-up=3.
fn orientation_to_transform(orientation: &str) -> u32 {
    match orientation {
        "left-up"   => 1,
        "bottom-up" => 2,
        "right-up"  => 3,
        _           => 0,
    }
}

fn transform_to_rotation(transform: u32) -> &'static str {
    match transform {
        1 => "left-up",
        2 => "inverted",
        3 => "right-up",
        _ => "normal",
    }
}

/// Build Zenbook Duo logical monitors for a given transform.
/// Physical connector placement is fixed regardless of which display is primary:
///   t=0 normal:    eDP-1(0,0)   eDP-2(0,lh)   stacked, eDP-2 below
///   t=1 left-up:   eDP-2(0,0)   eDP-1(lw,0)   eDP-2 left, eDP-1 right
///   t=2 bottom-up: eDP-2(0,0)   eDP-1(0,lh)   eDP-2 above, eDP-1 below
///   t=3 right-up:  eDP-1(0,0)   eDP-2(lw,0)   eDP-1 left, eDP-2 right
///
/// **Mutter apply order:** the logical monitor with `primary=true` must be **last** in the vector
/// passed to `apply_monitors_config` (same as `gdctl set`: specify non-primary first, then primary).
/// Otherwise the Zenbook Duo stack can end up with duplicate logical rows for one connector.
fn build_duo_lms(
    primary_connector: &str,
    primary_mode: &(String, i32, i32),
    secondary: Option<(&str, &(String, i32, i32))>,
    transform: u32,
    scale: f64,
) -> Vec<ApplyLm> {
    let mut lms = vec![];
    if let Some((sc, sm)) = secondary {
        if (primary_connector == "eDP-1" && sc == "eDP-2")
            || (primary_connector == "eDP-2" && sc == "eDP-1")
        {
            let (edp1_mode, edp2_mode) = if primary_connector == "eDP-1" {
                (primary_mode, sm)
            } else {
                (sm, primary_mode)
            };
            let (e1lw, e1lh) = logical_size(edp1_mode.1, edp1_mode.2, scale, transform);
            let (e2lw, e2lh) = logical_size(edp2_mode.1, edp2_mode.2, scale, transform);
            let ((e1x, e1y), (e2x, e2y)) = match transform {
                0 => ((0, 0), (0, e1lh)),
                1 => ((e2lw, 0), (0, 0)),
                2 => ((0, e2lh), (0, 0)),
                3 => ((0, 0), (e1lw, 0)),
                _ => ((0, 0), (0, e1lh)),
            };

            let edp1_is_primary = primary_connector == "eDP-1";
            let edp2_is_primary = primary_connector == "eDP-2";

            lms.push((
                e1x, e1y, scale, transform, edp1_is_primary,
                vec![("eDP-1".to_string(), edp1_mode.0.clone(), HashMap::new())],
            ));
            lms.push((
                e2x, e2y, scale, transform, edp2_is_primary,
                vec![("eDP-2".to_string(), edp2_mode.0.clone(), HashMap::new())],
            ));
            // eDP-1-then-eDP-2 is correct when eDP-2 is primary (non-primary first). When eDP-1 is
            // primary, swap so the primary monitor is last for Mutter.
            if primary_connector == "eDP-1" {
                lms.swap(0, 1);
            }
        } else {
            let (plw, plh) = logical_size(primary_mode.1, primary_mode.2, scale, transform);
            let (slw, slh) = logical_size(sm.1, sm.2, scale, transform);
            let (px, py, sx, sy) = match transform {
                0 => (0,   0,   0,   plh),
                1 => (slw, 0,   0,   0),
                2 => (0,   slh, 0,   0),
                3 => (0,   0,   plw, 0),
                _ => (0,   0,   0,   plh),
            };
            lms.push((
                px, py, scale, transform, true,
                vec![(primary_connector.to_string(), primary_mode.0.clone(), HashMap::new())],
            ));
            lms.push((
                sx, sy, scale, transform, false,
                vec![(sc.to_string(), sm.0.clone(), HashMap::new())],
            ));
            // Non-primary logical monitor must precede primary for Mutter (see module note).
            lms.swap(0, 1);
        }
    } else {
        lms.push((
            0, 0, scale, transform, true,
            vec![(primary_connector.to_string(), primary_mode.0.clone(), HashMap::new())],
        ));
    }
    lms
}

/// Within one **mirrored** logical monitor (several outputs, same mode), Mutter expects the
/// **desired-primary connector last** in the list passed to `apply_monitors_config` — same
/// convention as [`build_duo_lms`] for joined mode and as `gdctl set --logical-monitor` (`-M`
/// lines: non-primary outputs first, **primary last**).
fn order_mirror_mons_primary_last(mut mons: Vec<ApplyMonSpec>, primary_connector: &str) -> Vec<ApplyMonSpec> {
    if let Some(i) = mons
        .iter()
        .position(|(name, _, _)| name == primary_connector)
    {
        let row = mons.remove(i);
        mons.push(row);
    }
    mons
}

/// Read current display config from logical monitors.
/// Returns (config, is_corrupted) where is_corrupted=true if same connector appears twice.
/// Mirrors Python's read_current_state() logic with duplicate detection.
pub fn read_current_config(logical: &[LogicalMonitor]) -> (HashMap<String, (i32, i32, f64, u32, bool)>, bool) {
    crate::session::display_mode::read_current_logical_rows(logical)
}

/// Read scale from eDP-1 logical monitor. Falls back to primary logical monitor then DEFAULT.
/// eDP-1 is the reference: it never changes mode on its own and is always active.
fn read_edp1_scale(logical: &[LogicalMonitor]) -> f64 {
    if let Some(lm) = logical.iter().find(|lm| lm.5.first().map(|r| r.0.as_str()) == Some("eDP-1")) {
        return lm.2;
    }
    logical.iter().find(|lm| lm.4).map(|lm| lm.2).unwrap_or(DEFAULT_DUO_SCALE)
}

fn read_scale_for_connector(logical: &[LogicalMonitor], connector: &str) -> Option<f64> {
    logical
        .iter()
        .find(|lm| lm.5.iter().any(|r| r.0 == connector))
        .map(|lm| lm.2)
}

/// monitors.xml `<transform><rotation>` token for a Mutter transform, or `None`
/// for transform 0 (normal, unflipped) which Mutter omits entirely.
fn transform_to_xml_rotation(transform: u32) -> Option<&'static str> {
    match transform {
        0 => None,
        1 => Some("left"),
        2 => Some("upside_down"),
        3 => Some("right"),
        4 => Some("normal"),
        5 => Some("left"),
        6 => Some("upside_down"),
        7 => Some("right"),
        _ => None,
    }
}

/// Serialize Mutter's current logical layout into the exact `monitors.xml` v2 format
/// Mutter itself writes. Mutter D-Bus state is the source of truth; this string is only
/// a derived cache so the layout can be read back at login (by the user session and the
/// GDM greeter) without our daemon needing to re-apply — which is what causes login flicker.
fn serialize_monitors_xml(
    physical: &[PhysicalMonitor],
    logical: &[LogicalMonitor],
    global_props: &HashMap<String, OwnedValue>,
) -> String {
    let modes = extract_all_modes(physical);
    let layout_mode = global_props
        .get("layout-mode")
        .and_then(|v| u32::try_from(v.clone()).ok())
        .unwrap_or(1);
    let layout_mode_str = if layout_mode == 2 { "physical" } else { "logical" };

    let esc = |s: &str| {
        s.replace('&', "&amp;")
            .replace('<', "&lt;")
            .replace('>', "&gt;")
    };

    let mut out = String::new();
    out.push_str("<monitors version=\"2\">\n");
    out.push_str("  <configuration>\n");
    out.push_str(&format!("    <layoutmode>{layout_mode_str}</layoutmode>\n"));

    for (x, y, scale, transform, primary, refs, _props) in logical {
        out.push_str("    <logicalmonitor>\n");
        out.push_str(&format!("      <x>{x}</x>\n"));
        out.push_str(&format!("      <y>{y}</y>\n"));
        out.push_str(&format!("      <scale>{scale}</scale>\n"));
        if *primary {
            out.push_str("      <primary>yes</primary>\n");
        }
        if let Some(rotation) = transform_to_xml_rotation(*transform) {
            out.push_str("      <transform>\n");
            out.push_str(&format!("        <rotation>{rotation}</rotation>\n"));
            if *transform >= 4 {
                out.push_str("        <flipped>yes</flipped>\n");
            }
            out.push_str("      </transform>\n");
        }
        for (connector, vendor, product, serial) in refs {
            out.push_str("      <monitor>\n");
            out.push_str("        <monitorspec>\n");
            out.push_str(&format!("          <connector>{}</connector>\n", esc(connector)));
            out.push_str(&format!("          <vendor>{}</vendor>\n", esc(vendor)));
            out.push_str(&format!("          <product>{}</product>\n", esc(product)));
            out.push_str(&format!("          <serial>{}</serial>\n", esc(serial)));
            out.push_str("        </monitorspec>\n");
            if let Some((mode_id, w, h)) = modes.get(connector) {
                let rate = mode_id.split('@').nth(1).unwrap_or("");
                out.push_str("        <mode>\n");
                out.push_str(&format!("          <width>{w}</width>\n"));
                out.push_str(&format!("          <height>{h}</height>\n"));
                out.push_str(&format!("          <rate>{rate}</rate>\n"));
                out.push_str("        </mode>\n");
            }
            out.push_str("      </monitor>\n");
        }
        out.push_str("    </logicalmonitor>\n");
    }
    out.push_str("  </configuration>\n");
    out.push_str("</monitors>\n");
    out
}

/// `$XDG_CONFIG_HOME/monitors.xml` (default `$HOME/.config/monitors.xml`).
fn user_monitors_xml_path() -> String {
    let config_home = std::env::var("XDG_CONFIG_HOME")
        .ok()
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| {
            let home = std::env::var("HOME").unwrap_or_else(|_| "/root".to_string());
            format!("{home}/.config")
        });
    format!("{config_home}/monitors.xml")
}

/// Atomically write the user's `monitors.xml` (temp file + rename).
async fn write_user_monitors_xml(xml: &str) -> std::io::Result<()> {
    let path = user_monitors_xml_path();
    if let Some(dir) = std::path::Path::new(&path).parent() {
        tokio::fs::create_dir_all(dir).await?;
    }
    let tmp = format!("{path}.tmp.{}", std::process::id());
    tokio::fs::write(&tmp, xml).await?;
    tokio::fs::rename(&tmp, &path).await?;
    Ok(())
}

/// After the display reaches a stable state, persist it (so login is flicker-free) and
/// propagate it to the GDM greeter via the root daemon. No-op when the serialized layout
/// is unchanged since the last persist, or when the state looks transitional/corrupted.
async fn persist_layout_if_changed(
    display: &DisplayConfigProxy<'_>,
    last_persisted: &mut Option<String>,
) {
    let (_, physical, logical, props) = match display.get_current_state().await {
        Ok(s) => s,
        Err(e) => {
            warn!("persist_layout: get_current_state failed: {e}");
            return;
        }
    };
    let (cfg, corrupted) = read_current_config(&logical);
    if corrupted || cfg.is_empty() {
        return;
    }
    let xml = serialize_monitors_xml(&physical, &logical, &props);
    if last_persisted.as_deref() == Some(xml.as_str()) {
        return;
    }

    match write_user_monitors_xml(&xml).await {
        Ok(()) => info!(
            "persist_layout: wrote {} ({} bytes, {} logical monitors)",
            user_monitors_xml_path(),
            xml.len(),
            logical.len()
        ),
        Err(e) => warn!("persist_layout: writing user monitors.xml failed: {e}"),
    }

    match crate::dbus_state::persist_display_layout_xml_to_root(xml.clone()).await {
        Ok(()) => info!("persist_layout: propagated layout to GDM via root daemon"),
        Err(e) => warn!("persist_layout: propagate to GDM via root failed: {e}"),
    }

    *last_persisted = Some(xml);
}

/// Returns true if the error is a Mutter "displays not adjacent" error.
fn is_non_adjacent_error<E: std::fmt::Display>(e: &E) -> bool {
    let s = e.to_string().to_lowercase();
    s.contains("adjacent")
}

/// Adjust the non-origin monitor's offset by `delta` pixels along the correct axis.
/// For left/right orientations (transforms 1,3): X axis. For normal/bottom-up (0,2): Y axis.
fn adjust_offset_pixel(lms: &[ApplyLm], transform: u32, delta: i32) -> Vec<ApplyLm> {
    lms.iter().cloned().map(|(x, y, scale, tr, primary, monitors)| {
        let (nx, ny) = match transform {
            1 | 3 => (if x != 0 { x + delta } else { x }, y),
            _     => (x, if y != 0 { y + delta } else { y }),
        };
        (nx, ny, scale, tr, primary, monitors)
    }).collect()
}

/// Apply a dual-monitor configuration with automatic ±1 pixel adjacency correction.
/// Returns Ok(true) = applied, Ok(false) = non-adjacent and unrecoverable after ±1px, Err = other error.
async fn apply_duo_config_with_adjacency_fix(
    display: &DisplayConfigProxy<'_>,
    serial: u32,
    lms: Vec<ApplyLm>,
    transform: u32,
) -> Result<bool, Box<dyn std::error::Error + Send + Sync>> {
    match display.apply_monitors_config(serial, 1, lms.clone(), HashMap::new()).await {
        Ok(()) => return Ok(true),
        Err(ref e) if is_non_adjacent_error(e) => {
            warn!("DisplayConfig: non-adjacent error on original config, trying +1px on offset axis");
        }
        Err(e) => return Err(e.into()),
    }

    let lms_p1 = adjust_offset_pixel(&lms, transform, 1);
    match display.apply_monitors_config(serial, 1, lms_p1, HashMap::new()).await {
        Ok(()) => return Ok(true),
        Err(ref e) if is_non_adjacent_error(e) => {
            warn!("DisplayConfig: +1px still non-adjacent, trying -1px from original");
        }
        Err(e) => return Err(e.into()),
    }

    let lms_m1 = adjust_offset_pixel(&lms, transform, -1);
    match display.apply_monitors_config(serial, 1, lms_m1, HashMap::new()).await {
        Ok(()) => Ok(true),
        Err(ref e) if is_non_adjacent_error(e) => {
            warn!("DisplayConfig: -1px also non-adjacent — unrecoverable adjacency failure");
            Ok(false)
        }
        Err(e) => Err(e.into()),
    }
}

/// Compare requested logical monitors to Mutter's `GetCurrentState` snapshot.
///
/// `warn_on_field_mismatch`: when `false`, field differences are **debug** only (current vs desired
/// before apply — mismatch means we need an apply, not an error). When `true`, use **warn**
/// (post-apply verification).
fn requested_layout_matches_full(
    requested: &[ApplyLm],
    actual_config: &HashMap<String, (i32, i32, f64, u32, bool)>,
    actual_modes: &HashMap<String, (String, i32, i32)>,
    warn_on_field_mismatch: bool,
) -> bool {
    let expected_connectors: usize = requested.iter().map(|lm| lm.5.len()).sum();
    if expected_connectors != actual_config.len() {
        return false;
    }

    for (i, lm_request) in requested.iter().enumerate() {
        let (x, y, scale, transform, is_primary, connectors) = lm_request;

        for (connector, mode_id, _) in connectors {
            let Some((actual_x, actual_y, actual_scale, actual_transform, actual_is_primary)) =
                actual_config.get(connector)
            else {
                warn!("Display: connector {connector} missing from actual config");
                return false;
            };

            let Some((actual_mode_id, _, _)) = actual_modes.get(connector) else {
                warn!("Display: connector {connector} missing current mode");
                return false;
            };

            let matches = *x == *actual_x
                && *y == *actual_y
                && (scale - actual_scale).abs() < 0.01
                && *transform == *actual_transform
                && *is_primary == *actual_is_primary
                && mode_id == actual_mode_id;
            if !matches {
                let msg = format!(
                    "Display: LM[{i}] {connector} mismatch: requested ({x},{y},{scale},{transform},{is_primary},mode={mode_id}), got ({actual_x},{actual_y},{actual_scale},{actual_transform},{actual_is_primary},mode={actual_mode_id})"
                );
                if warn_on_field_mismatch {
                    warn!("{msg}");
                } else {
                    info!("{msg} (current vs desired — apply needed)");
                }
                return false;
            }
        }
    }

    true
}

#[derive(Debug)]
enum DisplayApplyError {
    /// Root daemon guard paused or over budget.
    GuardPaused,
    /// Could not query the root daemon; session must not apply without permission.
    GuardCheck(String),
    /// Mutter/D-Bus or verification failure after permission was granted.
    Apply(String),
}

impl std::fmt::Display for DisplayApplyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            DisplayApplyError::GuardPaused => write!(
                f,
                "root display apply guard is paused — run zenbook-duo-daemon resume-display-applies (GNOME session) or sudo zenbook-duo-daemon resume-display-applies"
            ),
            DisplayApplyError::GuardCheck(s) => write!(f, "display apply guard unreachable: {s}"),
            DisplayApplyError::Apply(s) => write!(f, "{s}"),
        }
    }
}

impl std::error::Error for DisplayApplyError {}

impl DisplayApplyError {
    fn is_guard_block(&self) -> bool {
        matches!(self, Self::GuardPaused | Self::GuardCheck(_))
    }
}

struct ApplyBusyClear<'a>(&'a Arc<AtomicBool>);

impl Drop for ApplyBusyClear<'_> {
    fn drop(&mut self) {
        self.0.store(false, Ordering::SeqCst);
    }
}

/// True when the current Mutter layout is **only eDP-1** (single logical monitor) and the desired
/// state is **dual with eDP-2 as primary**.
///
/// **Why two applies:** On Zenbook Duo + GNOME/Mutter (~50.x), a single `apply_monitors_config` that
/// both enables the second internal panel and moves the primary to eDP-2 in one shot often leaves
/// inconsistent logical-monitor state (duplicate connector on two LMs, both `primary: true` in
/// `GetCurrentState`, etc.). Manual `gdctl` experiments and journal evidence (`drmModeAtomicCommit:
/// Invalid argument`, `Page flip failed`, prior gnome-shell crashes in `meta_monitor_mode_foreach_crtc`)
/// point to KMS atomic / Mutter monitor graph updates failing or racing when too much changes at once.
///
/// **Phase 1:** dual layout with **eDP-1 still primary** (non-primary output specified first, primary
/// last — see [`build_duo_lms`]). **Phase 2:** swap to **eDP-2 primary** with the same ordering rule.
/// Between phases we sleep [`EDPTWO_PRIMARY_PHASE1_STABILITY_MS`] and re-read state (fresh **serial**)
/// before the second apply.
fn requires_phase1_edp2_primary_from_edp1_solo(
    current_corrupted: bool,
    edp2_should_be_enabled: bool,
    desired_primary_effective: &str,
    logical: &[LogicalMonitor],
    current_config: &HashMap<String, (i32, i32, f64, u32, bool)>,
) -> bool {
    !current_corrupted
        && edp2_should_be_enabled
        && desired_primary_effective == "eDP-2"
        && logical.len() == 1
        && current_config.len() == 1
        && current_config.contains_key("eDP-1")
}

/// Two stacked logical monitors (eDP-1 and eDP-2), **each** marked primary in a Mutter snapshot:
/// a wedged state seen after mode switches (`gdctl show`: both `Primary: yes`). Recovery (validated
/// manually with `gdctl set`) is: apply joined **eDP-1 primary**, then the desired layout (e.g.
/// eDP-2 primary).
fn mutter_dual_internal_primary_wedge(logical: &[LogicalMonitor]) -> bool {
    logical.len() == 2
        && logical.iter().filter(|lm| lm.4).count() == 2
        && logical.iter().all(|lm| lm.5.len() == 1)
        && logical
            .iter()
            .all(|lm| matches!(lm.5[0].0.as_str(), "eDP-1" | "eDP-2"))
        && {
            let a = logical[0].5[0].0.as_str();
            let b = logical[1].5[0].0.as_str();
            (a == "eDP-1" && b == "eDP-2") || (a == "eDP-2" && b == "eDP-1")
        }
}

/// Unified display state: read current → build desired → (permission) → apply once → verify.
/// All `apply_monitors_config` calls go through this path only.
///
/// **Two-phase eDP-2 primary:** when [`requires_phase1_edp2_primary_from_edp1_solo`] holds, this
/// performs an extra apply + stability delay before the final layout. **Do not merge into a single
/// apply** without re-validating on hardware; see README.
async fn apply_desired_display_state(
    display: &DisplayConfigProxy<'_>,
    keyboard_pogo_docked: bool,
    desired_secondary_enabled: bool,
    desired_primary: &str,
    desired_transform: u32,
    desired_attachment: &str,
    desired_layout: &str,
    apply_allow_fallback: bool,
) -> Result<bool, DisplayApplyError> {
    for pass in 0u32..2 {
        let (raw_attachment, layout) = if pass == 0 {
            (desired_attachment, desired_layout)
        } else {
            (
                super::display_mode::ATTACH_BUILTIN_ONLY,
                super::display_mode::LAYOUT_JOINED,
            )
        };

        if pass == 1 {
            info!("Display: falling back to builtin_only+joined after failed canonical apply");
            let _ = super::notifications::send_transient_notification(
                "Display layout",
                "Could not apply the requested layout. Restored internal panels only — adjust externals in Settings if needed.",
                "dialog-warning",
                6500,
                1,
            )
            .await;
        }

        // Single fallback pass: only when caller allows it and the first pass was not already the safe default.
        let try_fallback = pass == 0
            && apply_allow_fallback
            && (desired_attachment != super::display_mode::ATTACH_BUILTIN_ONLY
                || desired_layout != super::display_mode::LAYOUT_JOINED);

        let (mut serial, mut physical, mut logical, _props) = display
            .get_current_state()
            .await
            .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
        let mut all_modes = extract_all_modes(&physical);
        let (mut current_config, mut current_corrupted) = read_current_config(&logical);

        let externals_with_modes =
            super::display_mode::external_connectors_ordered(&physical, &all_modes);
        let attachment =
            if raw_attachment == super::display_mode::ATTACH_ALL_CONNECTED
                && externals_with_modes.is_empty()
            {
                info!(
                    "Display: persisted all_connected but no external connectors with modes — applying as builtin_only"
                );
                super::display_mode::ATTACH_BUILTIN_ONLY
            } else {
                raw_attachment
            };

        let edp2_physically_available = all_modes.contains_key("eDP-2");
        let edp2_should_be_enabled =
            edp2_physically_available && desired_secondary_enabled && !keyboard_pogo_docked;

        let mut edp1_scale = read_edp1_scale(&logical);

        let desired_primary_effective = if desired_primary == "eDP-2" {
            if !edp2_physically_available || !desired_secondary_enabled || keyboard_pogo_docked {
                "eDP-1".to_string()
            } else {
                "eDP-2".to_string()
            }
        } else {
            "eDP-1".to_string()
        };

        let edp1_mode = all_modes
            .get("eDP-1")
            .cloned()
            .ok_or_else(|| DisplayApplyError::Apply("No mode found for eDP-1".to_string()))?;
        let edp2_mode_if_duo: Option<(String, i32, i32)> = if edp2_should_be_enabled {
            Some(
                find_mode_matching_size(&physical, "eDP-2", edp1_mode.1, edp1_mode.2)
                    .or_else(|| all_modes.get("eDP-2").cloned())
                    .ok_or_else(|| DisplayApplyError::Apply("No mode found for eDP-2".to_string()))?,
            )
        } else {
            None
        };

        let desired_lms: Vec<ApplyLm> =
            if attachment == super::display_mode::ATTACH_EXTERNAL_ONLY {
                let mut v = vec![];
                for lm in &logical {
                    let mons: Vec<_> = lm
                        .5
                        .iter()
                        .filter(|r| !super::display_mode::is_internal_connector(&r.0))
                        .filter_map(|r| {
                            all_modes
                                .get(&r.0)
                                .map(|m| (r.0.clone(), m.0.clone(), HashMap::new()))
                        })
                        .collect();
                    if mons.is_empty() {
                        continue;
                    }
                    v.push((lm.0, lm.1, lm.2, lm.3, lm.4, mons));
                }
                if v.is_empty() {
                    info!("Display: external_only but no external in logical layout; skip apply");
                    return Ok(false);
                }
                v.sort_by_key(|l| !l.4);
                v
            } else if attachment == super::display_mode::ATTACH_BUILTIN_ONLY
                && layout == super::display_mode::LAYOUT_MIRROR
            {
                if let Some(ref em) = edp2_mode_if_duo {
                    let mons = order_mirror_mons_primary_last(
                        vec![
                            ("eDP-1".to_string(), edp1_mode.0.clone(), HashMap::new()),
                            ("eDP-2".to_string(), em.0.clone(), HashMap::new()),
                        ],
                        &desired_primary_effective,
                    );
                    vec![(
                        0,
                        0,
                        edp1_scale,
                        desired_transform,
                        true,
                        mons,
                    )]
                } else {
                    let mons = vec![(
                        "eDP-1".to_string(),
                        edp1_mode.0.clone(),
                        HashMap::new(),
                    )];
                    vec![(
                        0,
                        0,
                        edp1_scale,
                        desired_transform,
                        true,
                        mons,
                    )]
                }
            } else if attachment == super::display_mode::ATTACH_ALL_CONNECTED
                && layout == super::display_mode::LAYOUT_MIRROR
            {
                let mut mons = vec![(
                    "eDP-1".to_string(),
                    edp1_mode.0.clone(),
                    HashMap::new(),
                )];
                if edp2_should_be_enabled {
                    if let Some(ref em) = edp2_mode_if_duo {
                        mons.push(("eDP-2".to_string(), em.0.clone(), HashMap::new()));
                    }
                }
                for ext_name in &externals_with_modes {
                    let ext_mode = all_modes.get(ext_name).ok_or_else(|| {
                        DisplayApplyError::Apply(format!("No mode for external {ext_name}"))
                    })?;
                    mons.push((ext_name.clone(), ext_mode.0.clone(), HashMap::new()));
                }
                let mons = order_mirror_mons_primary_last(mons, &desired_primary_effective);
                vec![(0, 0, edp1_scale, desired_transform, true, mons)]
            } else if attachment == super::display_mode::ATTACH_ALL_CONNECTED
                && layout == super::display_mode::LAYOUT_JOINED
            {
                let mut duo_lms = if let Some(ref edp2_mode) = edp2_mode_if_duo {
                    let (primary_mode, secondary_connector, secondary_mode) =
                        if desired_primary_effective == "eDP-1" {
                            (&edp1_mode, "eDP-2", edp2_mode)
                        } else {
                            (edp2_mode, "eDP-1", &edp1_mode)
                        };
                    build_duo_lms(
                        &desired_primary_effective,
                        primary_mode,
                        Some((secondary_connector, secondary_mode)),
                        desired_transform,
                        edp1_scale,
                    )
                } else {
                    build_duo_lms(
                        &desired_primary_effective,
                        &edp1_mode,
                        None,
                        desired_transform,
                        edp1_scale,
                    )
                };
                let (e1x, e1y) = duo_lms
                    .iter()
                    .find(|lm| lm.5.iter().any(|(c, _, _)| c == "eDP-1"))
                    .map(|lm| (lm.0, lm.1))
                    .unwrap_or((0, 0));
                let mut cursor_x = e1x
                    + super::display_mode::edp_logical_extent_x(
                        edp1_mode.1,
                        edp1_mode.2,
                        edp1_scale,
                        desired_transform,
                    );
                for ext_name in &externals_with_modes {
                    let ext_mode = all_modes.get(ext_name).ok_or_else(|| {
                        DisplayApplyError::Apply(format!("No mode for external {ext_name}"))
                    })?;
                    let ext_scale =
                        read_scale_for_connector(&logical, ext_name).unwrap_or(edp1_scale);
                    duo_lms.push((
                        cursor_x,
                        e1y,
                        ext_scale,
                        0u32,
                        false,
                        vec![(ext_name.clone(), ext_mode.0.clone(), HashMap::new())],
                    ));
                    cursor_x += super::display_mode::edp_logical_extent_x(
                        ext_mode.1,
                        ext_mode.2,
                        ext_scale,
                        0u32,
                    );
                }
                duo_lms.sort_by_key(|lm| !lm.4);
                duo_lms
            } else if let Some(ref edp2_mode) = edp2_mode_if_duo {
                let (primary_mode, secondary_connector, secondary_mode) =
                    if desired_primary_effective == "eDP-1" {
                        (&edp1_mode, "eDP-2", edp2_mode)
                    } else {
                        (edp2_mode, "eDP-1", &edp1_mode)
                    };
                build_duo_lms(
                    &desired_primary_effective,
                    primary_mode,
                    Some((secondary_connector, secondary_mode)),
                    desired_transform,
                    edp1_scale,
                )
            } else {
                build_duo_lms(
                    &desired_primary_effective,
                    &edp1_mode,
                    None,
                    desired_transform,
                    edp1_scale,
                )
            };

        let needs_dual_primary_wedge_heal = mutter_dual_internal_primary_wedge(&logical)
            && edp2_should_be_enabled
            && attachment == super::display_mode::ATTACH_BUILTIN_ONLY
            && matches!(
                layout,
                super::display_mode::LAYOUT_JOINED | super::display_mode::LAYOUT_MIRROR
            );

        if needs_dual_primary_wedge_heal {
            let Some(ref edp2_mode) = edp2_mode_if_duo else {
                return Err(DisplayApplyError::Apply(
                    "internal error: dual-primary wedge heal requires eDP-2 mode".to_string(),
                ));
            };
            info!("Display: Mutter dual-primary wedge on both internals — healing with eDP-1-primary joined layout, then applying desired layout");
            match crate::dbus_state::register_display_apply_attempt().await {
                Ok(crate::dbus_state::DisplayApplyPermit::Allowed) => {}
                Ok(crate::dbus_state::DisplayApplyPermit::Paused) => {
                    return Err(DisplayApplyError::GuardPaused);
                }
                Err(e) => return Err(DisplayApplyError::GuardCheck(e)),
            }

            let heal_lms = build_duo_lms(
                "eDP-1",
                &edp1_mode,
                Some(("eDP-2", edp2_mode)),
                desired_transform,
                edp1_scale,
            );
            let applied = apply_duo_config_with_adjacency_fix(
                display,
                serial,
                heal_lms,
                desired_transform,
            )
            .await
            .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
            if !applied {
                if try_fallback {
                    continue;
                }
                return Err(DisplayApplyError::Apply(
                    "Dual-primary wedge heal: failed to apply eDP-1-primary joined layout"
                        .to_string(),
                ));
            }

            tokio::time::sleep(std::time::Duration::from_millis(
                EDPTWO_PRIMARY_PHASE1_STABILITY_MS,
            ))
            .await;

            let (s, phys, log, _) = display
                .get_current_state()
                .await
                .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
            serial = s;
            physical = phys;
            logical = log;
            all_modes = extract_all_modes(&physical);
            (current_config, current_corrupted) = read_current_config(&logical);
            edp1_scale = read_edp1_scale(&logical);
        }

        let current_matches_desired =
            !current_corrupted
                && requested_layout_matches_full(&desired_lms, &current_config, &all_modes, false);
        if current_matches_desired {
            info!("Display state already matches desired (no change needed)");
            return Ok(false);
        }

        let run_edp2_primary_phase1 = attachment == super::display_mode::ATTACH_BUILTIN_ONLY
            && layout == super::display_mode::LAYOUT_JOINED
            && requires_phase1_edp2_primary_from_edp1_solo(
                current_corrupted,
                edp2_should_be_enabled,
                &desired_primary_effective,
                &logical,
                &current_config,
            );

        if run_edp2_primary_phase1 {
            let Some(ref edp2_mode) = edp2_mode_if_duo else {
                return Err(DisplayApplyError::Apply(
                    "internal error: two-phase eDP-2 primary requires eDP-2 mode".to_string(),
                ));
            };
            let intermediate_lms = build_duo_lms(
                "eDP-1",
                &edp1_mode,
                Some(("eDP-2", edp2_mode)),
                desired_transform,
                edp1_scale,
            );
            info!(
                "Display: two-phase eDP-2 primary — phase 1: enable dual layout with eDP-1 still primary, then pause for KMS/Mutter"
            );
            match crate::dbus_state::register_display_apply_attempt().await {
                Ok(crate::dbus_state::DisplayApplyPermit::Allowed) => {}
                Ok(crate::dbus_state::DisplayApplyPermit::Paused) => {
                    return Err(DisplayApplyError::GuardPaused);
                }
                Err(e) => return Err(DisplayApplyError::GuardCheck(e)),
            }

            let applied = apply_duo_config_with_adjacency_fix(
                display,
                serial,
                intermediate_lms,
                desired_transform,
            )
            .await
            .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
            if !applied {
                if try_fallback {
                    continue;
                }
                return Err(DisplayApplyError::Apply(
                    "Two-phase eDP-2 primary: phase 1 failed (dual layout with eDP-1 primary)"
                        .to_string(),
                ));
            }

            tokio::time::sleep(std::time::Duration::from_millis(
                EDPTWO_PRIMARY_PHASE1_STABILITY_MS,
            ))
            .await;

            let (serial_after_p1, _phys_p1, logical_after_p1, _) = display
                .get_current_state()
                .await
                .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
            let (cfg_p1, corrupt_p1) = read_current_config(&logical_after_p1);
            if corrupt_p1 || !cfg_p1.contains_key("eDP-1") || !cfg_p1.contains_key("eDP-2") {
                if try_fallback {
                    continue;
                }
                return Err(DisplayApplyError::Apply(
                    "Two-phase eDP-2 primary: phase 1 did not leave a stable dual layout".to_string(),
                ));
            }
            serial = serial_after_p1;
        }

        match crate::dbus_state::register_display_apply_attempt().await {
            Ok(crate::dbus_state::DisplayApplyPermit::Allowed) => {}
            Ok(crate::dbus_state::DisplayApplyPermit::Paused) => {
                return Err(DisplayApplyError::GuardPaused);
            }
            Err(e) => return Err(DisplayApplyError::GuardCheck(e)),
        }

        let use_duo_adjacency = desired_lms.len() == 2
            && attachment == super::display_mode::ATTACH_BUILTIN_ONLY
            && layout == super::display_mode::LAYOUT_JOINED;

        let apply_outcome: Result<(), DisplayApplyError> = if use_duo_adjacency {
            let applied = apply_duo_config_with_adjacency_fix(
                display,
                serial,
                desired_lms.clone(),
                desired_transform,
            )
            .await
            .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
            if !applied {
                Err(DisplayApplyError::Apply(
                    "Failed to apply desired dual-monitor layout after adjacency correction"
                        .to_string(),
                ))
            } else {
                Ok(())
            }
        } else {
            display
                .apply_monitors_config(serial, 1, desired_lms.clone(), HashMap::new())
                .await
                .map_err(|e| DisplayApplyError::Apply(e.to_string()))
        };

        if let Err(e) = apply_outcome {
            if try_fallback {
                continue;
            }
            return Err(e);
        }

        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        let (_, physical_after, logical_after, _) = display
            .get_current_state()
            .await
            .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
        let (after_config, after_corrupted) = read_current_config(&logical_after);
        let after_modes = extract_all_modes(&physical_after);
        let verified =
            !after_corrupted
                && requested_layout_matches_full(&desired_lms, &after_config, &after_modes, true);
        if !verified {
            if try_fallback {
                continue;
            }
            return Err(DisplayApplyError::Apply(
                "Desired display layout mismatch after apply".to_string(),
            ));
        }

        info!("Display state applied successfully");
        return Ok(true);
    }

    Err(DisplayApplyError::Apply(
        "Display apply: exhausted fallback attempts".into(),
    ))
}

/// While true, [`subscribe_monitors_changed`] must not persist observed display mode or nudge reconcile.
#[inline]
fn mon_changed_skip_if_apply_busy(apply_busy: &Arc<AtomicBool>, phase: &'static str) -> bool {
    if apply_busy.load(Ordering::SeqCst) {
        debug!(
            "MonitorsChanged: apply/recovery active ({phase}) — ignoring (no root display mode change, no reconcile nudge)"
        );
        true
    } else {
        false
    }
}

/// Subscribe to `MonitorsChanged`: debounce, classify topology, report observed mode to root, and
/// nudge reconciliation (including eDP-2 primary restore when applicable).
///
/// Suppressed whenever [`apply_busy`](AtomicBool) is set for the whole handling path: we do not
/// call [`crate::dbus_state::report_observed_display_mode_from_session`] or
/// `availability_tx` while the session is applying layout, in fallback, or in recovery.
async fn subscribe_monitors_changed(
    display: &DisplayConfigProxy<'_>,
    availability_tx: broadcast::Sender<()>,
    desired_primary: Arc<tokio::sync::RwLock<String>>,
    apply_busy: Arc<AtomicBool>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    info!("DisplayConfig: subscribing to MonitorsChanged signal (event-driven, no polling)");

    let mut signal_rx = display.receive_monitors_changed().await?;

    while signal_rx.next().await.is_some() {
        if mon_changed_skip_if_apply_busy(&apply_busy, "before debounce") {
            continue;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(GNOME_TOPOLOGY_DEBOUNCE_MS)).await;

        if mon_changed_skip_if_apply_busy(&apply_busy, "after debounce") {
            continue;
        }

        let (serial_a, _, _, _) = match display.get_current_state().await {
            Ok(s) => s,
            Err(e) => {
                warn!("MonitorsChanged: get_current_state failed: {e}");
                continue;
            }
        };

        if mon_changed_skip_if_apply_busy(&apply_busy, "after first GetCurrentState") {
            continue;
        }

        tokio::time::sleep(tokio::time::Duration::from_millis(150)).await;

        if mon_changed_skip_if_apply_busy(&apply_busy, "after serial stability delay") {
            continue;
        }

        let (serial_b, _, logical_b, _) = match display.get_current_state().await {
            Ok(s) => s,
            Err(e) => {
                warn!("MonitorsChanged: second get_current_state failed: {e}");
                continue;
            }
        };

        if mon_changed_skip_if_apply_busy(&apply_busy, "after second GetCurrentState") {
            continue;
        }

        if serial_a != serial_b {
            debug!(
                "MonitorsChanged: state still changing (serial {serial_a} -> {serial_b}), skipping classify"
            );
            continue;
        }

        if mon_changed_skip_if_apply_busy(&apply_busy, "before report to root") {
            continue;
        }

        let attachment = super::display_mode::classify_attachment(&logical_b);
        let layout = super::display_mode::classify_layout(&logical_b);
        info!(
            "MonitorsChanged: classified display state as attachment={attachment} layout={layout}, persisting to root"
        );
        if let Err(e) = crate::dbus_state::report_observed_display_mode_from_session(
            attachment.to_string(),
            layout.to_string(),
        )
        .await
        {
            warn!("MonitorsChanged: report_observed_display_mode failed: {e}");
        }

        if mon_changed_skip_if_apply_busy(&apply_busy, "before availability nudge") {
            continue;
        }

        let desired = desired_primary.read().await.clone();
        if desired == "eDP-2" {
            let (logical_map, _) = read_current_config(&logical_b);
            match logical_map.get("eDP-2") {
                Some((_, _, _, _, is_primary)) if !*is_primary => {
                    info!(
                        "MonitorsChanged: eDP-2 is available and desired, attempting to restore as primary"
                    );
                }
                _ => {}
            }
        }

        let _ = availability_tx.send(());
    }

    Ok(())
}

fn ensure_availability_monitor(
    availability_task: &mut Option<tokio::task::JoinHandle<()>>,
    conn: &Connection,
    availability_tx: &broadcast::Sender<()>,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
    apply_busy: &Arc<AtomicBool>,
) {
    let already_running = availability_task
        .as_ref()
        .is_some_and(|task| !task.is_finished());
    if already_running {
        return;
    }

    let conn_clone = conn.clone();
    let availability_tx_clone = availability_tx.clone();
    let desired_primary_clone = Arc::clone(desired_primary);
    let apply_busy_clone = Arc::clone(apply_busy);
    *availability_task = Some(tokio::spawn(async move {
        match DisplayConfigProxy::new(&conn_clone).await {
            Ok(display) => {
                if let Err(e) = subscribe_monitors_changed(
                    &display,
                    availability_tx_clone,
                    desired_primary_clone,
                    apply_busy_clone,
                )
                .await
                {
                    error!("DisplayConfig: MonitorsChanged subscription error: {e}");
                }
            }
            Err(e) => {
                error!("DisplayConfig: failed to create proxy for MonitorsChanged listener: {e}");
            }
        }
    }));
}

async fn current_desired_transform(
    display: &DisplayConfigProxy<'_>,
) -> Result<u32, Box<dyn std::error::Error + Send + Sync>> {
    match super::orientation::current_orientation().await {
        Ok(Some(orientation)) => Ok(orientation_to_transform(&orientation)),
        Ok(None) => Ok(display
            .get_current_state()
            .await?
            .2
            .iter()
            .find(|lm| lm.4)
            .map(|lm| lm.3)
            .unwrap_or(0)),
        Err(e) => {
            warn!("Display recovery: failed to read current orientation: {e}");
            Ok(display
                .get_current_state()
                .await?
                .2
                .iter()
                .find(|lm| lm.4)
                .map(|lm| lm.3)
                .unwrap_or(0))
        }
    }
}

async fn reconcile_display_state(
    display: &DisplayConfigProxy<'_>,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
    desired_secondary: &Arc<tokio::sync::RwLock<bool>>,
    keyboard_pogo_docked: bool,
    apply_busy: &Arc<AtomicBool>,
    apply_serial: &Arc<tokio::sync::Mutex<()>>,
) -> Result<bool, DisplayApplyError> {
    apply_busy.store(true, Ordering::SeqCst);
    let _apply_idle = ApplyBusyClear(apply_busy);
    let _serial_guard = apply_serial.lock().await;

    let desired_primary_value = desired_primary.read().await.clone();
    let desired_secondary_enabled = *desired_secondary.read().await;

    let (desired_attachment, desired_layout) =
        match crate::dbus_state::read_persisted_display_mode_from_root().await {
            Ok(v) => v,
            Err(e) if e.contains("not registered") => {
                warn!("Display reconcile: {e}; skipping apply until session registers with root");
                return Ok(false);
            }
            Err(e) => return Err(DisplayApplyError::Apply(e)),
        };

    let desired_transform = current_desired_transform(display)
        .await
        .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
    if let Err(e) = crate::dbus_state::report_display_rotation_from_session(
        transform_to_rotation(desired_transform).to_string(),
    )
    .await
    {
        warn!("Display reconcile: report_display_rotation failed: {e}");
    }
    info!(
        "Display reconcile: persisted mode attachment={desired_attachment} layout={desired_layout}; \
         desired transform={desired_transform} (from accelerometer)"
    );

    let changed = apply_desired_display_state(
        display,
        keyboard_pogo_docked,
        desired_secondary_enabled,
        &desired_primary_value,
        desired_transform,
        &desired_attachment,
        &desired_layout,
        true,
    )
    .await?;

    Ok(changed)
}

async fn attempt_display_recovery(
    conn: &Connection,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
    desired_secondary: &Arc<tokio::sync::RwLock<bool>>,
    keyboard_pogo_docked: &Arc<tokio::sync::RwLock<bool>>,
) -> Result<(), DisplayApplyError> {
    let display = DisplayConfigProxy::new(conn)
        .await
        .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;

    let kb_before = *keyboard_pogo_docked.read().await;
    let sec_before = *desired_secondary.read().await;
    let pri_before = desired_primary.read().await.clone();

    let (attachment, layout) =
        match crate::dbus_state::read_persisted_display_mode_from_root().await {
            Ok(v) => v,
            Err(e) => {
                return Err(DisplayApplyError::Apply(format!(
                    "persisted display mode unavailable: {e}"
                )));
            }
        };

    let desired_transform = current_desired_transform(&display)
        .await
        .map_err(|e| DisplayApplyError::Apply(e.to_string()))?;
    if let Err(e) = crate::dbus_state::report_display_rotation_from_session(
        transform_to_rotation(desired_transform).to_string(),
    )
    .await
    {
        warn!("Display recovery: report_display_rotation failed: {e}");
    }

    apply_desired_display_state(
        &display,
        kb_before,
        sec_before,
        &pri_before,
        desired_transform,
        &attachment,
        &layout,
        true,
    )
    .await?;

    let kb_after = *keyboard_pogo_docked.read().await;
    let sec_after = *desired_secondary.read().await;
    let pri_after = desired_primary.read().await.clone();
    if (kb_before, sec_before, pri_before) != (kb_after, sec_after, pri_after) {
        return Err(DisplayApplyError::Apply(
            "keyboard or desired display inputs changed during recovery; not treating as layout convergence"
                .to_string(),
        ));
    }

    Ok(())
}

fn ensure_display_recovery_task(
    recovery_task: &mut Option<tokio::task::JoinHandle<()>>,
    conn: &Connection,
    desired_primary: &Arc<tokio::sync::RwLock<String>>,
    desired_secondary: &Arc<tokio::sync::RwLock<bool>>,
    keyboard_pogo_docked: &Arc<tokio::sync::RwLock<bool>>,
    reason: &'static str,
    apply_busy: Arc<AtomicBool>,
    apply_serial: Arc<tokio::sync::Mutex<()>>,
) {
    let already_running = recovery_task
        .as_ref()
        .is_some_and(|task| !task.is_finished());
    if already_running {
        return;
    }

    let conn = conn.clone();
    let desired_primary = Arc::clone(desired_primary);
    let desired_secondary = Arc::clone(desired_secondary);
    let keyboard_pogo_docked = Arc::clone(keyboard_pogo_docked);
    *recovery_task = Some(tokio::spawn(async move {
        apply_busy.store(true, Ordering::SeqCst);
        let _campaign_apply_idle = ApplyBusyClear(&apply_busy);
        let _campaign_serial = apply_serial.lock().await;

        const MAX_ATTEMPTS: usize = 20;

        for attempt in 1..=MAX_ATTEMPTS {
            match attempt_display_recovery(
                &conn,
                &desired_primary,
                &desired_secondary,
                &keyboard_pogo_docked,
            )
            .await
            {
                Ok(()) => {
                    info!(
                        "Display recovery: converged successfully on attempt {}/{} after {}",
                        attempt, MAX_ATTEMPTS, reason
                    );
                    super::notifications::reset_display_recovery_notif_chain().await;
                    return;
                }
                Err(e) if e.is_guard_block() => {
                    warn!("Display recovery: aborting — {e}");
                    if let Err(notify_err) =
                        super::notifications::send_display_recovery_persistent_critical_chained(
                            "Display applies blocked",
                            "The root display guard paused changes or could not be reached, so recovery stopped. If applies were paused due to repeated failures, run: zenbook-duo-daemon resume-display-applies (from GNOME session) or sudo zenbook-duo-daemon resume-display-applies",
                            "dialog-warning",
                        )
                        .await
                    {
                        warn!("Display recovery: failed to send notification: {notify_err}");
                    }
                    return;
                }
                Err(e) => {
                    let msg = e.to_string();
                    if is_dead_session_error(&msg) {
                        warn!(
                            "Display recovery: session D-Bus is dead (Broken pipe) — \
                             this is a stale session daemon; exiting cleanly"
                        );
                        std::process::exit(0);
                    }
                    warn!(
                        "Display recovery: attempt {}/{} failed after {}: {}",
                        attempt, MAX_ATTEMPTS, reason, e
                    );
                    if attempt < MAX_ATTEMPTS {
                        let body = format!(
                            "Display recovery attempt {attempt}/{MAX_ATTEMPTS} did not converge yet. Retrying automatically."
                        );
                        if let Err(notify_err) =
                            super::notifications::send_display_recovery_transient_chained(
                                "Display recovery retrying",
                                &body,
                                "dialog-warning",
                                DISPLAY_RECOVERY_RETRY_TOAST_EXPIRE_MS,
                                1,
                            )
                            .await
                        {
                            warn!("Display recovery: failed to send transient retry warning: {notify_err}");
                        }
                    }
                }
            }

            if attempt < MAX_ATTEMPTS {
                tokio::time::sleep(std::time::Duration::from_secs(
                    DISPLAY_RECOVERY_ATTEMPT_DELAY_SECS,
                ))
                .await;
            }
        }

        if let Err(e) = super::notifications::send_display_recovery_persistent_critical_chained(
            "Display Recovery Failed",
            "Zenbook Duo daemon could not restore the desired display layout after repeated retries.",
            "dialog-error",
        )
        .await
        {
            warn!("Display recovery: failed to send desktop notification: {e}");
        }
    }));
}

fn cancel_display_recovery_task(
    recovery_task: &mut Option<tokio::task::JoinHandle<()>>,
    reason: &str,
) {
    if let Some(task) = recovery_task.take() {
        if !task.is_finished() {
            info!("Display recovery: cancelling in-flight recovery after {}", reason);
            task.abort();
        }
    }
}

fn schedule_reconcile(
    debounce_deadline: &mut Option<tokio::time::Instant>,
    pending_reason: &mut &'static str,
    new_reason: &'static str,
    delay_ms: u64,
) {
    let now = tokio::time::Instant::now();
    let new_deadline = now + std::time::Duration::from_millis(delay_ms);
    if let Some(old_deadline) = *debounce_deadline {
        info!(
            "Display debounce: event='{}' rescheduling previous='{}' old_due_in={}ms new_due_in={}ms",
            new_reason,
            *pending_reason,
            old_deadline.saturating_duration_since(now).as_millis(),
            delay_ms
        );
    } else {
        info!(
            "Display debounce: event='{}' scheduling apply in {}ms",
            new_reason,
            delay_ms
        );
    }
    *pending_reason = new_reason;
    *debounce_deadline = Some(new_deadline);
}

/// Returns true when the error string indicates the session D-Bus connection is irrecoverably
/// dead — typically a Broken pipe after GDM hands off to the real user session and gnome-shell
/// has already exited. In that case the daemon is stale and should exit rather than retrying.
fn is_dead_session_error(msg: &str) -> bool {
    msg.contains("Broken pipe") || msg.contains("os error 32")
}

/// Re-run GNOME tablet `output` gsettings from current Mutter state (operator / D-Bus driven).
pub async fn reapply_tablet_mapping_now(
    display: &DisplayConfigProxy<'_>,
    desired_primary: &str,
    tablet_config: &crate::config::TabletMappingConfig,
) {
    if !tablet_config.enable {
        info!("Tablet mapping: immediate reapply skipped (disabled)");
        return;
    }
    match display.get_current_state().await {
        Ok(st) => {
            super::tablet_mapping::reapply_after_reconcile(tablet_config, desired_primary, &st.1)
                .await;
        }
        Err(e) => warn!("Tablet mapping: get_current_state for immediate reapply: {e}"),
    }
}

/// Log the full current Mutter display configuration at INFO level for diagnostics.
/// `label` identifies the call site (e.g. "startup", "post-apply").
async fn log_display_snapshot(label: &str, display: &DisplayConfigProxy<'_>) {
    match display.get_current_state().await {
        Ok((serial, physical, logical, _)) => {
            let modes = extract_all_modes(&physical);
            let (cfg, corrupted) = read_current_config(&logical);
            let mut parts: Vec<String> = cfg
                .iter()
                .map(|(conn, (x, y, scale, transform, is_primary))| {
                    let mode = modes.get(conn).map(|(m, _, _)| m.as_str()).unwrap_or("?");
                    format!(
                        "{conn}@({x},{y}) s={scale:.3} t={transform} pri={is_primary} mode={mode}"
                    )
                })
                .collect();
            parts.sort();
            info!(
                "Display snapshot [{label}]: serial={serial} corrupted={corrupted} connectors={} | {}",
                cfg.len(),
                parts.join(", ")
            );
        }
        Err(e) => warn!("Display snapshot [{label}]: get_current_state failed: {e}"),
    }
}

pub async fn run(
    mut orient_rx: broadcast::Receiver<String>,
    mut kb_rx: broadcast::Receiver<bool>,
    mut desired_secondary_rx: broadcast::Receiver<bool>,
    mut desired_primary_rx: broadcast::Receiver<String>,
    secondary_result_tx: tokio::sync::mpsc::Sender<()>,
    kb_result_tx: tokio::sync::mpsc::Sender<()>,
    desired_primary: Arc<tokio::sync::RwLock<String>>,
    desired_secondary: Arc<tokio::sync::RwLock<bool>>,
    tablet_config: Arc<tokio::sync::RwLock<crate::config::TabletMappingConfig>>,
    mut tablet_remap_rx: broadcast::Receiver<()>,
) {
    {
        let pid = std::process::id();
        let uid = nix::unistd::getuid();
        let uname = nix::unistd::User::from_uid(uid)
            .ok()
            .flatten()
            .map(|u| u.name)
            .unwrap_or_else(|| "?".to_string());
        let env = |k: &str| std::env::var(k).unwrap_or_else(|_| "<unset>".to_string());
        info!(
            "Session display daemon starting: pid={pid} uid={uid} user={uname} \
             XDG_SESSION_ID={} XDG_SESSION_TYPE={} XDG_SESSION_DESKTOP={} \
             WAYLAND_DISPLAY={} DISPLAY={} DBUS_SESSION_BUS_ADDRESS={}",
            env("XDG_SESSION_ID"),
            env("XDG_SESSION_TYPE"),
            env("XDG_SESSION_DESKTOP"),
            env("WAYLAND_DISPLAY"),
            env("DISPLAY"),
            if std::env::var("DBUS_SESSION_BUS_ADDRESS").is_ok() {
                "<set>"
            } else {
                "<unset>"
            }
        );
    }

    let conn = loop {
        match Connection::session().await {
            Ok(c) => break c,
            Err(e) => {
                error!("DisplayConfig: session D-Bus unavailable: {e}, retrying in 3s");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
    };

    let display = loop {
        match DisplayConfigProxy::new(&conn).await {
            Ok(d) => break d,
            Err(e) => {
                error!("DisplayConfig: proxy unavailable: {e}, retrying in 3s");
                tokio::time::sleep(std::time::Duration::from_secs(3)).await;
            }
        }
    };

    // Broadcast channel for availability checks triggered by display state changes
    let (availability_tx, _) = broadcast::channel::<()>(8);

    // `apply_busy`: set for an entire reconcile (including multi-pass fallback inside
    // `apply_desired_display_state`) and for an entire recovery campaign (all attempts + delays),
    // so `MonitorsChanged` never persists observed mode to root during those windows. Cleared when
    // the operation finishes, aborts (new reconcile cancels recovery), or the guard blocks recovery.
    let apply_busy = Arc::new(AtomicBool::new(false));
    let apply_serial = Arc::new(tokio::sync::Mutex::new(()));

    if let Ok(transform) = current_desired_transform(&display).await {
        info!("Display: startup desired transform={transform}");
    }
    log_display_snapshot("startup", &display).await;
    let mut availability_rx = availability_tx.subscribe();
    let mut availability_task: Option<tokio::task::JoinHandle<()>> = None;
    ensure_availability_monitor(
        &mut availability_task,
        &conn,
        &availability_tx,
        &desired_primary,
        &apply_busy,
    );

    let mut recovery_task: Option<tokio::task::JoinHandle<()>> = None;
    let mut keyboard_pogo_docked = false;
    let keyboard_pogo_docked_state = Arc::new(tokio::sync::RwLock::new(false));
    let mut keyboard_state_initialized = false;
    let mut delayed_reconciles_deepness: u32 = 0;
    let mut quiet_until: Option<tokio::time::Instant> = None;
    let mut debounce_deadline: Option<tokio::time::Instant> = None;
    let mut pending_reason: &'static str = "display state update";
    let mut changed_apply_waiting_followup = false;
    // True until the first reconcile attempt completes. While in startup, use
    // STARTUP_RECONCILE_DEBOUNCE_MS instead of RECONCILE_DEBOUNCE_MS so that
    // we recover from gnome-shell's startup display reset as quickly as possible.
    let mut in_startup = true;
    // Last layout (serialized monitors.xml) we persisted to disk + propagated to GDM.
    // Used to skip redundant writes when the stable layout has not changed.
    let mut last_persisted_layout: Option<String> = None;

    // Do not apply any desired_primary at startup from the local default.
    // Root daemon is the authority and will publish the real desired_primary
    // over D-Bus; acting before that causes visible primary-display flips.

    loop {
        tokio::select! {
            _ = async {
                if let Some(deadline) = debounce_deadline {
                    tokio::time::sleep_until(deadline).await;
                }
            }, if debounce_deadline.is_some() => {
                info!("Display debounce: applying scheduled reconcile for '{}'", pending_reason);
                debounce_deadline = None;
                in_startup = false;
                if let Some(until) = quiet_until {
                    if tokio::time::Instant::now() < until {
                        info!("Display reconcile: quiet period active, skipping apply");
                        continue;
                    }
                    quiet_until = None;
                    delayed_reconciles_deepness = 0;
                }

                cancel_display_recovery_task(&mut recovery_task, pending_reason);
                let start = std::time::Instant::now();
                match reconcile_display_state(
                    &display,
                    &desired_primary,
                    &desired_secondary,
                    keyboard_pogo_docked,
                    &apply_busy,
                    &apply_serial,
                )
                .await
                {
                    Ok(changed) => {
                        if changed {
                            changed_apply_waiting_followup = true;
                        } else {
                            delayed_reconciles_deepness = 0;
                            changed_apply_waiting_followup = false;
                        }
                        let notify_root_sysfs_poweroff_ready = {
                            let sec_desired = *desired_secondary.read().await;
                            if !sec_desired {
                                true
                            } else if keyboard_pogo_docked {
                                match display.get_current_state().await {
                                    Ok(st) => {
                                        let has_edp2 = logical_layout_includes_edp2(&st);
                                        if has_edp2 {
                                            false
                                        } else {
                                            info!(
                                                "Display: pogo-docked keyboard and Mutter layout omit eDP-2 — notifying root for sysfs secondary power save (desired_secondary still true)"
                                            );
                                            true
                                        }
                                    }
                                    Err(e) => {
                                        warn!(
                                            "Display: get_current_state failed for sysfs-ready decision: {e}"
                                        );
                                        false
                                    }
                                }
                            } else {
                                false
                            }
                        };
                        if notify_root_sysfs_poweroff_ready {
                            if let Err(e) =
                                crate::dbus_state::notify_secondary_sysfs_poweroff_ready().await
                            {
                                warn!("Display: secondary sysfs poweroff ready notify failed: {e}");
                            }
                        }
                        {
                            let tc = tablet_config.read().await.clone();
                            if tc.enable {
                                match display.get_current_state().await {
                                    Ok(st) => {
                                        let prim = desired_primary.read().await.clone();
                                        super::tablet_mapping::reapply_after_reconcile(
                                            &tc,
                                            &prim,
                                            &st.1,
                                        )
                                        .await;
                                    }
                                    Err(e) => {
                                        warn!("Tablet mapping: get_current_state after reconcile: {e}");
                                    }
                                }
                            }
                        }
                        info!(
                            "Display reconcile completed in {:.2}ms",
                            start.elapsed().as_secs_f64() * 1000.0
                        );
                        persist_layout_if_changed(&display, &mut last_persisted_layout).await;
                    }
                    Err(e) if e.is_guard_block() => {
                        warn!("Display reconcile skipped ({e}); not starting recovery without apply guard");
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                        info!(
                            "Display reconcile completed in {:.2}ms",
                            start.elapsed().as_secs_f64() * 1000.0
                        );
                    }
                    Err(e) => {
                        let msg = e.to_string();
                        if is_dead_session_error(&msg) {
                            warn!(
                                "Display reconcile: session D-Bus is dead (Broken pipe) — \
                                 stale session daemon; exiting cleanly"
                            );
                            std::process::exit(0);
                        }
                        warn!("Display reconcile failed after {pending_reason}: {e}");
                        ensure_display_recovery_task(
                            &mut recovery_task,
                            &conn,
                            &desired_primary,
                            &desired_secondary,
                            &keyboard_pogo_docked_state,
                            pending_reason,
                            Arc::clone(&apply_busy),
                            Arc::clone(&apply_serial),
                        );
                    }
                }
            }
            msg = orient_rx.recv() => match msg {
                Ok(_orientation) => {
                    if let Some(until) = quiet_until {
                        if tokio::time::Instant::now() < until {
                            info!("Display reconcile: dropping orientation signal during quiet period");
                            continue;
                        }
                        quiet_until = None;
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                    }
                    if changed_apply_waiting_followup {
                        delayed_reconciles_deepness = delayed_reconciles_deepness.saturating_add(1);
                        changed_apply_waiting_followup = false;
                    }
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "orientation update",
                        ORIENTATION_STABILIZATION_MS,
                    );
                }
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    warn!("Display handler lagged by {n}");
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "orientation lag",
                        ORIENTATION_STABILIZATION_MS,
                    );
                }
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = desired_secondary_rx.recv() => match msg {
                Ok(enable) => {
                    *desired_secondary.write().await = enable;
                    // ACK is receive-only; display apply is performed by debounced reconciler.
                    let _ = secondary_result_tx.send(()).await;

                    if let Some(until) = quiet_until {
                        if tokio::time::Instant::now() < until {
                            info!("Display reconcile: dropping desired_secondary signal during quiet period");
                            continue;
                        }
                        quiet_until = None;
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                    }
                    if changed_apply_waiting_followup {
                        delayed_reconciles_deepness = delayed_reconciles_deepness.saturating_add(1);
                        changed_apply_waiting_followup = false;
                    }
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "desired secondary update",
                        if in_startup { STARTUP_RECONCILE_DEBOUNCE_MS } else { RECONCILE_DEBOUNCE_MS },
                    );
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Desired secondary handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = kb_rx.recv() => match msg {
                Ok(attached) => {
                    let previous_keyboard_pogo_docked = keyboard_pogo_docked;
                    keyboard_pogo_docked = attached;
                    *keyboard_pogo_docked_state.write().await = attached;
                    info!("Display: keyboard_pogo_docked={attached}");
                    // ACK is receive-only; display apply is performed by debounced reconciler.
                    let _ = kb_result_tx.send(()).await;

                    if !keyboard_state_initialized {
                        keyboard_state_initialized = true;
                        if !attached {
                            info!("Display: initial keyboard state sync is detached; skipping detach restore actions");
                            continue;
                        }
                    } else if attached == previous_keyboard_pogo_docked {
                        info!("Display: keyboard_pogo_docked unchanged; skipping duplicate edge handling");
                        continue;
                    }

                    if let Some(until) = quiet_until {
                        if tokio::time::Instant::now() < until {
                            info!("Display reconcile: dropping keyboard signal during quiet period");
                            continue;
                        }
                        quiet_until = None;
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                    }
                    if changed_apply_waiting_followup {
                        delayed_reconciles_deepness = delayed_reconciles_deepness.saturating_add(1);
                        changed_apply_waiting_followup = false;
                    }
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "keyboard pogo dock state update",
                        RECONCILE_DEBOUNCE_MS,
                    );
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("Keyboard handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            _ = tablet_remap_rx.recv() => {
                let tc = tablet_config.read().await.clone();
                let prim = desired_primary.read().await.clone();
                reapply_tablet_mapping_now(&display, &prim, &tc).await;
            }
            msg = desired_primary_rx.recv() => match msg {
                Ok(desired) => {
                    *desired_primary.write().await = desired;
                    if let Some(until) = quiet_until {
                        if tokio::time::Instant::now() < until {
                            info!("Display reconcile: dropping desired_primary signal during quiet period");
                            continue;
                        }
                        quiet_until = None;
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                    }
                    if changed_apply_waiting_followup {
                        delayed_reconciles_deepness = delayed_reconciles_deepness.saturating_add(1);
                        changed_apply_waiting_followup = false;
                    }
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "desired primary update",
                        if in_startup { STARTUP_RECONCILE_DEBOUNCE_MS } else { RECONCILE_DEBOUNCE_MS },
                    );
                }
                Err(broadcast::error::RecvError::Lagged(n)) => warn!("DesiredPrimary handler lagged by {n}"),
                Err(broadcast::error::RecvError::Closed) => break,
            },
            msg = availability_rx.recv() => match msg {
                Ok(_) => {
                    if let Some(until) = quiet_until {
                        if tokio::time::Instant::now() < until {
                            info!("Display reconcile: dropping availability signal during quiet period");
                            continue;
                        }
                        quiet_until = None;
                        delayed_reconciles_deepness = 0;
                        changed_apply_waiting_followup = false;
                    }
                    if changed_apply_waiting_followup {
                        delayed_reconciles_deepness = delayed_reconciles_deepness.saturating_add(1);
                        changed_apply_waiting_followup = false;
                    }
                    schedule_reconcile(
                        &mut debounce_deadline,
                        &mut pending_reason,
                        "display availability update",
                        AVAILABILITY_RECONCILE_DEBOUNCE_MS,
                    );
                }
                Err(broadcast::error::RecvError::Lagged(_)) => {}
                Err(broadcast::error::RecvError::Closed) => break,
            },
        }

        if delayed_reconciles_deepness >= 20 {
            let quiet_period = std::time::Duration::from_secs(60);
            let timeout = std::cmp::max(
                std::time::Duration::from_secs(1),
                quiet_period.saturating_sub(std::time::Duration::from_secs(1)),
            );
            let timeout_ms = i32::try_from(timeout.as_millis()).unwrap_or(i32::MAX);
            if let Err(e) = super::notifications::send_transient_notification(
                "Display changes throttled",
                "You are changing your mind too fast or hardware is sending too many display-change signals. Quieting down for 1 minute.",
                "dialog-warning",
                timeout_ms,
                1,
            ).await {
                warn!("Display reconcile: failed to send throttling warning: {e}");
            }
            quiet_until = Some(tokio::time::Instant::now() + quiet_period);
            debounce_deadline = None;
            delayed_reconciles_deepness = 0;
            changed_apply_waiting_followup = false;
        }
    }
}
