pub mod attach;
pub mod launch;
pub mod wrap;

use std::collections::HashMap;
use std::path::PathBuf;

use anyhow::Result;
use tokio::io::{BufReader, BufWriter};

use crate::daemon::protocol::{read_frame, write_control, Frame, Request, Response, SessionState};

pub fn terminal_env() -> HashMap<String, String> {
    std::env::vars().collect()
}

fn read_yn() -> bool {
    use nix::sys::termios::{self, LocalFlags, SetArg};
    use std::os::fd::{AsRawFd, BorrowedFd};

    let fd = std::io::stdin().as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let original = termios::tcgetattr(borrowed).ok();

    if let Some(ref orig) = original {
        let mut raw = orig.clone();
        raw.local_flags &= !(LocalFlags::ICANON | LocalFlags::ECHO);
        termios::tcsetattr(borrowed, SetArg::TCSANOW, &raw).ok();
    }

    let mut buf = [0u8; 1];
    use std::io::Read;
    std::io::stdin().read_exact(&mut buf).ok();

    if let Some(ref orig) = original {
        termios::tcsetattr(borrowed, SetArg::TCSANOW, orig).ok();
    }

    buf[0] == b'y' || buf[0] == b'Y'
}

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
        if rel.is_empty() {
            Ok(PathBuf::from(&home)
                .file_name()
                .map(|n| n.to_string_lossy().into_owned())
                .unwrap_or_else(|| "home".into()))
        } else {
            Ok(rel.to_string())
        }
    } else {
        Ok(base
            .file_name()
            .map(|n| n.to_string_lossy().into_owned())
            .unwrap_or_else(|| "session".into()))
    }
}

pub async fn get_session_list() -> Result<Vec<crate::daemon::protocol::SessionInfo>> {
    match launch::try_connect().await {
        Ok(stream) => {
            let (reader, writer) = stream.into_split();
            let mut reader = BufReader::new(reader);
            let mut writer = BufWriter::new(writer);
            write_control(&mut writer, &Request::ListSessions).await?;
            match read_frame(&mut reader).await? {
                Some(Frame::Control(payload)) => {
                    let response: Response = serde_json::from_slice(&payload)?;
                    match response {
                        Response::SessionList { sessions } => Ok(sessions),
                        _ => Ok(Vec::new()),
                    }
                }
                _ => Ok(Vec::new()),
            }
        }
        Err(_) => Ok(Vec::new()),
    }
}

fn is_numbered_session(name: &str) -> bool {
    name.rsplit_once('.')
        .map(|(_, suffix)| suffix.chars().all(|c| c.is_ascii_digit()))
        .unwrap_or(false)
}

pub fn next_available_name(
    sessions: &[crate::daemon::protocol::SessionInfo],
    base: &str,
) -> String {
    let mut n = 1;
    loop {
        let candidate = format!("{}.{}", base, n);
        if !sessions.iter().any(|s| s.name == candidate) {
            return candidate;
        }
        n += 1;
    }
}

