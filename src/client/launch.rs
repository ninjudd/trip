use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Result;
use tokio::net::UnixStream;

use crate::common::{daemon_log_path, socket_path, trip_dir};

pub async fn try_connect() -> Result<UnixStream> {
    let stream = UnixStream::connect(socket_path()).await?;
    Ok(stream)
}

pub async fn connect() -> Result<UnixStream> {
    match try_connect().await {
        Ok(stream) => Ok(stream),
        Err(_) => {
            start_daemon()?;

            for _ in 0..50 {
                tokio::time::sleep(Duration::from_millis(100)).await;
                if let Ok(stream) = try_connect().await {
                    return Ok(stream);
                }
            }

            anyhow::bail!("daemon failed to start")
        }
    }
}

fn start_daemon() -> Result<()> {
    let dir = trip_dir();
    std::fs::create_dir_all(&dir)?;

    // The daemon has no terminal, so this file is the only place its
    // panics and errors can surface. Discarding them means a dead daemon
    // (and every session with it) leaves no trace of why.
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(daemon_log_path())?;

    let exe = std::env::current_exe()?;
    Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()?;

    Ok(())
}
