//! Classify ASUS Zenbook Duo keyboard USB connections by xhci hub port.
//!
//! On Linux the sysfs `devpath` attribute **is** the hub port index (`lsusb -t` “Port 004” →
//! devpath `4`, sysfs node `3-4`). We read `devpath` because nusb gives bus + device address,
//! not the `3-N` node name, and devpath stays stable when the device number changes (007 → 008).
//!
//! On UX8406 hardware the same `0b05:1bf2` device appears on different internal ports:
//! - port **4** — side USB (charge / data while undocked)
//! - port **6** — pogo contacts when the keyboard sits on the bottom panel

use log::{info, warn};
use std::path::Path;
use std::sync::OnceLock;
use std::time::Duration;

use crate::config::UsbKeyboardPortsConfig;

static POGO_DEVPATHS: OnceLock<Vec<String>> = OnceLock::new();
static USB_KEYBOARD_IDS: OnceLock<(u16, u16)> = OnceLock::new();

const SYSFS_DEVPATH_RETRY_COUNT: u32 = 25;
const SYSFS_DEVPATH_RETRY_DELAY_MS: u64 = 40;

/// Install pogo devpath list from config (call once at daemon startup).
pub fn init_pogo_devpaths(config: &UsbKeyboardPortsConfig) {
    let _ = POGO_DEVPATHS.set(config.pogo_dock_hub_ports.clone());
}

/// USB vendor/product used for sysfs fallback when `devnum` is not populated yet.
pub fn init_usb_keyboard_ids(vendor_id: u16, product_id: u16) {
    let _ = USB_KEYBOARD_IDS.set((vendor_id, product_id));
}

fn pogo_devpaths() -> &'static [String] {
    POGO_DEVPATHS
        .get()
        .map(|v| v.as_slice())
        .unwrap_or(&[])
}

fn usb_keyboard_ids() -> Option<(u16, u16)> {
    USB_KEYBOARD_IDS.get().copied()
}

/// `true` when the keyboard is on the bottom-panel pogo USB port (clamshell / secondary policy).
pub fn is_pogo_dock_devpath(devpath: &str) -> bool {
    pogo_devpaths().iter().any(|p| p == devpath)
}

fn sysfs_devpath_for_usb_device_once(bus_id: &str, device_address: u8) -> Option<String> {
    let busnum = parse_bus_id(bus_id)?;
    let entries = std::fs::read_dir("/sys/bus/usb/devices").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(entry_bus) = read_sysfs_trimmed(path.join("busnum")) else {
            continue;
        };
        let Some(devnum) = read_sysfs_trimmed(path.join("devnum")) else {
            continue;
        };
        if entry_bus == busnum.to_string() && devnum == device_address.to_string() {
            return read_sysfs_trimmed(path.join("devpath"));
        }
    }
    None
}

/// When `devnum` is not yet in sysfs, match the keyboard by VID/PID on the same bus.
fn sysfs_devpath_for_vendor_product_on_bus(
    bus_id: &str,
    device_address: u8,
) -> Option<String> {
    let (vendor_id, product_id) = usb_keyboard_ids()?;
    let busnum = parse_bus_id(bus_id)?;
    let vid = format!("{:04x}", vendor_id);
    let pid = format!("{:04x}", product_id);

    let mut matches: Vec<(String, String)> = Vec::new();
    let entries = std::fs::read_dir("/sys/bus/usb/devices").ok()?;
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let Some(entry_bus) = read_sysfs_trimmed(path.join("busnum")) else {
            continue;
        };
        if entry_bus != busnum.to_string() {
            continue;
        }
        let Some(entry_vid) = read_sysfs_trimmed(path.join("idVendor")) else {
            continue;
        };
        let Some(entry_pid) = read_sysfs_trimmed(path.join("idProduct")) else {
            continue;
        };
        if entry_vid != vid || entry_pid != pid {
            continue;
        }
        let Some(devpath) = read_sysfs_trimmed(path.join("devpath")) else {
            continue;
        };
        let devnum = read_sysfs_trimmed(path.join("devnum")).unwrap_or_default();
        matches.push((devpath, devnum));
    }

    match matches.len() {
        0 => None,
        1 => {
            let (devpath, devnum) = &matches[0];
            warn!(
                "USB keyboard on bus {} address {}: devnum sysfs lookup missed; \
                 using VID/PID fallback → devpath={devpath} devnum={devnum}",
                bus_id, device_address
            );
            Some(devpath.clone())
        }
        _ => {
            if let Some((devpath, _devnum)) = matches
                .iter()
                .find(|(_, devnum)| devnum == &device_address.to_string())
            {
                warn!(
                    "USB keyboard on bus {} address {}: multiple VID/PID matches; \
                     picked devnum match → devpath={devpath}",
                    bus_id, device_address
                );
                return Some(devpath.clone());
            }
            warn!(
                "USB keyboard on bus {} address {}: multiple VID/PID matches on bus; \
                 cannot pick devpath uniquely",
                bus_id, device_address
            );
            None
        }
    }
}

fn parse_bus_id(bus_id: &str) -> Option<u8> {
    bus_id.parse::<u16>().ok().and_then(|n| u8::try_from(n).ok())
}

fn read_sysfs_trimmed(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path).ok().map(|s| s.trim().to_string())
}

fn classify_devpath(bus_id: &str, device_address: u8, devpath: &str) -> bool {
    let pogo = is_pogo_dock_devpath(devpath);
    info!(
        "USB keyboard on bus {} address {} → devpath={devpath} pogo_dock={pogo}",
        bus_id, device_address
    );
    pogo
}

/// Resolve whether a connected keyboard is on the pogo dock port (async retries for sysfs race).
pub async fn keyboard_on_pogo_dock_async(bus_id: &str, device_address: u8) -> bool {
    for attempt in 0..SYSFS_DEVPATH_RETRY_COUNT {
        if let Some(devpath) = sysfs_devpath_for_usb_device_once(bus_id, device_address) {
            return classify_devpath(bus_id, device_address, &devpath);
        }
        if attempt + 1 < SYSFS_DEVPATH_RETRY_COUNT {
            tokio::time::sleep(Duration::from_millis(SYSFS_DEVPATH_RETRY_DELAY_MS)).await;
        }
    }

    if let Some(devpath) = sysfs_devpath_for_vendor_product_on_bus(bus_id, device_address) {
        return classify_devpath(bus_id, device_address, &devpath);
    }

    warn!(
        "USB keyboard on bus {} address {}: could not read sysfs devpath after {} attempts — treating as not pogo-docked",
        bus_id, device_address, SYSFS_DEVPATH_RETRY_COUNT
    );
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_bus_id_decimal() {
        assert_eq!(parse_bus_id("003"), Some(3));
        assert_eq!(parse_bus_id("3"), Some(3));
    }

    #[test]
    fn pogo_devpath_matching() {
        let _ = POGO_DEVPATHS.set(vec!["6".to_string(), "8".to_string()]);
        assert!(is_pogo_dock_devpath("6"));
        assert!(is_pogo_dock_devpath("8"));
        assert!(!is_pogo_dock_devpath("4"));
    }
}