pub async fn enter(name: Option<String>, command: Option<Vec<String>>) -> Result<()> {
    let name = match name {
        Some(n) => n,
        None => derive_session_name()?,
    };

    if let Ok(current) = std::env::var("TRIP_SESSION") {
        if current == name {
            println!("already in session '{}'", name);
            return Ok(());
        }
        // Set tab title before switching
        print!("\x1b]1;{}\x07", name);
        std::io::Write::flush(&mut std::io::stdout()).ok();

        let stream = launch::connect().await?;
        let (reader, writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut writer = BufWriter::new(writer);
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        write_control(
            &mut writer,
            &Request::SwitchSession {
                from: current,
                to: name,
                command,
                cwd,
                env: terminal_env(),
            },
        )
        .await?;
        match read_frame(&mut reader).await? {
            Some(Frame::Control(payload)) => {
                let response: Response = serde_json::from_slice(&payload)?;
                match response {
                    Response::Ok => return Ok(()),
                    Response::Error { message } => anyhow::bail!("{}", message),
                    _ => anyhow::bail!("unexpected response"),
                }
            }
            _ => anyhow::bail!("unexpected frame"),
        }
    }

    let sessions = get_session_list().await?;
    let session = sessions.iter().find(|s| s.name == name);

    match session.map(|s| s.attached) {
        None => {
            create_session(name.clone(), command).await?;
        }
        Some(true) => {
            eprint!("session '{}' is in use. take over? [y/n] ", name);
            if read_yn() {
                take_over(name.clone()).await?;
            } else {
                eprintln!();
            }
        }
        Some(false) => {}
    }

    attach::attach(name).await?;
    Ok(())
}

pub async fn new_session(name: Option<String>, command: Option<Vec<String>>) -> Result<()> {
    let base = match name {
        Some(n) => n,
        None => derive_session_name()?,
    };

    let sessions = get_session_list().await?;
    let name = next_available_name(&sessions, &base);

    create_session(name.clone(), command).await?;
    attach::attach(name).await?;
    Ok(())
}

pub async fn create_session(name: String, command: Option<Vec<String>>) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let cwd = std::env::current_dir()?.to_string_lossy().to_string();
    write_control(
        &mut writer,
        &Request::CreateSession {
            name,
            command,
            cwd,
            env: terminal_env(),
        },
    )
    .await?;

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

pub async fn list_sessions(all: bool) -> Result<()> {
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
                    let sessions: Vec<_> = if all {
                        sessions
                    } else {
                        sessions
                            .into_iter()
                            .filter(|s| !is_numbered_session(&s.name) || s.attached)
                            .collect()
                    };
                    if sessions.is_empty() {
                        println!("no sessions");
                        return Ok(());
                    }
                    let current = std::env::var("TRIP_SESSION").ok();
                    let home = std::env::var("HOME").unwrap_or_default();
                    let rows: Vec<_> = sessions
                        .iter()
                        .map(|s| {
                            let is_current = current.as_deref() == Some(&s.name);
                            let marker = if is_current {
                                "*"
                            } else if s.attached {
                                "+"
                            } else {
                                match s.state {
                                    SessionState::Exited(_) => "✕",
                                    _ => "-",
                                }
                            };
                            let cmd = s
                                .title
                                .as_deref()
                                .or(s.fg_command.as_deref())
                                .unwrap_or(&s.command);
                            let branch = s.git_branch.as_deref().unwrap_or("-");
                            let cwd = s.cwd.as_deref().unwrap_or("");
                            let cwd = if !home.is_empty() && cwd.starts_with(&home) {
                                format!("~{}", &cwd[home.len()..])
                            } else {
                                cwd.to_string()
                            };
                            (
                                marker.to_string(),
                                s.name.clone(),
                                cmd.to_string(),
                                branch.to_string(),
                                cwd,
                            )
                        })
                        .collect();

                    let nw = rows.iter().map(|r| r.1.len()).max().unwrap_or(0);
                    let cw = rows.iter().map(|r| r.2.len()).max().unwrap_or(0);
                    let bw = rows.iter().map(|r| r.3.len()).max().unwrap_or(0);

                    for (marker, name, cmd, branch, cwd) in &rows {
                        println!(
                            "{} {:<nw$}  {:<cw$}  {:<bw$}  {}",
                            marker,
                            name,
                            cmd,
                            branch,
                            cwd,
                            nw = nw,
                            cw = cw,
                            bw = bw,
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

pub const TERMINAL_RESET: &[u8] =
    b"\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[H\x1b[2J";

pub async fn get_screen(name: String, watch: bool) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    write_control(
        &mut writer,
        &Request::GetScreen {
            name: name.clone(),
            watch,
        },
    )
    .await?;

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

pub async fn get_log(
    name: String,
    raw: bool,
    verbose: bool,
    follow: bool,
    since: Option<f64>,
) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    write_control(
        &mut writer,
        &Request::GetLog {
            name: name.clone(),
            raw,
            verbose,
            follow,
            since,
        },
    )
    .await?;

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

    write_control(
        &mut writer,
        &Request::SendInput {
            name: name.clone(),
            data,
        },
    )
    .await?;

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

async fn take_over(name: String) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    write_control(
        &mut writer,
        &Request::TakeOver {
            name,
            env: terminal_env(),
        },
    )
    .await?;

    match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::Ok => {}
                Response::Error { message } => anyhow::bail!("{}", message),
                _ => anyhow::bail!("unexpected response"),
            }
        }
        _ => anyhow::bail!("unexpected frame"),
    }

    Ok(())
}

