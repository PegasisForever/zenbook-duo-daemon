//! USB vendor interrupt battery reports (`0x5a 0x3d pct status …` on endpoint 0x85).

/// Vendor report prefix (same channel as Fn special keys).
pub const USB_REPORT_VENDOR: u8 = 0x5a;
/// Battery sub-report (GET `0x3d` does not update; device pushes on interrupt IN).
pub const USB_BATTERY_SUBTYPE: u8 = 0x3d;
/// Observed while SOC climbs during side-USB charge.
pub const USB_BATTERY_STATUS_CHARGING: u8 = 0x09;
/// Observed once at end of charge (often still 100% in byte 2).
pub const USB_BATTERY_STATUS_FULL: u8 = 0x01;

/// Treat as “full” for notifications when firmware has not sent `FULL` yet (aged cells).
pub const BATTERY_FULL_PCT_THRESHOLD: u8 = 95;

pub fn usb_battery_charging(status: u8, pct: u8) -> bool {
    status == USB_BATTERY_STATUS_CHARGING && pct < BATTERY_FULL_PCT_THRESHOLD
}

pub fn usb_battery_full(status: u8, pct: u8) -> bool {
    status == USB_BATTERY_STATUS_FULL || pct >= BATTERY_FULL_PCT_THRESHOLD
}

/// Parse vendor battery bytes from USB interrupt IN or GET_REPORT payloads.
pub fn parse_usb_battery_report(data: &[u8]) -> Option<(u8, u8)> {
    match data {
        [r0, r1, pct, status, ..]
            if *r0 == USB_REPORT_VENDOR
                && *r1 == USB_BATTERY_SUBTYPE
                && *pct <= 100 =>
        {
            Some((*pct, *status))
        }
        _ => None,
    }
}

/// Ignore empty HID GET_REPORT echoes (`5a 3d 00 00`) on pogo when no fresh sample exists.
pub fn plausible_usb_battery_sample(pct: u8, status: u8) -> bool {
    !(pct == 0 && status == 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vendor_battery_report() {
        assert_eq!(
            parse_usb_battery_report(&[0x5a, 0x3d, 72, 0x09, 0, 0]),
            Some((72, 0x09))
        );
        assert_eq!(parse_usb_battery_report(&[0x5a, 0x3d, 100, 0x01, 0, 0]), Some((100, 0x01)));
    }

    #[test]
    fn charging_status_while_below_full_threshold() {
        assert!(usb_battery_charging(USB_BATTERY_STATUS_CHARGING, 94));
        assert!(!usb_battery_charging(USB_BATTERY_STATUS_FULL, 100));
        assert!(!usb_battery_charging(USB_BATTERY_STATUS_CHARGING, 96));
    }

    #[test]
    fn full_at_threshold_or_firmware_flag() {
        assert!(usb_battery_full(USB_BATTERY_STATUS_CHARGING, 95));
        assert!(usb_battery_full(USB_BATTERY_STATUS_FULL, 80));
    }

    #[test]
    fn rejects_empty_get_report_echo() {
        assert!(!plausible_usb_battery_sample(0, 0));
        assert!(plausible_usb_battery_sample(78, 0));
    }
}
