//! Bluetooth keyboard support for the ASUS Zenbook Duo.
//!
//! # Why multiple `/dev/input/event*` tasks for one physical keyboard?
//!
//! For Bluetooth, the kernel exposes several `/dev/input/event*` character devices for what is
//! logically **one** ASUS Zenbook Duo Keyboard. The stable identity per node is sysfs
//! `device/uniq` (BlueZ-style MAC); see [`mac_from_evdev_sysfs`].
//!
//! **Normal typing** (letters, digits, standard modifiers) is delivered as usual `EV_KEY` events on
//! whichever node the input stack attached as the main keyboard stream.
//!
//! **ASUS vendor hotkeys** (Fn+F4 keyboard backlight, Fn+F5/F6 panel brightness, Fn+F8 swap
//! primaries, Fn+F9 mic mute, Fn+F11 emoji, Fn+F12 MyASUS, the key right of F12 for the bottom
//! display, etc.) are surfaced as `EV_ABS` / `ABS_MISC` with vendor-specific values—the same
//! encoding idea as the USB HID vendor path in `keyboard_usb`.
//!
//! **Kernel behaviour that forces our design:** those `ABS_MISC` events may be routed through one
//! of several sibling `event*` nodes—or both in quick succession—depending on connect timing, BlueZ
//! churn, and internal routing. Listening only to the first discovered node was observed in the
//! field as an intermittent **dead Fn row** (the hotkey never arrived on the fd we chose). We
//! therefore register **one blocking read loop per qualifying path** (each must advertise
//! `ABS_MISC`), not to duplicate normal key handling, but because the stack uses **parallel fds**
//! as delivery channels for the same physical keyboard.
//!
//! **How we keep this bounded:** `BtVendorHotkeyReaders` groups paths by MAC and records whether
//! GATT restore already ran; on the first `ENODEV` on **any** sibling path we tear down the **whole**
//! MAC session (abort other reader tasks, drop the GATT shutdown sender, one `bluetooth_connection_stopped`)
//! so the GATT-owner path cannot disappear while siblings keep running without LED/backlight control.
//! `suppress_twin_abs_misc_pulse` drops identical non-zero `ABS_MISC`
//! values within ~40 ms; `bt_hid_gatt_write_lock` serializes BlueZ GATT writes; on `ENODEV` from
//! `next_event` we remove this path from the map (see `start_bt_keyboard_task`). Collapsing to a
//! single reader without new evidence would risk missing vendor keys after reconnect—treat as
//! intentional design, not a smell.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    sync::{Arc, Mutex as StdMutex, OnceLock},
    time::{Duration, Instant},
};

use evdev_rs::{
    Device, DeviceWrapper as _, InputEvent, ReadFlag,
    enums::{EV_ABS, EventCode},
};
use futures::stream::StreamExt;
use inotify::{Inotify, WatchMask};
use log::{debug, info, warn};
use nix::libc;
use tokio::sync::{Mutex, broadcast};
use tokio::task::AbortHandle;
use tokio::{fs, task::spawn_blocking};
use zbus::Connection;
use zbus::fdo::ObjectManagerProxy;
use zbus::proxy;
use zbus::zvariant::{OwnedObjectPath, OwnedValue};

use crate::{
    config::Config,
    events::Event,
    idle_detection::ActivityNotifier,
    state::{KeyboardBacklightState, KeyboardStateManager},
    virtual_keyboard::VirtualKeyboard,
};

/// Per Bluetooth MAC (`uniq`): which `event*` paths currently have an `ABS_MISC` reader, and
/// whether we already ran the one-shot GATT/HID vendor restore for this connect session.
///
/// **Why a set of paths:** ASUS can expose **multiple** sibling evdev nodes for the same keyboard;
/// vendor hotkeys may arrive on any subset. We must open each `ABS_MISC` node, but run GATT restore
/// only once per session (`hid_restore_done`).
///
/// **`gatt_shutdown`:** the sending end of the oneshot watched by the GATT control task. Dropping it
/// (session teardown) closes the channel so the task exits. It lives here so removing this struct
/// always stops GATT even if the owning evdev node disappears first.
/// Handle for aborting one per-path reader task during coordinated MAC-session teardown.
struct BtReaderHandle {
    task_id: usize,
    abort: AbortHandle,
}

