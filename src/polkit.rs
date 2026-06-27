//! Polkit authorization for system D-Bus entry points (see `org.zenbook.duo.policy`).

use std::collections::HashMap;
use std::io;

use log::warn;
use zbus::fdo;
use zbus::zvariant::OwnedValue;
use zbus::Connection;

pub const ACTION_REGISTER_SESSION: &str = "org.zenbook.duo.register-session";
pub const ACTION_OPERATOR: &str = "org.zenbook.duo.operator";
pub const ACTION_RESUME_DISPLAY_APPLIES: &str = "org.zenbook.duo.resume-display-applies";

const POLKIT_DESTINATION: &str = "org.freedesktop.PolicyKit1";
const POLKIT_PATH: &str = "/org/freedesktop/PolicyKit1/Authority";
const POLKIT_INTERFACE: &str = "org.freedesktop.PolicyKit1.Authority";

/// Field index of `starttime` in `/proc/[pid]/stat` after the `) ` that closes `comm`.
const PROC_STAT_STARTTIME_FIELD: usize = 19;

async fn proc_pid_start_time_ticks(pid: u32) -> io::Result<u64> {
    let path = format!("/proc/{pid}/stat");
    let data = tokio::fs::read_to_string(&path).await?;
    let after_comm = data
        .rfind(')')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidData, "no ')' in /proc stat"))?;
    let fields: Vec<&str> = data[after_comm + 2..]
        .split_whitespace()
        .collect();
    let s = fields.get(PROC_STAT_STARTTIME_FIELD).ok_or_else(|| {
        io::Error::new(
            io::ErrorKind::InvalidData,
            "short /proc stat after comm",
        )
    })?;
    s.parse::<u64>()
        .map_err(|e| io::Error::new(io::ErrorKind::InvalidData, e))
}

/// Returns whether Polkit authorizes `action_id` for the Unix process `pid`.
pub async fn check_authorization(bus: &Connection, pid: u32, action_id: &str) -> fdo::Result<()> {
    let start_time = proc_pid_start_time_ticks(pid).await.map_err(|e| {
        fdo::Error::Failed(format!(
            "cannot read process start time for pid {pid} (Polkit subject): {e}"
        ))
    })?;

    let mut subject_details: HashMap<String, OwnedValue> = HashMap::new();
    subject_details.insert("pid".into(), OwnedValue::from(pid));
    subject_details.insert("start-time".into(), OwnedValue::from(start_time));

    let subject = ("unix-process", subject_details);

    let details: HashMap<String, String> = HashMap::new();
    let flags: u32 = 0;
    let cancellation_id = "";

    let reply = bus
        .call_method(
            Some(POLKIT_DESTINATION),
            POLKIT_PATH,
            Some(POLKIT_INTERFACE),
            "CheckAuthorization",
            &(subject, action_id, details, flags, cancellation_id),
        )
        .await
        .map_err(|e| {
            fdo::Error::Failed(format!(
                "Polkit CheckAuthorization failed (is polkit running? install org.zenbook.duo.policy?): {e}"
            ))
        })?;

    let (is_authorized, is_challenge, _detail_map): (bool, bool, HashMap<String, String>) = reply
        .body()
        .deserialize()
        .map_err(|e| fdo::Error::Failed(format!("Polkit reply decode failed: {e}")))?;

    if is_authorized {
        return Ok(());
    }

    if is_challenge {
        warn!("Polkit: action {action_id} for pid {pid} requires interactive authentication (no agent on system bus)");
        return Err(fdo::Error::AuthFailed(
            "Polkit requires interactive authentication for this action".into(),
        ));
    }

    Err(fdo::Error::AccessDenied(format!(
        "Polkit denied action {action_id} for pid {pid} (not an active local session, or policy does not allow this)"
    )))
}
