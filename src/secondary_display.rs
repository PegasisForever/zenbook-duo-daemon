use std::{fs, thread, time::Duration};

pub fn sync_secondary_display_brightness_thread() {
    let source = "/sys/class/backlight/intel_backlight/brightness";
    let target = "/sys/class/backlight/card1-eDP-2-backlight/brightness";

    loop {
        match fs::read_to_string(source) {
            Ok(brightness) => {
                fs::write(target, brightness.trim()).ok();
            }
            Err(_) => {}
        }
        thread::sleep(Duration::from_millis(500));
    }
}

const SECONDARY_DISPLAY_PATH: &str = "/sys/class/drm/card1-eDP-2/status";

pub fn control_secondary_display(enable: bool) {
    if enable {
        fs::write(SECONDARY_DISPLAY_PATH, b"on").unwrap();
    } else {
        fs::write(SECONDARY_DISPLAY_PATH, b"off").unwrap();
    }
}

pub fn toggle_secondary_display() {
    let contents = fs::read_to_string(SECONDARY_DISPLAY_PATH).unwrap();

    if contents.trim() == "connected" {
        control_secondary_display(false);
    } else {
        control_secondary_display(true);
    }
}