pub async fn return_session(name: String) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    write_control(&mut writer, &Request::ReturnSession { name }).await?;

    match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::Ok => {}
                Response::Error { message } => anyhow::bail!("{}", message),
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

pub async fn shutdown(yes: bool) -> Result<()> {
    if !yes {
        eprint!("this will kill all sessions. are you sure? [y/n] ");
        if !read_yn() {
            eprintln!();
            return Ok(());
        }
        eprintln!();
    }

    let stream = match launch::try_connect().await {
        Ok(s) => s,
        Err(_) => {
            println!("daemon not running");
            return Ok(());
        }
    };
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    write_control(&mut writer, &Request::Shutdown).await?;

    match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::Ok => println!("daemon stopped"),
                Response::Error { message } => anyhow::bail!("{}", message),
                _ => anyhow::bail!("unexpected response"),
            }
        }
        _ => println!("daemon stopped"),
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

pub fn agent_on() -> Result<()> {
    let session_name =
        std::env::var("TRIP_SESSION").map_err(|_| anyhow::anyhow!("not in a trip session"))?;

    let cwd = std::env::current_dir()?.to_string_lossy().to_string();

    let (kind, log_path) = if let Ok(session_id) = std::env::var("CLAUDE_CODE_SESSION_ID") {
        let encoded_cwd = cwd.replace('/', "-");
        let home = std::env::var("HOME").unwrap_or_default();
        let path = format!(
            "{}/.claude/projects/{}/{}.jsonl",
            home, encoded_cwd, session_id
        );
        ("claude".to_string(), path)
    } else if let Ok(thread_id) = std::env::var("CODEX_THREAD_ID") {
        let home = std::env::var("HOME").unwrap_or_default();
        let codex_dir = format!("{}/.codex/sessions", home);
        match find_codex_log(&codex_dir, &thread_id) {
            Some(path) => ("codex".to_string(), path),
            None => anyhow::bail!("could not find codex log for thread {}", thread_id),
        }
    } else {
        anyhow::bail!("no agent detected (need CLAUDE_CODE_SESSION_ID or CODEX_THREAD_ID)");
    };

    let config = crate::daemon::agent::AgentConfig {
        kind,
        log_path: log_path.clone(),
    };
    let config_path = crate::daemon::agent::agent_config_path(&session_name);
    let json = serde_json::to_string(&config)?;
    std::fs::write(&config_path, json)?;
    eprintln!("trip on: {}", log_path);
    Ok(())
}

fn find_codex_log(base_dir: &str, thread_id: &str) -> Option<String> {
    fn search_dir(dir: &std::path::Path, thread_id: &str) -> Option<String> {
        for entry in std::fs::read_dir(dir).ok()?.flatten() {
            let path = entry.path();
            if path.is_dir() {
                if let Some(found) = search_dir(&path, thread_id) {
                    return Some(found);
                }
            } else if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.contains(thread_id) && name.ends_with(".jsonl") {
                    return Some(path.to_string_lossy().to_string());
                }
            }
        }
        None
    }
    search_dir(std::path::Path::new(base_dir), thread_id)
}
