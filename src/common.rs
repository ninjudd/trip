use std::path::PathBuf;

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

pub fn session_dir(name: &str) -> PathBuf {
    trip_dir().join("sessions").join(name)
}

pub fn screens_dir(name: &str) -> PathBuf {
    session_dir(name).join("screens")
}

pub fn log_path(name: &str) -> PathBuf {
    session_dir(name).join("log.jsonl")
}

pub fn terminal_env_path(name: &str) -> PathBuf {
    session_dir(name).join("terminal.env")
}