#[derive(Default)]
struct BtVendorHotkeyReaders {
    paths: HashSet<PathBuf>,
    hid_restore_done: bool,
    next_reader_task_id: usize,
    gatt_shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    reader_abort_handles: Vec<BtReaderHandle>,
}

/// When both sibling nodes emit the **same** non-zero `ABS_MISC` value for one physical press,
/// treat the second arrival within this window as duplicate and ignore it (see module docs).
static LAST_BT_VENDOR_MISC: StdMutex<Option<(i32, Instant)>> = StdMutex::new(None);

fn suppress_twin_abs_misc_pulse(value: i32) -> bool {
    if value == 0 {
        return false;
    }
    let now = Instant::now();
    let mut guard = LAST_BT_VENDOR_MISC.lock().unwrap();
    let duplicate = matches!(
        guard.as_ref(),
        Some((v, t)) if *v == value && now.duration_since(*t) < Duration::from_millis(40)
    );
    if !duplicate {
        *guard = Some((value, now));
    }
    duplicate
}

// ── BlueZ GATT proxies ───────────────────────────────────────────────────────

#[proxy(
    interface = "org.bluez.GattCharacteristic1",
    default_service = "org.bluez"
)]
trait GattCharacteristic {
    async fn write_value(
        &self,
        value: Vec<u8>,
        options: HashMap<String, OwnedValue>,
    ) -> zbus::Result<()>;
}

#[proxy(
    interface = "org.bluez.GattDescriptor1",
    default_service = "org.bluez"
)]
trait GattDescriptor {
    async fn read_value(&self, options: HashMap<String, OwnedValue>) -> zbus::Result<Vec<u8>>;
}

// ── GATT helpers ─────────────────────────────────────────────────────────────

/// Derive BT MAC (BlueZ path format: "FD_0C_1A_6E_05_A3") from an evdev path
/// via the kernel sysfs `uniq` attribute.
async fn mac_from_evdev_sysfs(path: &PathBuf) -> Option<String> {
    let name = path.file_name()?.to_str()?;
    let uniq = fs::read_to_string(format!("/sys/class/input/{}/device/uniq", name))
        .await
        .ok()?;
    let mac = uniq.trim().to_uppercase().replace(':', "_");
    if mac.is_empty() { None } else { Some(mac) }
}

