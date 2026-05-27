use std::io::Write;
use std::path::Path;
use std::time::Duration;

use anyhow::Result;
use serde::{Deserialize, Serialize};
use tokio::io::{BufReader, BufWriter};
use tokio::time::{sleep, Instant};

use crate::daemon::protocol::{
    read_frame, write_control, write_frame, Frame, Request, Response, SessionState, FRAME_DATA,
    FRAME_RESIZE,
};
use crate::daemon::recording::RecordEvent;

#[derive(Deserialize)]
#[serde(tag = "type")]
enum WrapInput {
    #[serde(rename = "send")]
    Send { text: String },
    #[serde(rename = "key")]
    Key { key: String },
    #[serde(rename = "resize")]
    Resize { cols: u16, rows: u16 },
    #[serde(rename = "screenshot")]
    Screenshot,
    #[serde(rename = "close")]
    Close,
}

#[derive(Serialize)]
#[serde(tag = "type")]
enum WrapOutput {
    #[serde(rename = "log")]
    Log { text: String },
    #[serde(rename = "screen")]
    Screen { text: String },
    #[serde(rename = "exit")]
    Exit { code: i32 },
    #[serde(rename = "error")]
    Error { message: String },
}

fn emit(event: &WrapOutput) {
    if let Ok(line) = serde_json::to_string(event) {
        let mut stdout = std::io::stdout().lock();
        let _ = writeln!(stdout, "{}", line);
        let _ = stdout.flush();
    }
}

fn key_to_bytes(name: &str) -> Option<Vec<u8>> {
    match name {
        "enter" => Some(vec![b'\r']),
        "tab" => Some(vec![b'\t']),
        "escape" | "esc" => Some(vec![0x1b]),
        "backspace" => Some(vec![0x7f]),
        "delete" => Some(b"\x1b[3~".to_vec()),
        "up" => Some(b"\x1b[A".to_vec()),
        "down" => Some(b"\x1b[B".to_vec()),
        "right" => Some(b"\x1b[C".to_vec()),
        "left" => Some(b"\x1b[D".to_vec()),
        "home" => Some(b"\x1b[H".to_vec()),
        "end" => Some(b"\x1b[F".to_vec()),
        "page-up" | "pageup" => Some(b"\x1b[5~".to_vec()),
        "page-down" | "pagedown" => Some(b"\x1b[6~".to_vec()),
        "space" => Some(vec![b' ']),
        s if s.starts_with("ctrl-") => {
            let ch = s.strip_prefix("ctrl-")?;
            let c = ch.chars().next()?;
            if ch.len() == 1 && c.is_ascii_lowercase() {
                Some(vec![(c as u8) - b'a' + 1])
            } else {
                None
            }
        }
        _ => None,
    }
}

fn check_log(log_path: &Path, last_size: &mut u64, in_agent: &mut bool) {
    let current_size = std::fs::metadata(log_path).map(|m| m.len()).unwrap_or(0);
    if current_size <= *last_size {
        return;
    }
    if let Ok(mut file) = std::fs::File::open(log_path) {
        use std::io::{Read, Seek, SeekFrom};
        if file.seek(SeekFrom::Start(*last_size)).is_ok() {
            let mut new_bytes = String::new();
            if file.read_to_string(&mut new_bytes).is_ok() {
                for line in new_bytes.lines() {
                    if let Ok(event) = serde_json::from_str::<RecordEvent>(line) {
                        match &event {
                            RecordEvent::AgentSessionStart { .. } => {
                                *in_agent = true;
                                emit_raw(line);
                            }
                            RecordEvent::AgentSessionEnd { .. } => {
                                *in_agent = false;
                                emit_raw(line);
                            }
                            _ if event.is_agent_event() => {
                                emit_raw(line);
                            }
                            RecordEvent::Screen { text, .. } if !*in_agent => {
                                if !text.is_empty() {
                                    emit(&WrapOutput::Log { text: text.clone() });
                                }
                            }
                            _ => {}
                        }
                    }
                }
            }
        }
    }
    *last_size = current_size;
}

