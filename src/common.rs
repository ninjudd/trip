use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub fn trip_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join(".trip")
}

pub fn socket_path() -> PathBuf {
    trip_dir().join("daemon.sock")
}

pub fn lock_path() -> PathBuf {
    trip_dir().join("daemon.lock")
}

pub fn daemon_log_path() -> PathBuf {
    trip_dir().join("daemon.log")
}

pub fn session_dir(name: &str) -> PathBuf {
    trip_dir().join("sessions").join(name)
}

pub fn log_path(name: &str) -> PathBuf {
    session_dir(name).join("log.jsonl")
}

pub fn terminal_env_path(name: &str) -> PathBuf {
    session_dir(name).join("terminal.env")
}

pub fn meta_path(name: &str) -> PathBuf {
    session_dir(name).join("meta.json")
}

/// What a session was, written at spawn and removed on clean exit. A
/// meta.json left behind therefore marks a session that died with the
/// daemon rather than on its own — the set a future `trip resume` offers.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SessionMeta {
    pub name: String,
    /// Original argv, or None when the session ran the default shell.
    pub command: Option<Vec<String>>,
    pub cwd: String,
    pub created_at: u64,
}

pub fn write_session_meta(meta: &SessionMeta) {
    if let Ok(json) = serde_json::to_string_pretty(meta) {
        let _ = std::fs::write(meta_path(&meta.name), json);
    }
}

pub fn remove_session_meta(name: &str) {
    let _ = std::fs::remove_file(meta_path(name));
}