/// Find the GATT characteristic used for vendor HID feature reports.
/// Scans BlueZ objects under `device_path`, reads the Report Reference
/// descriptor (UUID 0x2908) on each HID Report char (UUID 0x2a4d), and
/// returns the path of the one with value [0x5a, 0x03] (report-id=90, feature).
async fn find_vendor_char(
    conn: &Connection,
    device_path: &str,
) -> Result<OwnedObjectPath, Box<dyn std::error::Error + Send + Sync>> {
    const REPORT_CHAR_UUID: &str = "00002a4d-0000-1000-8000-00805f9b34fb";
    const REPORT_REF_UUID: &str = "00002908-0000-1000-8000-00805f9b34fb";

    let manager = ObjectManagerProxy::builder(conn)
        .destination("org.bluez")?
        .path("/")?
        .build()
        .await?;

    let objects = manager.get_managed_objects().await?;

    let mut candidate_chars: Vec<String> = objects
        .iter()
        .filter_map(|(path, ifaces)| {
            let s = path.as_str();
            if !s.starts_with(device_path) {
                return None;
            }
            let char_iface = ifaces.get("org.bluez.GattCharacteristic1")?;
            let uuid = format!("{:?}", char_iface.get("UUID")?);
            if uuid.contains(REPORT_CHAR_UUID) { Some(s.to_string()) } else { None }
        })
        .collect();

    candidate_chars.sort();

    let opts: HashMap<String, OwnedValue> = HashMap::new();
    for char_path in &candidate_chars {
        let desc_paths: Vec<String> = objects
            .keys()
            .filter_map(|p| {
                let s = p.as_str();
                if s.starts_with(char_path.as_str()) && s.len() > char_path.len() {
                    Some(s.to_string())
                } else {
                    None
                }
            })
            .collect();

        for desc_path in desc_paths {
            let desc_ifaces = match objects.get(&OwnedObjectPath::try_from(desc_path.as_str())?) {
                Some(i) => i,
                None => continue,
            };
            let desc_iface = match desc_ifaces.get("org.bluez.GattDescriptor1") {
                Some(i) => i,
                None => continue,
            };
            let uuid = match desc_iface.get("UUID") {
                Some(u) => format!("{:?}", u),
                None => continue,
            };
            if !uuid.contains(REPORT_REF_UUID) {
                continue;
            }

            let desc_proxy = GattDescriptorProxy::builder(conn)
                .path(desc_path.as_str())?
                .build()
                .await?;
            match desc_proxy.read_value(opts.clone()).await {
                Ok(val) if val == vec![0x5a, 0x03] => {
                    return Ok(OwnedObjectPath::try_from(char_path.as_str())?);
                }
                _ => continue,
            }
        }
    }

    Err(format!("Could not find vendor HID feature char under {}", device_path).into())
}

/// BlueZ allows only one GATT write at a time per peripheral. `/dev/input` churn can briefly
/// spawn overlapping BT tasks (same MAC) during dock/undock; without serialization + retry,
/// `org.bluez.Error.InProgress` causes **fn_lock** (and LED sync) to silently fail so Fn+F8 etc.
/// stop working until reconnect.
static BT_HID_GATT_WRITE_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn bt_hid_gatt_write_lock() -> &'static Mutex<()> {
    BT_HID_GATT_WRITE_LOCK.get_or_init(|| Mutex::new(()))
}

async fn bt_hid_gatt_write(
    proxy: &GattCharacteristicProxy<'_>,
    payload: Vec<u8>,
    what: &'static str,
) {
    let _serial = bt_hid_gatt_write_lock().lock().await;
    let mut delay = Duration::from_millis(45);
    for attempt in 1..=12u32 {
        match proxy.write_value(payload.clone(), HashMap::new()).await {
            Ok(()) => return,
            Err(e) => {
                let s = e.to_string().to_lowercase();
                let busy =
                    s.contains("in progress") || s.contains("inprogress") || s.contains("busy");
                if busy && attempt < 12 {
                    tokio::time::sleep(delay).await;
                    delay = delay.saturating_mul(2).min(Duration::from_millis(400));
                    continue;
                }
                warn!("Failed BT HID GATT {what}: {e}");
                return;
            }
        }
    }
}

async fn send_bt_backlight_state(proxy: &GattCharacteristicProxy<'_>, state: KeyboardBacklightState) {
    let level: u8 = match state {
        KeyboardBacklightState::Off => 0,
        KeyboardBacklightState::Low => 1,
        KeyboardBacklightState::Medium => 2,
        KeyboardBacklightState::High => 3,
    };
    bt_hid_gatt_write(proxy, vec![0xba, 0xc5, 0xc4, level], "backlight level").await;
}

async fn send_bt_mic_mute_state(proxy: &GattCharacteristicProxy<'_>, muted: bool) {
    bt_hid_gatt_write(
        proxy,
        vec![0xd0, 0x7c, if muted { 0x01 } else { 0x00 }],
        "mic mute led",
    )
    .await;
}

