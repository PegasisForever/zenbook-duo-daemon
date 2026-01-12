use std::{
    ffi::CString, fs, io::BufReader, os::unix::net::UnixStream, path::PathBuf, thread,
    time::Duration,
};

use log::{info, warn};
use pulseaudio::protocol::{self, DEFAULT_SOURCE, ProtocolError, SubscriptionEvent};
use users::{get_user_by_uid, os::unix::UserExt as _};

use crate::state::KeyboardStateManager;

pub fn start_listen_mute_state_thread(state_manager: KeyboardStateManager) {
    thread::spawn(move || {
        loop {
            if let Some((uid, pa_socket_path)) = find_pulseaudio_socket_path() {
                info!("Found pulseaudio socket path: {:?}", pa_socket_path);
                if let Err(e) = listen_mute_state(pa_socket_path, uid, state_manager.clone()) {
                    warn!("Error listening to mute state: {:?}", e);
                }

                state_manager.set_mic_mute_led(false);
            }
            thread::sleep(Duration::from_secs(1));
        }
    });
}

fn find_pulseaudio_socket_path() -> Option<(u32, PathBuf)> {
    if let Ok(entries) = fs::read_dir("/run/user") {
        for entry in entries.flatten() {
            if let Ok(file_type) = entry.file_type() {
                if file_type.is_dir() {
                    let uid_folder = entry.file_name();
                    if let Ok(uid) = uid_folder.to_string_lossy().parse::<u32>() {
                        if uid < 1000 || uid > 2000 {
                            continue; // ignore system users
                        }
                        if get_user_by_uid(uid).is_none() {
                            continue; // ignore users that are not found
                        }
                        let pa_socket_path = PathBuf::from(format!(
                            "/run/user/{}/pulse/native",
                            uid_folder.to_string_lossy()
                        ));
                        if pa_socket_path.exists() {
                            return Some((uid, pa_socket_path));
                        }
                    }
                }
            }
        }
    }
    None
}

fn listen_mute_state(
    pa_socket_path: PathBuf,
    uid: u32,
    state_manager: KeyboardStateManager,
) -> Result<(), ProtocolError> {
    let user = get_user_by_uid(uid).unwrap();
    let home_dir = user.home_dir();
    let cookie_path = home_dir.join(".config/pulse/cookie");

    if let Ok(cookie) = std::fs::read(&cookie_path) {
        let mut subscription_client = PulseAudioClient::new(&pa_socket_path, cookie.clone())?;
        let mut get_muted_client = PulseAudioClient::new(&pa_socket_path, cookie)?;

        subscription_client.subscribe_source_events(|_| {
            let is_muted = get_muted_client.get_is_default_source_muted()?;
            state_manager.set_mic_mute_led(is_muted);
            Ok(())
        })?;
    } else {
        warn!("Could not read pulseaudio cookie file: {:?}", cookie_path);
    }

    Ok(())
}

struct PulseAudioClient {
    sock: BufReader<UnixStream>,
    seq: u32,
    protocol_version: u16,
}

impl PulseAudioClient {
    pub fn new(socket_path: &PathBuf, cookie: Vec<u8>) -> Result<Self, ProtocolError> {
        let mut sock = BufReader::new(UnixStream::connect(socket_path).unwrap());

        let auth = pulseaudio::protocol::AuthParams {
            version: pulseaudio::protocol::MAX_VERSION,
            supports_shm: false,
            supports_memfd: false,
            cookie: cookie,
        };

        protocol::write_command_message(
            sock.get_mut(),
            0,
            &protocol::Command::Auth(auth),
            protocol::MAX_VERSION,
        )?;

        // get protocol version
        let (_, auth_reply) =
            protocol::read_reply_message::<protocol::AuthReply>(&mut sock, protocol::MAX_VERSION)?;
        let protocol_version = std::cmp::min(protocol::MAX_VERSION, auth_reply.version);

        // set client name
        let mut props = protocol::Props::new();
        props.set(
            protocol::Prop::ApplicationName,
            CString::new("zenbook-duo-daemon").unwrap(),
        );
        protocol::write_command_message(
            sock.get_mut(),
            1,
            &protocol::Command::SetClientName(props),
            protocol_version,
        )?;
        let _ = protocol::read_reply_message::<protocol::SetClientNameReply>(
            &mut sock,
            protocol_version,
        )?;

        Ok(Self {
            sock,
            seq: 2,
            protocol_version,
        })
    }

    pub fn subscribe_source_events<F>(&mut self, mut callback: F) -> Result<(), ProtocolError>
    where
        F: FnMut(SubscriptionEvent) -> Result<(), ProtocolError>,
    {
        protocol::write_command_message(
            self.sock.get_mut(),
            self.seq,
            &protocol::Command::Subscribe(protocol::SubscriptionMask::SOURCE),
            self.protocol_version,
        )?;
        self.seq += 1;

        // The first reply is just an ACK.
        let _ = protocol::read_ack_message(&mut self.sock)?;

        loop {
            let (_, event) = protocol::read_command_message(&mut self.sock, self.protocol_version)?;

            match event {
                protocol::Command::SubscribeEvent(event) => {
                    callback(event)?;
                }
                _ => {}
            }
        }
    }

    pub fn get_is_default_source_muted(&mut self) -> Result<bool, ProtocolError> {
        protocol::write_command_message(
            self.sock.get_mut(),
            self.seq,
            &protocol::Command::GetSourceInfo(protocol::GetSourceInfo {
                index: None,
                name: Some(DEFAULT_SOURCE.into()),
            }),
            self.protocol_version,
        )?;
        self.seq += 1;

        let (_, response) = protocol::read_reply_message::<protocol::SourceInfo>(
            &mut self.sock,
            self.protocol_version,
        )?;
        Ok(response.muted)
    }
}