fn emit_raw(json_line: &str) {
    let mut stdout = std::io::stdout().lock();
    let _ = writeln!(stdout, "{}", json_line);
    let _ = stdout.flush();
}

async fn get_screen_text(name: &str) -> Result<String> {
    let stream = super::launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    write_control(
        &mut writer,
        &Request::GetScreen {
            name: name.to_string(),
            watch: false,
        },
    )
    .await?;
    match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::ScreenData { content } => Ok(content),
                Response::Error { message } => Err(anyhow::anyhow!("{}", message)),
                _ => Err(anyhow::anyhow!("unexpected response")),
            }
        }
        _ => Err(anyhow::anyhow!("unexpected frame")),
    }
}

async fn get_exit_code(name: &str) -> i32 {
    let Ok(stream) = super::launch::connect().await else {
        return -1;
    };
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    if write_control(&mut writer, &Request::ListSessions)
        .await
        .is_err()
    {
        return -1;
    }
    match read_frame(&mut reader).await {
        Ok(Some(Frame::Control(payload))) => {
            let Ok(Response::SessionList { sessions }) = serde_json::from_slice(&payload) else {
                return -1;
            };
            for s in sessions {
                if s.name == name {
                    if let SessionState::Exited(code) = s.state {
                        return code;
                    }
                }
            }
            -1
        }
        _ => -1,
    }
}

