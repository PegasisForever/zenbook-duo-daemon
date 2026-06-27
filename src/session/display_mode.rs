//! Classification of Mutter logical layout for `desired_display_attachment` / `desired_display_layout`.
//! External attachment rules are documented in **`README.md`** (external monitors).

use std::collections::{HashMap, HashSet};

use log::{info, warn};

use super::display::{LogicalMonitor, PhysicalMonitor};

pub const ATTACH_BUILTIN_ONLY: &str = "builtin_only";
pub const ATTACH_EXTERNAL_ONLY: &str = "external_only";
pub const ATTACH_ALL_CONNECTED: &str = "all_connected";

pub const LAYOUT_JOINED: &str = "joined";
pub const LAYOUT_MIRROR: &str = "mirror";

pub fn is_internal_connector(name: &str) -> bool {
    matches!(name, "eDP-1" | "eDP-2")
}

/// Every connector listed under any logical monitor (multi-output LMs yield multiple).
pub fn connectors_bound_in_logical(logical: &[LogicalMonitor]) -> Vec<String> {
    let mut out = Vec::new();
    for lm in logical {
        for r in &lm.5 {
            out.push(r.0.clone());
        }
    }
    out
}

pub fn classify_attachment(logical: &[LogicalMonitor]) -> &'static str {
    let bound = connectors_bound_in_logical(logical);
    let ext: Vec<_> = bound.iter().filter(|c| !is_internal_connector(c)).collect();
    let edp: Vec<_> = bound.iter().filter(|c| is_internal_connector(c)).collect();

    let result = if !ext.is_empty() && !edp.is_empty() {
        ATTACH_ALL_CONNECTED
    } else if !ext.is_empty() && edp.is_empty() {
        ATTACH_EXTERNAL_ONLY
    } else {
        ATTACH_BUILTIN_ONLY
    };

    info!(
        "classify_attachment={result}: connectors=[{}], internal=[{}], external=[{}]",
        bound.join(", "),
        edp.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
        ext.iter().map(|s| s.as_str()).collect::<Vec<_>>().join(", "),
    );
    result
}

pub fn classify_layout(logical: &[LogicalMonitor]) -> &'static str {
    let edp1 = logical
        .iter()
        .any(|lm| lm.5.iter().any(|r| r.0 == "eDP-1"));
    let edp2 = logical
        .iter()
        .any(|lm| lm.5.iter().any(|r| r.0 == "eDP-2"));

    let (result, reason) = if edp1 && !edp2 {
        (LAYOUT_JOINED, "eDP-1 present, eDP-2 absent")
    } else if edp1 && edp2 {
        let same_lm = logical.iter().any(|lm| {
            let has_edp1 = lm.5.iter().any(|r| r.0 == "eDP-1");
            let has_edp2 = lm.5.iter().any(|r| r.0 == "eDP-2");
            has_edp1 && has_edp2
        });
        if same_lm {
            (LAYOUT_MIRROR, "eDP-1 and eDP-2 share the same logical monitor")
        } else {
            (LAYOUT_JOINED, "eDP-1 and eDP-2 on separate logical monitors")
        }
    } else {
        (LAYOUT_JOINED, "no internal panels or only eDP-2")
    };

    let lm_summary: Vec<String> = logical
        .iter()
        .enumerate()
        .map(|(i, lm)| {
            let connectors: Vec<&str> = lm.5.iter().map(|r| r.0.as_str()).collect();
            format!(
                "LM[{i}]({},{} s={:.2} t={} pri={})[{}]",
                lm.0, lm.1, lm.2, lm.3, lm.4,
                connectors.join("+"),
            )
        })
        .collect();

    info!(
        "classify_layout={result}: {reason} — {}",
        lm_summary.join(", "),
    );
    result
}

