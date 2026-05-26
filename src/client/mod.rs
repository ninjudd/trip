pub mod attach;
pub mod launch;

use std::path::PathBuf;

use anyhow::Result;
use tokio::io::{BufReader, BufWriter};

use crate::daemon::protocol::{
    read_frame, write_control, Frame, Request, Response, SessionState,
};

pub fn derive_session_name() -> Result<String> {
    let cwd = std::env::current_dir()?;
    let home = std::env::var("HOME").unwrap_or_default();

    // Try git root first
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .output();

    let base = match output {
        Ok(out) if out.status.success() => {
            PathBuf::from(String::from_utf8_lossy(&out.stdout).trim().to_string())
        }
        _ => cwd,
    };

    let path = base.to_string_lossy();
    if !home.is_empty() && path.starts_with(&home) {
        let rel = &path[home.len()..];
        let rel = rel.strip_prefix('/').unwrap_or(rel);
        Ok(rel.to_string())
    } else {
        Ok(base
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".into()))
    }
}

fn find_session<'a>(
    sessions: &'a [crate::daemon::protocol::SessionInfo],
    name: &str,
) -> Option<&'a crate::daemon::protocol::SessionInfo> {
    sessions.iter().find(|s| s.name == name)
}

pub async fn enter(name: Option<String>, command: Option<Vec<String>>) -> Result<()> {
    let name = match name {
        Some(n) => n,
        None => derive_session_name()?,
    };

    // Check if we're already in this session
    if let Ok(current) = std::env::var("DRIP_SESSION") {
        if current == name {
            println!("already in session '{}'", name);
            return Ok(());
        } else {
            anyhow::bail!(
                "already in session '{}' (use drip enter --force to switch — not yet implemented)",
                current
            );
        }
    }

    // Check if session exists and whether someone is attached
    let session_info = match launch::try_connect().await {
        Ok(stream) => {
            let (reader, writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut writer = BufWriter::new(writer);
            write_control(&mut writer, &Request::ListSessions).await?;
            match read_frame(&mut reader).await? {
                Some(Frame::Control(payload)) => {
                    let response: Response = serde_json::from_slice(&payload)?;
                    match response {
                        Response::SessionList { sessions } => {
                            find_session(&sessions, &name).map(|s| (true, s.attached))
                        }
                        _ => None,
                    }
                }
                _ => None,
            }
        }
        Err(_) => None,
    };

    match session_info {
        None => {
            // Session doesn't exist — create it
            create_session(name.clone(), command).await?;
        }
        Some((_, true)) => {
            // Session exists and someone is attached — ask what to do
            eprint!("session '{}' has an active writer. steal it? [y/n] ", name);
            use std::io::Read;
            let mut buf = [0u8; 1];
            std::io::stdin().read_exact(&mut buf).ok();
            if buf[0] == b'y' || buf[0] == b'Y' {
                detach_session(name.clone()).await?;
            }
        }
        Some((_, false)) => {
            // Session exists, no one attached — just attach
        }
    }

    attach::attach(name).await?;
    Ok(())
}

pub async fn create_session(name: String, command: Option<Vec<String>>) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let cwd = std::env::current_dir()?
        .to_string_lossy()
        .to_string();
    write_control(&mut writer, &Request::CreateSession { name, command, cwd }).await?;

    match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::SessionCreated { name, pid } => {
                    println!("session '{}' created (pid {})", name, pid);
                }
                Response::Error { message } => {
                    anyhow::bail!("{}", message);
                }
                _ => anyhow::bail!("unexpected response"),
            }
        }
        _ => anyhow::bail!("unexpected frame"),
    }

    Ok(())
}

pub async fn list_sessions() -> Result<()> {
    let stream = match launch::try_connect().await {
        Ok(s) => s,
        Err(_) => {
            // No daemon running means no sessions
            return Ok(());
        }
    };

    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    write_control(&mut writer, &Request::ListSessions).await?;

    match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::SessionList { sessions } => {
                    if sessions.is_empty() {
                        println!("no sessions");
                        return Ok(());
                    }
                    let current = std::env::var("DRIP_SESSION").ok();
                    for s in sessions {
                        let marker = if current.as_deref() == Some(&s.name) { "* " } else { "  " };
                        let state = match s.state {
                            SessionState::Running => {
                                if s.attached {
                                    "attached"
                                } else {
                                    "detached"
                                }
                            }
                            SessionState::Exited(code) => {
                                if code == 0 {
                                    "exited"
                                } else {
                                    "failed"
                                }
                            }
                        };

                        let cmd = s.fg_command.as_deref().unwrap_or(&s.command);
                        let branch = s.git_branch.as_deref().unwrap_or("-");
                        let home = std::env::var("HOME").unwrap_or_default();
                        let cwd = s.cwd.as_deref().unwrap_or("");
                        let cwd = if !home.is_empty() && cwd.starts_with(&home) {
                            format!("~{}", &cwd[home.len()..])
                        } else {
                            cwd.to_string()
                        };

                        println!(
                            "{}{:<12} {:<10} {:<16} {:<16} {}",
                            marker, s.name, state, cmd, branch, &cwd
                        );
                    }
                }
                Response::Error { message } => {
                    anyhow::bail!("{}", message);
                }
                _ => anyhow::bail!("unexpected response"),
            }
        }
        _ => anyhow::bail!("unexpected frame"),
    }

    Ok(())
}