/// Set keyboard fn_lock state via BT GATT.
/// fn_lock=true  → multimedia keys are default (Fn needed for F1-F12).
/// fn_lock=false → F1-F12 are default (Fn needed for multimedia).
async fn send_bt_fn_lock_state(proxy: &GattCharacteristicProxy<'_>, fn_lock: bool) {
    // Mirror of the USB HID feature report 5a d0 4e [00/01], without the 0x5a report-ID prefix.
    let value: u8 = if fn_lock { 0x00 } else { 0x01 };
    bt_hid_gatt_write(proxy, vec![0xd0, 0x4e, value], "fn_lock").await;
}

async fn restore_bt_hid_vendor_state_after_connect(
    char_proxy: &GattCharacteristicProxy<'_>,
    fn_lock: bool,
    state_manager: &KeyboardStateManager,
) {
    // BlueZ / firmware sometimes ignores the first feature writes after link or input churn.
    for round in 0..4 {
        if round > 0 {
            tokio::time::sleep(Duration::from_millis(450)).await;
        }
        send_bt_fn_lock_state(char_proxy, fn_lock).await;
        send_bt_backlight_state(char_proxy, state_manager.get_keyboard_backlight()).await;
        send_bt_mic_mute_state(char_proxy, state_manager.get_mic_mute_led()).await;
    }
}

// ── Monitor / task startup ───────────────────────────────────────────────────

pub fn start_bt_keyboard_monitor_task(
    config: &Config,
    event_sender: broadcast::Sender<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
) {
    // Scans existing `/dev/input/event*` nodes, then watches for new ones. Each candidate path is
    // handed to `try_start_bt_keyboard_task`, which may spawn **one reader per ABS_MISC sibling**
    // for the same Bluetooth MAC (see module-level docs).
    let config_clone = config.clone();
    let virtual_keyboard_clone = virtual_keyboard.clone();
    let state_manager_clone = state_manager.clone();
    // BT uniq (MAC key) → active ABS_MISC reader paths + whether HID/GATT restore ran this session.
    let abs_misc_vendor_claimed: Arc<Mutex<HashMap<String, BtVendorHotkeyReaders>>> =
        Arc::new(Mutex::new(HashMap::new()));

    tokio::spawn(async move {
        let mut entries = match fs::read_dir("/dev/input").await {
            Ok(entries) => entries,
            Err(e) => {
                warn!("Failed to read /dev/input: {}", e);
                return;
            }
        };

        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            try_start_bt_keyboard_task(
                &config_clone,
                path,
                event_sender.subscribe(),
                virtual_keyboard_clone.clone(),
                state_manager_clone.clone(),
                activity_notifier.clone(),
                abs_misc_vendor_claimed.clone(),
            )
            .await;
        }

        let inotify = Inotify::init().expect("Failed to initialize inotify");
        inotify
            .watches()
            .add("/dev/input/", WatchMask::CREATE)
            .expect("Failed to add inotify watch");

        let mut buffer = [0; 1024];
        let mut stream = inotify.into_event_stream(&mut buffer).unwrap();

        while let Some(event_result) = stream.next().await {
            if let Ok(event) = event_result {
                if let Some(name) = event.name {
                    if event.mask.contains(inotify::EventMask::CREATE) {
                        if name.to_str().unwrap_or("").starts_with("event") {
                            let path = PathBuf::from("/dev/input/").join(name);
                            try_start_bt_keyboard_task(
                                &config_clone,
                                path,
                                event_sender.subscribe(),
                                virtual_keyboard_clone.clone(),
                                state_manager_clone.clone(),
                                activity_notifier.clone(),
                                abs_misc_vendor_claimed.clone(),
                            )
                            .await;
                        }
                    }
                }
            }
        }
    });
}

