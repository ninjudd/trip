use std::process::{Command, Stdio};
use std::time::Duration;

use anyhow::Result;
use tokio::net::UnixStream;

use crate::common::{drip_dir, socket_path};

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
    let dir = drip_dir();
    std::fs::create_dir_all(&dir)?;

    let exe = std::env::current_exe()?;
    Command::new(exe)
        .arg("daemon")
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    Ok(())
}
