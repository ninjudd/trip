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

pub fn daemon_record_path() -> PathBuf {
    trip_dir().join("daemon.json")
}

/// What `~/.trip/daemon.json` holds while a daemon runs. install.sh reads
/// both fields with `sed`, so each must stay a plain JSON string or number.
#[derive(Debug, PartialEq, Serialize, Deserialize)]
pub struct DaemonRecord {
    pub version: String,
    pub pid: u32,
}

/// The record as the file holds it, whether or not its daemon still runs.
pub fn read_daemon_record() -> Option<DaemonRecord> {
    let text = std::fs::read_to_string(daemon_record_path()).ok()?;
    serde_json::from_str(&text).ok()
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

#[cfg(test)]
mod tests {
    use super::DaemonRecord;

    #[test]
    fn the_daemon_record_is_one_line_of_plain_values() {
        // install.sh reads "version" and "pid" with sed, line by line.
        let record = DaemonRecord {
            version: "0.2.0".into(),
            pid: 42,
        };
        assert_eq!(
            serde_json::to_string(&record).unwrap(),
            r#"{"version":"0.2.0","pid":42}"#
        );
    }

    #[test]
    fn a_record_reads_back_however_it_is_spaced() {
        let record: DaemonRecord =
            serde_json::from_str("{\"version\": \"0.2.0\",\n \"pid\": 42}\n").unwrap();
        assert_eq!(
            record,
            DaemonRecord {
                version: "0.2.0".into(),
                pid: 42
            }
        );
        for broken in [
            r#"{"version": "0.2.0"}"#,
            r#"{"pid": 42}"#,
            r#"{"version": 2, "pid": 42}"#,
            r#"{"version": "0.2.0", "pid": -1}"#,
            "",
        ] {
            assert!(
                serde_json::from_str::<DaemonRecord>(broken).is_err(),
                "{}",
                broken
            );
        }
    }
}