async fn try_start_bt_keyboard_task(
    config: &Config,
    path: PathBuf,
    event_receiver: broadcast::Receiver<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
    activity_notifier: ActivityNotifier,
    abs_misc_vendor_claimed: Arc<Mutex<HashMap<String, BtVendorHotkeyReaders>>>,
) {
    if let Ok(metadata) = fs::metadata(&path).await {
        if metadata.is_dir() {
            return;
        }
    } else {
        return;
    }

    if let Some(fname) = path.file_name().and_then(|n| n.to_str()) {
        if !fname.starts_with("event") {
            return;
        }
    } else {
        return;
    }

    let path_clone = path.clone();
    let opened = spawn_blocking(move || {
        let file = std::fs::File::open(path_clone).ok()?;
        evdev_rs::Device::new_from_file(file).ok()
    })
    .await
    .unwrap();

    let Some(pre_input) = opened else {
        return;
    };

    if pre_input.name() != Some("ASUS Zenbook Duo Keyboard") {
        return;
    }

    // `uniq` is stable (`fd:…` → BlueZ `dev_FD_…`). See crate-level `keyboard_bt` docs: we may keep
    // **several** `event*` paths open per MAC because `ABS_MISC` vendor hotkeys can land on any
    // sibling node; skipping nodes without `ABS_MISC` is correct (not a vendor channel).
    if !pre_input.has(EventCode::EV_ABS(EV_ABS::ABS_MISC)) {
        debug!(
            "Skipping ASUS BT evdev {} (no ABS_MISC — not vendor hotkey channel)",
            path.display()
        );
        return;
    }

    drop(pre_input);

    // Let `/dev/input` + BlueZ settle after USB/BT role changes. Do **not** gate on
    // `is_usb_keyboard_attached()` here: ordering vs nusb can skip BT startup and leave no
    // ABS_MISC reader at all (broken Fn row) even though `uniq` is unchanged.
    tokio::time::sleep(Duration::from_millis(280)).await;

    if fs::metadata(&path).await.is_err() {
        return;
    }

    let path_clone2 = path.clone();
    let input = match spawn_blocking(move || {
        let file = std::fs::File::open(path_clone2).ok()?;
        evdev_rs::Device::new_from_file(file).ok()
    })
    .await
    {
        Ok(Some(d)) => d,
        _ => return,
    };

    if input.name() != Some("ASUS Zenbook Duo Keyboard") {
        return;
    }
    if !input.has(EventCode::EV_ABS(EV_ABS::ABS_MISC)) {
        debug!(
            "Skipping ASUS BT evdev {} after debounce (lost ABS_MISC?)",
            path.display()
        );
        return;
    }

    let dedupe_key = match mac_from_evdev_sysfs(&path).await {
        Some(m) => m,
        None => format!("no-uniq:{}", path.display()),
    };

    let first_path_in_session = 'reg: loop {
        for attempt in 0..8u32 {
            let mut map = abs_misc_vendor_claimed.lock().await;
            let entry = map.entry(dedupe_key.clone()).or_default();
            if entry.paths.contains(&path) {
                drop(map);
                if attempt + 1 < 8 {
                    tokio::time::sleep(Duration::from_millis(100)).await;
                    continue;
                }
                info!(
                    "Skipping BT vendor-hotkey {:?}: path still listed active (stale claim / stuck inotify)",
                    path
                );
                return;
            }
            let run_hid_restore = !entry.hid_restore_done;
            let gatt_shutdown_rx = if run_hid_restore {
                entry.hid_restore_done = true;
                let (tx, rx) = tokio::sync::oneshot::channel();
                entry.gatt_shutdown = Some(tx);
                Some(rx)
            } else {
                None
            };
            let first_path_in_session = entry.paths.is_empty();
            entry.paths.insert(path.clone());
            let reader_task_id = entry.next_reader_task_id;
            entry.next_reader_task_id = entry.next_reader_task_id.saturating_add(1);

            let reader_jh = start_bt_keyboard_task(
                config.clone(),
                path,
                input,
                event_receiver,
                virtual_keyboard.clone(),
                state_manager.clone(),
                abs_misc_vendor_claimed.clone(),
                dedupe_key.clone(),
                run_hid_restore,
                gatt_shutdown_rx,
                reader_task_id,
            );
            entry.reader_abort_handles.push(BtReaderHandle {
                task_id: reader_task_id,
                abort: reader_jh.abort_handle(),
            });
            drop(map);
            break 'reg first_path_in_session;
        }
        return;
    };

    if first_path_in_session {
        state_manager.bluetooth_connection_started();
        activity_notifier.notify();
    }
}