pub async fn list_screens(name: String, index: Option<usize>) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    match index {
        None => {
            write_control(&mut writer, &Request::ListScreens { name }).await?;
            match read_frame(&mut reader).await? {
                Some(Frame::Control(payload)) => {
                    let response: Response = serde_json::from_slice(&payload)?;
                    match response {
                        Response::ScreenList { screens } => {
                            if screens.is_empty() {
                                println!("no screens captured yet");
                                return Ok(());
                            }
                            for s in screens {
                                println!("{:<6} {:<12} {} lines", s.index, s.timestamp, s.lines);
                            }
                        }
                        Response::Error { message } => anyhow::bail!("{}", message),
                        _ => anyhow::bail!("unexpected response"),
                    }
                }
                _ => anyhow::bail!("unexpected frame"),
            }
        }
        Some(idx) => {
            write_control(&mut writer, &Request::GetScreenAt { name, index: idx }).await?;
            match read_frame(&mut reader).await? {
                Some(Frame::Control(payload)) => {
                    let response: Response = serde_json::from_slice(&payload)?;
                    match response {
                        Response::ScreenData { content } => {
                            println!("{}", content);
                        }
                        Response::Error { message } => anyhow::bail!("{}", message),
                        _ => anyhow::bail!("unexpected response"),
                    }
                }
                _ => anyhow::bail!("unexpected frame"),
            }
        }
    }

    Ok(())
}

pub const TERMINAL_RESET: &[u8] = b"\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l";

pub async fn get_screen(name: String, watch: bool) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    write_control(&mut writer, &Request::GetScreen { name: name.clone(), watch }).await?;

    std::io::Write::write_all(&mut std::io::stdout(), TERMINAL_RESET).ok();

    let mut first = true;
    loop {
        match read_frame(&mut reader).await? {
            Some(Frame::Control(payload)) => {
                let response: Response = serde_json::from_slice(&payload)?;
                match response {
                    Response::ScreenData { content } => {
                        if !first {
                            print!("\n--- screen updated ---\n\n");
                        }
                        print!("{}", content);
                        std::io::Write::flush(&mut std::io::stdout())?;
                        first = false;
                        if !watch {
                            return Ok(());
                        }
                    }
                    Response::Error { message } => {
                        anyhow::bail!("{}", message);
                    }
                    _ => anyhow::bail!("unexpected response"),
                }
            }
            None => return Ok(()),
            _ => anyhow::bail!("unexpected frame"),
        }
    }
}

pub async fn get_log(name: String, raw: bool, follow: bool, since: Option<f64>) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    write_control(&mut writer, &Request::GetLog { name: name.clone(), raw, follow, since }).await?;

    loop {
        match read_frame(&mut reader).await? {
            Some(Frame::Control(payload)) => {
                let response: Response = serde_json::from_slice(&payload)?;
                match response {
                    Response::LogData { content } => {
                        print!("{}", content);
                        std::io::Write::flush(&mut std::io::stdout())?;
                        if !follow {
                            return Ok(());
                        }
                    }
                    Response::Error { message } => {
                        anyhow::bail!("{}", message);
                    }
                    _ => anyhow::bail!("unexpected response"),
                }
            }
            None => return Ok(()),
            _ => anyhow::bail!("unexpected frame"),
        }
    }
}

pub async fn send_input(name: String, input: String, raw: bool) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let mut data = input.into_bytes();
    if !raw {
        data.push(b'\r');
    }

    write_control(&mut writer, &Request::SendInput { name: name.clone(), data }).await?;

    match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::Ok => {}
                Response::Error { message } => {
                    anyhow::bail!("{}", message);
                }
                _ => anyhow::bail!("unexpected response"),
            }
        }
        _ => anyhow::bail!("unexpected frame"),
    }

    Ok(())
}

pub async fn detach_session(name: String) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    println!("detaching '{}'", name);

    write_control(&mut writer, &Request::DetachSession { name: name.clone() }).await?;

    match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::Ok => {}
                Response::Error { message } => {
                    anyhow::bail!("{}", message);
                }
                _ => anyhow::bail!("unexpected response"),
            }
        }
        _ => anyhow::bail!("unexpected frame"),
    }

    Ok(())
}

pub async fn kill_session(name: String) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    write_control(&mut writer, &Request::KillSession { name: name.clone() }).await?;

    match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::Ok => {
                    println!("session '{}' killed", name);
                }
                Response::Error { message } => {
                    anyhow::bail!("{}", message);
                }
                _ => anyhow::bail!("unexpected response"),
            }
        }
        _ => anyhow::bail!("unexpected frame"),
    }

    Ok(())
}
