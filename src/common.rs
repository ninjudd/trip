use std::path::PathBuf;

pub fn drip_dir() -> PathBuf {
    let home = std::env::var("HOME").expect("HOME not set");
    PathBuf::from(home).join(".drip")
}

pub fn socket_path() -> PathBuf {
    drip_dir().join("daemon.sock")
}

pub fn lock_path() -> PathBuf {
    drip_dir().join("daemon.lock")
}

pub const DEFAULT_SCROLLBACK: usize = 100 * 1024;

pub fn session_dir(name: &str) -> PathBuf {
    drip_dir().join("sessions").join(name)
}

pub fn screens_dir(name: &str) -> PathBuf {
    session_dir(name).join("screens")
}

pub fn log_path(name: &str) -> PathBuf {
    session_dir(name).join("log.jsonl")
}