fn start_bt_keyboard_task(
    config: Config,
    path: PathBuf,
    keyboard: Device,
    event_receiver: broadcast::Receiver<Event>,
    virtual_keyboard: Arc<Mutex<VirtualKeyboard>>,
    state_manager: KeyboardStateManager,
    abs_misc_vendor_claimed: Arc<Mutex<HashMap<String, BtVendorHotkeyReaders>>>,
    vendor_channel_dedupe_key: String,
    run_hid_restore: bool,
    gatt_shutdown_rx: Option<tokio::sync::oneshot::Receiver<()>>,
    reader_task_id: usize,
) -> tokio::task::JoinHandle<()> {
    info!(
        "Bluetooth connected on {} (vendor hotkeys / ABS_MISC){}",
        path.display(),
        if run_hid_restore {
            " — HID/GATT restore task will run"
        } else {
            ""
        }
    );

    // GATT: one restore + one event-driven writer per MAC connect session (first ABS_MISC path wins).
    // Sibling paths set `run_hid_restore = false` and drop their `event_receiver`; they only read
    // `ABS_MISC`. The matching `oneshot::Sender` lives in `BtVendorHotkeyReaders::gatt_shutdown`
    // so removing that map entry (coordinated teardown) always closes this control loop.
    if let Some(mut shutdown_rx) = gatt_shutdown_rx {
        let state_manager_ctrl = state_manager.clone();
        let fn_lock = config.fn_lock;
        let path_for_control = path.clone();
        let mut event_receiver_gatt = event_receiver;
        tokio::spawn(async move {
            let mac = match mac_from_evdev_sysfs(&path_for_control).await {
                Some(m) => m,
                None => {
                    warn!(
                        "Could not derive BT MAC from {:?} — backlight/mic LED won't work",
                        path_for_control
                    );
                    return;
                }
            };

            let device_path = format!("/org/bluez/hci0/dev_{}", mac);

            let conn = match Connection::system().await {
                Ok(c) => c,
                Err(e) => {
                    warn!("D-Bus connection failed: {} — backlight/mic LED won't work", e);
                    return;
                }
            };

            let char_path = match find_vendor_char(&conn, &device_path).await {
                Ok(p) => {
                    info!("BT vendor char found: {}", p.as_str());
                    p
                }
                Err(e) => {
                    warn!("Could not find BT vendor char: {} — backlight/mic LED won't work", e);
                    return;
                }
            };

            let char_proxy = match GattCharacteristicProxy::builder(&conn)
                .path(char_path.as_str())
                .unwrap()
                .build()
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    warn!("Failed to build GATT proxy: {} — backlight/mic LED won't work", e);
                    return;
                }
            };

            // Restore state on connect (mirrors USB behaviour)
            restore_bt_hid_vendor_state_after_connect(&char_proxy, fn_lock, &state_manager_ctrl)
                .await;

            loop {
                tokio::select! {
                    _ = &mut shutdown_rx => {
                        info!("Bluetooth control task shutting down");
                        break;
                    }
                    result = event_receiver_gatt.recv() => {
                        match result {
                            Ok(Event::Backlight(state)) => {
                                send_bt_backlight_state(&char_proxy, state).await;
                            }
                            Ok(Event::MicMuteLed(enabled)) => {
                                send_bt_mic_mute_state(&char_proxy, enabled).await;
                            }
                            Ok(_) => {}
                            Err(broadcast::error::RecvError::Lagged(_)) => {
                                continue;
                            }
                            Err(broadcast::error::RecvError::Closed) => {
                                break;
                            }
                        }
                    }
                }
            }
        });
    } else {
        drop(event_receiver);
    }

    let keyboard = Arc::new(std::sync::Mutex::new(keyboard));
    let claimed_cleanup = abs_misc_vendor_claimed.clone();
    let dedupe_cleanup = vendor_channel_dedupe_key.clone();
    tokio::spawn(async move {
        loop {
            let keyboard_clone = keyboard.clone();
            let result = spawn_blocking(move || {
                let kb = keyboard_clone.lock().unwrap();
                kb.next_event(ReadFlag::NORMAL | ReadFlag::BLOCKING)
            })
            .await
            .unwrap();

            match result {
                Ok((_status, event)) => {
                    parse_keyboard_event(event, &config, &virtual_keyboard, &state_manager).await;
                }
                Err(e) => {
                    if let Some(libc::ENODEV) = e.raw_os_error() {
                        info!("Bluetooth device disconnected. Exiting task.");
                        let session = {
                            let mut map = claimed_cleanup.lock().await;
                            map.remove(&dedupe_cleanup)
                        };
                        if let Some(session) = session {
                            for h in session.reader_abort_handles {
                                if h.task_id != reader_task_id {
                                    h.abort.abort();
                                }
                            }
                            state_manager.bluetooth_connection_stopped();
                            virtual_keyboard.lock().await.release_all_keys();
                        }
                        return;
                    } else {
                        warn!("Failed to read event: {:?}", e);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
        }
    })
}

// ── Key event parsing ────────────────────────────────────────────────────────
//
// `ABS_MISC` values mirror the USB vendor byte (see `keyboard_usb::parse_keyboard_data`).
// Fn+F7 is not emitted here: display cycling is **Super+P** on the main keyboard evdev node.
// Fn+F10 pairing is firmware — unmapped here on purpose.

async fn parse_keyboard_event(
    event: InputEvent,
    config: &Config,
    virtual_keyboard: &Arc<Mutex<VirtualKeyboard>>,
    state_manager: &KeyboardStateManager,
) {
    if event.event_code == EventCode::EV_ABS(EV_ABS::ABS_MISC) {
        if suppress_twin_abs_misc_pulse(event.value) {
            return;
        }
        match event.value {
            0 => {
                debug!("No key pressed");
                virtual_keyboard.lock().await.release_all_keys();
            }
            199 => {
                // Fn+F4 — keyboard backlight
                debug!("Backlight key pressed (Fn+F4)");
                config
                    .keyboard_backlight_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            16 => {
                // Fn+F5
                debug!("Brightness down key pressed (Fn+F5)");
                config
                    .brightness_down_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            32 => {
                // Fn+F6
                debug!("Brightness up key pressed (Fn+F6)");
                config
                    .brightness_up_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            156 => {
                // Fn+F8 — swap primaries
                debug!("Swap up down display key pressed (Fn+F8)");
                config
                    .swap_up_down_display_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            124 => {
                // Fn+F9
                debug!("Microphone mute key pressed (Fn+F9)");
                config
                    .microphone_mute_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            126 => {
                // Fn+F11
                debug!("Emoji picker key pressed (Fn+F11)");
                config
                    .emoji_picker_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            134 => {
                // Fn+F12 — ASUS / MyASUS
                debug!("MyASUS key pressed (Fn+F12)");
                config
                    .myasus_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            106 => {
                // Key right of F12 — bottom display toggle
                debug!("Toggle secondary display key pressed (key right of F12)");
                config
                    .toggle_secondary_display_key
                    .execute(&virtual_keyboard, &state_manager)
                    .await;
            }
            _ => {
                debug!("Unknown key pressed: {:?}", event);
                virtual_keyboard.lock().await.release_all_keys();
            }
        }
    }
}