/// Connector positions from logical monitors — used for layout verification (incl. multi-output LMs).
pub fn read_current_logical_rows(
    logical: &[LogicalMonitor],
) -> (HashMap<String, (i32, i32, f64, u32, bool)>, bool) {
    let mut map = HashMap::new();
    let mut corrupted = false;
    for lm in logical {
        let (x, y, sc, tr, pri) = (lm.0, lm.1, lm.2, lm.3, lm.4);
        for r in &lm.5 {
            let connector = r.0.clone();
            if map.contains_key(&connector) {
                warn!(
                    "Display: corruption detected — {} appears twice in logical monitors",
                    connector
                );
                corrupted = true;
            }
            map.insert(connector, (x, y, sc, tr, pri));
        }
    }
    (map, corrupted)
}

/// Non-internal connectors that have a resolved mode in `all_modes`, in **physical list order**
/// (Mutter’s `GetCurrentState` physical monitor order). Duplicates are skipped.
pub fn external_connectors_ordered(
    physical: &[PhysicalMonitor],
    all_modes: &HashMap<String, (String, i32, i32)>,
) -> Vec<String> {
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for (info, _, _) in physical {
        let name = info.0.as_str();
        if is_internal_connector(name) {
            continue;
        }
        if !all_modes.contains_key(&info.0) {
            continue;
        }
        if seen.insert(info.0.clone()) {
            out.push(info.0.clone());
        }
    }
    out
}

/// Logical width of an internal panel along global **X** (for placing another monitor to the right).
///
/// [`super::display::logical_size`] already maps physical size into compositor `(lw, lh)` for the
/// given rotation; the **first** tuple element is the extent along global **X** used by
/// [`super::display::build_duo_lms`] for horizontal adjacency (including left-up / right-up).
pub fn edp_logical_extent_x(pw: i32, ph: i32, scale: f64, transform: u32) -> i32 {
    let (lw, _) = super::display::logical_size(pw, ph, scale, transform);
    lw
}

#[cfg(test)]
mod external_connectors_ordered_tests {
    use std::collections::HashMap;

    use super::external_connectors_ordered;
    use super::super::display::PhysicalMonitor;

    fn phys(name: &str) -> PhysicalMonitor {
        (
            (
                name.to_string(),
                String::new(),
                String::new(),
                String::new(),
            ),
            vec![],
            HashMap::new(),
        )
    }

    #[test]
    fn orders_externals_skips_internals_and_missing_modes() {
        let physical = vec![
            phys("eDP-1"),
            phys("HDMI-A-1"),
            phys("eDP-2"),
            phys("DP-1"),
            phys("Unknown-1"),
        ];
        let mut modes = HashMap::new();
        modes.insert("eDP-1".into(), ("m0".into(), 2880, 1800));
        modes.insert("HDMI-A-1".into(), ("m1".into(), 1920, 1080));
        modes.insert("eDP-2".into(), ("m2".into(), 2880, 1800));
        modes.insert("DP-1".into(), ("m3".into(), 2560, 1440));

        let v = external_connectors_ordered(&physical, &modes);
        assert_eq!(v, vec!["HDMI-A-1", "DP-1"]);
    }

    #[test]
    fn dedupes_duplicate_physical_rows() {
        let physical = vec![phys("DP-1"), phys("DP-1")];
        let mut modes = HashMap::new();
        modes.insert("DP-1".into(), ("m".into(), 1920, 1080));

        let v = external_connectors_ordered(&physical, &modes);
        assert_eq!(v, vec!["DP-1"]);
    }

    #[test]
    fn edp_logical_extent_x_matches_logical_size_width_for_rotations() {
        use super::edp_logical_extent_x;
        use super::super::display::logical_size;
        let pw = 2880i32;
        let ph = 1800i32;
        let scale = 5.0 / 3.0;
        for transform in [0u32, 1, 2, 3] {
            let (lw, _) = logical_size(pw, ph, scale, transform);
            assert_eq!(
                edp_logical_extent_x(pw, ph, scale, transform),
                lw,
                "transform {transform}"
            );
        }
    }
}