pub async fn wrap(base_name: String, command: Option<Vec<String>>) -> Result<()> {
    let sessions = super::get_session_list().await?;
    let existing = sessions.iter().any(|s| s.name == base_name);

    let name = if existing && command.is_none() {
        base_name.clone()
    } else {
        let name = super::next_available_name(&sessions, &base_name);

        let stream = super::launch::connect().await?;
        let (reader, writer) = stream.into_split();
        let mut reader = BufReader::new(reader);
        let mut writer = BufWriter::new(writer);

        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        write_control(
            &mut writer,
            &Request::CreateSession {
                name: name.clone(),
                command,
                cwd,
                env: super::terminal_env(),
            },
        )
        .await?;

        match read_frame(&mut reader).await? {
            Some(Frame::Control(payload)) => {
                let response: Response = serde_json::from_slice(&payload)?;
                match response {
                    Response::SessionCreated { .. } => {}
                    Response::Error { message } => anyhow::bail!("{}", message),
                    _ => anyhow::bail!("unexpected response"),
                }
            }
            _ => anyhow::bail!("unexpected frame"),
        }

        name
    };
    eprintln!("session: {}", name);

    // Attach to session (retry briefly in case of race with session startup)
    let (mut reader, mut writer) = {
        let mut last_err = String::from("failed to attach");
        let mut result = None;
        for _ in 0..5 {
            let stream = super::launch::connect().await?;
            let (r, w) = stream.into_split();
            let mut r = BufReader::new(r);
            let mut w = BufWriter::new(w);

            write_control(
                &mut w,
                &Request::Attach {
                    name: name.clone(),
                    cols: 80,
                    rows: 24,
                    env: super::terminal_env(),
                },
            )
            .await?;

            match read_frame(&mut r).await? {
                Some(Frame::Control(payload)) => {
                    let response: Response = serde_json::from_slice(&payload)?;
                    match response {
                        Response::Attached { .. } => {
                            result = Some((r, w));
                            break;
                        }
                        Response::Error { message } => {
                            last_err = message;
                        }
                        _ => anyhow::bail!("unexpected response"),
                    }
                }
                _ => anyhow::bail!("unexpected frame"),
            }
            tokio::time::sleep(Duration::from_millis(200)).await;
        }
        result.ok_or_else(|| anyhow::anyhow!("{}", last_err))?
    };

    // Track log file
    let log_path = crate::common::log_path(&name);
    let mut last_log_size = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
    let mut in_agent = false;

    // Read JSONL lines from stdin in a blocking thread
    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<String>(32);
    std::thread::spawn(move || {
        use std::io::BufRead;
        let stdin = std::io::stdin();
        for line in stdin.lock().lines() {
            match line {
                Ok(l) if !l.is_empty() => {
                    if stdin_tx.blocking_send(l).is_err() {
                        break;
                    }
                }
                Ok(_) => {}
                Err(_) => break,
            }
        }
    });

    let idle_duration = Duration::from_millis(600);
    let far_future = Duration::from_secs(86400);
    let mut idle_deadline = Box::pin(sleep(far_future));
    let mut log_pending = false;

    loop {
        tokio::select! {
            frame = read_frame(&mut reader) => {
                match frame? {
                    Some(Frame::Data(_)) => {
                        idle_deadline.as_mut().reset(Instant::now() + idle_duration);
                        log_pending = true;
                    }
                    _ => break,
                }
            }

            _ = &mut idle_deadline, if log_pending => {
                check_log(&log_path, &mut last_log_size, &mut in_agent);
                log_pending = false;
                idle_deadline.as_mut().reset(Instant::now() + far_future);
            }

            line = stdin_rx.recv() => {
                match line {
                    Some(line) => {
                        match serde_json::from_str::<WrapInput>(&line) {
                            Ok(WrapInput::Send { text }) => {
                                let trimmed = text.trim_end_matches('\n');
                                let data = if trimmed.contains('\n') {
                                    let mut bytes = Vec::new();
                                    bytes.extend_from_slice(b"\x1b[200~");
                                    bytes.extend_from_slice(trimmed.replace('\n', "\r").as_bytes());
                                    bytes.extend_from_slice(b"\x1b[201~");
                                    if text.ends_with('\n') {
                                        bytes.push(b'\r');
                                    }
                                    bytes
                                } else {
                                    text.replace('\n', "\r").into_bytes()
                                };
                                write_frame(&mut writer, FRAME_DATA, &data).await?;
                            }
                            Ok(WrapInput::Key { key }) => {
                                if let Some(bytes) = key_to_bytes(&key) {
                                    write_frame(&mut writer, FRAME_DATA, &bytes).await?;
                                } else {
                                    emit(&WrapOutput::Error {
                                        message: format!("unknown key: {}", key),
                                    });
                                }
                            }
                            Ok(WrapInput::Resize { cols, rows }) => {
                                let mut payload = Vec::with_capacity(4);
                                payload.extend_from_slice(&cols.to_be_bytes());
                                payload.extend_from_slice(&rows.to_be_bytes());
                                write_frame(&mut writer, FRAME_RESIZE, &payload).await?;
                            }
                            Ok(WrapInput::Screenshot) => {
                                // Flush pending log first
                                if log_pending {
                                    check_log(&log_path, &mut last_log_size, &mut in_agent);
                                    log_pending = false;
                                }
                                match get_screen_text(&name).await {
                                    Ok(text) => emit(&WrapOutput::Screen { text }),
                                    Err(e) => emit(&WrapOutput::Error {
                                        message: e.to_string(),
                                    }),
                                }
                            }
                            Ok(WrapInput::Close) => {
                                break;
                            }
                            Err(e) => {
                                emit(&WrapOutput::Error {
                                    message: format!("invalid input: {}", e),
                                });
                            }
                        }
                    }
                    None => break,
                }
            }
        }
    }

    // Final log flush
    tokio::time::sleep(Duration::from_millis(600)).await;
    check_log(&log_path, &mut last_log_size, &mut in_agent);

    // Wait for reaper to update exit code
    let mut code = -1;
    for _ in 0..10 {
        code = get_exit_code(&name).await;
        if code != -1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    emit(&WrapOutput::Exit { code });

    Ok(())
}
