pub mod diff;
pub mod procinfo;
pub mod protocol;
pub mod recording;
pub mod session;

use std::collections::HashMap;
use std::os::fd::AsRawFd;
use std::sync::Arc;

use anyhow::Result;
use nix::libc;
use nix::sys::wait::{waitpid, WaitPidFlag, WaitStatus};
use tokio::io::{BufReader, BufWriter};
use tokio::net::{UnixListener, UnixStream};
use tokio::sync::Mutex;

use crate::common::{drip_dir, lock_path, socket_path};
use protocol::{
    read_frame, write_control, write_frame, Frame, Request, Response, ScreenEntry, SessionInfo,
    SessionState, FRAME_DATA,
};
use recording::RecordEvent;
use session::{Session, SessionCommand};

fn format_events(events: &[RecordEvent], raw: bool) -> String {
    if raw {
        events
            .iter()
            .map(|e| serde_json::to_string(e).unwrap_or_default())
            .collect::<Vec<_>>()
            .join("\n")
            + if events.is_empty() { "" } else { "\n" }
    } else {
        let mut out = String::new();
        for event in events {
            if let RecordEvent::Screen { t: _, text } = event {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
                out.push('\n');
            }
        }
        out
    }
}

fn format_timestamp(t: f64) -> String {
    use std::time::{Duration, SystemTime, UNIX_EPOCH};
    let time = UNIX_EPOCH + Duration::from_secs_f64(t);
    let elapsed = time
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    // Use libc localtime for proper timezone
    let mut tm: nix::libc::tm = unsafe { std::mem::zeroed() };
    let time_t = elapsed as nix::libc::time_t;
    unsafe { nix::libc::localtime_r(&time_t, &mut tm) };
    format!("{:02}:{:02}:{:02}", tm.tm_hour, tm.tm_min, tm.tm_sec)
}

type Sessions = Arc<Mutex<HashMap<String, Session>>>;

pub async fn run() -> Result<()> {
    // Detach from controlling terminal so closing a tab doesn't kill us
    nix::unistd::setsid().ok();

    let dir = drip_dir();
    std::fs::create_dir_all(&dir)?;

    let lock_file = std::fs::File::create(lock_path())?;
    let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        anyhow::bail!("daemon already running");
    }

    let sock_path = socket_path();
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    let sessions_reaper = sessions.clone();
    tokio::spawn(async move {
        reap_children(sessions_reaper).await;
    });

    loop {
        let (stream, _) = listener.accept().await?;
        let sessions = sessions.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, sessions).await {
                eprintln!("client error: {}", e);
            }
        });
    }
}

async fn reap_children(sessions: Sessions) {
    use tokio::signal::unix::{signal, SignalKind};
    let mut sigchld = signal(SignalKind::child()).unwrap();

    loop {
        sigchld.recv().await;

        let mut sessions = sessions.lock().await;
        for session in sessions.values_mut() {
            if matches!(session.state, SessionState::Running) {
                match waitpid(session.pid, Some(WaitPidFlag::WNOHANG)) {
                    Ok(WaitStatus::Exited(_, code)) => {
                        session.state = SessionState::Exited(code);
                        session.detach_notify.notify_waiters();
                    }
                    Ok(WaitStatus::Signaled(_, _, _)) => {
                        session.state = SessionState::Exited(-1);
                        session.detach_notify.notify_waiters();
                    }
                    _ => {}
                }
            }
        }

        // Remove exited sessions with no clients
        sessions.retain(|_, s| {
            !(matches!(s.state, SessionState::Exited(_)) && s.client_count == 0)
        });

        if sessions.is_empty() {
            let _ = std::fs::remove_file(socket_path());
            std::process::exit(0);
        }
    }
}

async fn handle_client(stream: UnixStream, sessions: Sessions) -> Result<()> {
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let frame = read_frame(&mut reader).await?;
    let request: Request = match frame {
        Some(Frame::Control(payload)) => serde_json::from_slice(&payload)?,
        _ => {
            write_control(
                &mut writer,
                &Response::Error {
                    message: "expected control frame".into(),
                },
            )
            .await?;
            return Ok(());
        }
    };

    match request {
        Request::CreateSession { name, command, cwd } => {
            let mut sessions = sessions.lock().await;
            if sessions.contains_key(&name) {
                write_control(
                    &mut writer,
                    &Response::Error {
                        message: format!("session '{}' already exists", name),
                    },
                )
                .await?;
                return Ok(());
            }

            let session = Session::spawn(name.clone(), command, cwd, 80, 24)?;
            let pid = session.pid.as_raw() as u32;
            sessions.insert(name.clone(), session);

            write_control(&mut writer, &Response::SessionCreated { name, pid }).await?;
        }

        Request::ListSessions => {
            let sessions = sessions.lock().await;
            let list: Vec<SessionInfo> = sessions
                .values()
                .map(|s| {
                    let fg_pid = procinfo::get_foreground_pid(s.master_fd);
                    let cwd = fg_pid.and_then(procinfo::get_cwd);
                    let fg_command = fg_pid.and_then(procinfo::get_name);
                    let git_branch = cwd.as_ref().and_then(procinfo::get_git_branch);

                    SessionInfo {
                        name: s.name.clone(),
                        command: s.command.clone(),
                        pid: s.pid.as_raw() as u32,
                        created_at: s.created_at,
                        state: s.state.clone(),
                        attached: s.client_count > 0,
                        cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
                        fg_command,
                        git_branch,
                    }
                })
                .collect();
            write_control(&mut writer, &Response::SessionList { sessions: list }).await?;
        }

        Request::ListScreens { name } => {
            let dir = crate::common::screens_dir(&name);
            if !dir.exists() {
                write_control(&mut writer, &Response::Error {
                    message: format!("session '{}' not found", name),
                }).await?;
            } else {
                let mut entries: Vec<_> = std::fs::read_dir(&dir)
                    .into_iter()
                    .flatten()
                    .filter_map(|e| e.ok())
                    .filter(|e| e.path().extension().map(|x| x == "txt").unwrap_or(false))
                    .collect();
                entries.sort_by_key(|e| e.file_name());
                let screens: Vec<ScreenEntry> = entries
                    .iter()
                    .enumerate()
                    .map(|(i, e)| {
                        let content = std::fs::read_to_string(e.path()).unwrap_or_default();
                        let ts = e.metadata()
                            .and_then(|m| m.modified())
                            .map(|t| t.duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs_f64())
                            .unwrap_or(0.0);
                        ScreenEntry {
                            index: i,
                            timestamp: format_timestamp(ts),
                            lines: content.lines().count(),
                        }
                    })
                    .collect();
                write_control(&mut writer, &Response::ScreenList { screens }).await?;
            }
        }

        Request::GetScreenAt { name, index } => {
            let dir = crate::common::screens_dir(&name);
            let path = dir.join(format!("{:04}.txt", index));
            match std::fs::read_to_string(&path) {
                Ok(content) => {
                    write_control(&mut writer, &Response::ScreenData { content }).await?;
                }
                Err(_) => {
                    write_control(&mut writer, &Response::Error {
                        message: format!("screen {} not found", index),
                    }).await?;
                }
            }
        }

        Request::GetScreen { name, watch } => {
            if !watch {
                let sessions = sessions.lock().await;
                match sessions.get(&name) {
                    Some(s) => {
                        write_control(&mut writer, &Response::ScreenData { content: s.screen_text() }).await?;
                    }
                    None => {
                        write_control(&mut writer, &Response::Error {
                            message: format!("session '{}' not found", name),
                        }).await?;
                    }
                }
            } else {
                let mut output_rx = {
                    let sessions = sessions.lock().await;
                    match sessions.get(&name) {
                        Some(s) => {
                            write_control(&mut writer, &Response::ScreenData { content: s.screen_text() }).await?;
                            s.output_tx.subscribe()
                        }
                        None => {
                            write_control(&mut writer, &Response::Error {
                                message: format!("session '{}' not found", name),
                            }).await?;
                            return Ok(());
                        }
                    }
                };

                let mut last_screen = String::new();
                loop {
                    match output_rx.recv().await {
                        Ok(_) => {
                            // Wait for output to settle
                            loop {
                                match tokio::time::timeout(
                                    std::time::Duration::from_millis(500),
                                    output_rx.recv(),
                                ).await {
                                    Ok(Ok(_)) => continue,
                                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => continue,
                                    _ => break,
                                }
                            }
                            let current = {
                                let sessions = sessions.lock().await;
                                match sessions.get(&name) {
                                    Some(s) => s.screen_text(),
                                    None => break,
                                }
                            };
                            if current != last_screen {
                                write_control(&mut writer, &Response::ScreenData { content: current.clone() }).await?;
                                last_screen = current;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }

        Request::GetLog { name, raw, follow, since } => {
            let log_path = crate::common::log_path(&name);

            // Read existing log from disk
            let events = if log_path.exists() {
                let content = std::fs::read_to_string(&log_path).unwrap_or_default();
                content.lines()
                    .filter_map(|line| serde_json::from_str::<RecordEvent>(line).ok())
                    .filter(|e| since.map_or(true, |ts| e.timestamp() >= ts))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            let content = format_events(&events, raw);
            if !content.is_empty() {
                write_control(&mut writer, &Response::LogData { content }).await?;
            }

            if follow {
                // Follow by watching log file for new lines
                let output_rx = {
                    let sessions = sessions.lock().await;
                    match sessions.get(&name) {
                        Some(s) => Some(s.output_tx.subscribe()),
                        None => None,
                    }
                };
                if let Some(mut rx) = output_rx {
                    let mut last_size = std::fs::metadata(&log_path)
                        .map(|m| m.len())
                        .unwrap_or(0);
                    loop {
                        match rx.recv().await {
                            Ok(_) => {
                                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                                let current_size = std::fs::metadata(&log_path)
                                    .map(|m| m.len())
                                    .unwrap_or(0);
                                if current_size > last_size {
                                    let content = std::fs::read_to_string(&log_path).unwrap_or_default();
                                    let new_lines: String = content
                                        .bytes()
                                        .skip(last_size as usize)
                                        .map(|b| b as char)
                                        .collect();
                                    let new_events: Vec<RecordEvent> = new_lines
                                        .lines()
                                        .filter_map(|line| serde_json::from_str(line).ok())
                                        .collect();
                                    let formatted = format_events(&new_events, raw);
                                    if !formatted.is_empty() {
                                        write_control(&mut writer, &Response::LogData { content: formatted }).await?;
                                    }
                                    last_size = current_size;
                                }
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                }
            }
        }

        Request::SendInput { name, data } => {
            let sessions = sessions.lock().await;
            if let Some(session) = sessions.get(&name) {
                let _ = session.input_tx.send(SessionCommand::Input(data)).await;
                write_control(&mut writer, &Response::Ok).await?;
            } else {
                write_control(
                    &mut writer,
                    &Response::Error {
                        message: format!("session '{}' not found", name),
                    },
                )
                .await?;
            }
        }

        Request::SwitchSession { from, to, command, cwd } => {
            let mut sessions = sessions.lock().await;

            if !sessions.contains_key(&to) {
                match Session::spawn(to.clone(), command, cwd, 80, 24) {
                    Ok(session) => {
                        sessions.insert(to.clone(), session);
                    }
                    Err(e) => {
                        write_control(&mut writer, &Response::Error {
                            message: format!("failed to create session: {}", e),
                        }).await?;
                        return Ok(());
                    }
                }
            }

            // Signal the attach client on `from` to switch
            if let Some(session) = sessions.get(&from) {
                *session.switch_target.lock().unwrap() = Some(to.clone());
                session.switch_notify.notify_waiters();
                write_control(&mut writer, &Response::Ok).await?;
            } else {
                write_control(&mut writer, &Response::Error {
                    message: format!("session '{}' not found", from),
                }).await?;
            }
        }

        Request::TakeOver { name } => {
            let mut sessions = sessions.lock().await;
            if let Some(session) = sessions.get_mut(&name) {
                if let Some(flag) = session.writer_readonly_flag.take() {
                    flag.store(true, std::sync::atomic::Ordering::Relaxed);
                    session.writer_stack.push(flag);
                }
                session.writer_attached = false;
                write_control(&mut writer, &Response::Ok).await?;
            } else {
                write_control(
                    &mut writer,
                    &Response::Error {
                        message: format!("session '{}' not found", name),
                    },
                )
                .await?;
            }
        }

        Request::DetachSession { name } => {
            let sessions = sessions.lock().await;
            if let Some(session) = sessions.get(&name) {
                session.detach_notify.notify_waiters();
                write_control(&mut writer, &Response::Ok).await?;
            } else {
                write_control(
                    &mut writer,
                    &Response::Error {
                        message: format!("session '{}' not found", name),
                    },
                )
                .await?;
            }
        }

        Request::KillSession { name } => {
            let mut sessions = sessions.lock().await;
            if let Some(session) = sessions.get(&name) {
                let pid = session.pid;
                nix::sys::signal::kill(pid, nix::sys::signal::Signal::SIGHUP).ok();
                sessions.remove(&name);

                if sessions.is_empty() {
                    drop(sessions);
                    let _ = std::fs::remove_file(socket_path());
                    write_control(&mut writer, &Response::Ok).await?;
                    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
                    std::process::exit(0);
                }

                write_control(&mut writer, &Response::Ok).await?;
            } else {
                write_control(
                    &mut writer,
                    &Response::Error {
                        message: format!("session '{}' not found", name),
                    },
                )
                .await?;
            }
        }

        Request::Shutdown => {
            let mut sessions = sessions.lock().await;
            for session in sessions.values() {
                nix::sys::signal::kill(session.pid, nix::sys::signal::Signal::SIGHUP).ok();
            }
            sessions.clear();
            drop(sessions);
            let _ = std::fs::remove_file(socket_path());
            write_control(&mut writer, &Response::Ok).await?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            std::process::exit(0);
        }

        Request::Attach { name, cols, rows } => {
            let mut current_name = name;
            let mut current_cols = cols;
            let mut current_rows = rows;
            let mut first = true;

            loop {
                let (screen_data, mut output_rx, input_tx, detach_notify, switch_notify, switch_target, readonly, readonly_flag) = {
                    let mut sessions = sessions.lock().await;
                    let session = match sessions.get_mut(&current_name) {
                        Some(s) => s,
                        None => {
                            write_control(
                                &mut writer,
                                &Response::Error {
                                    message: format!("session '{}' not found", current_name),
                                },
                            )
                            .await?;
                            return Ok(());
                        }
                    };

                    let readonly = session.writer_attached;
                    let readonly_flag = Arc::new(std::sync::atomic::AtomicBool::new(readonly));
                    if !readonly {
                        session.writer_attached = true;
                        session.writer_readonly_flag = Some(readonly_flag.clone());
                        let _ = session
                            .input_tx
                            .send(SessionCommand::Resize(current_cols, current_rows))
                            .await;
                    }
                    session.client_count += 1;
                    let screen = session.screen_contents();
                    let rx = session.output_tx.subscribe();
                    let tx = session.input_tx.clone();
                    let detach = session.detach_notify.clone();
                    let sw_notify = session.switch_notify.clone();
                    let sw_target = session.switch_target.clone();
                    (screen, rx, tx, detach, sw_notify, sw_target, readonly, readonly_flag)
                };

                if first {
                    write_control(&mut writer, &Response::Attached { readonly }).await?;
                    first = false;
                }
                let screen_data = if readonly { strip_sgr(&screen_data) } else { screen_data };
                write_frame(&mut writer, FRAME_DATA, &screen_data).await?;

                let (result, r, w) = stream_session(
                    reader, writer, &mut output_rx, &input_tx,
                    &detach_notify, &switch_notify, &switch_target, &readonly_flag,
                    sessions.clone(), current_name.clone(), current_cols, current_rows,
                ).await?;
                reader = r;
                writer = w;

                // Clean up old session
                {
                    let mut sessions = sessions.lock().await;
                    if let Some(session) = sessions.get_mut(&current_name) {
                        session.client_count = session.client_count.saturating_sub(1);
                        let was_writer = !readonly_flag.load(std::sync::atomic::Ordering::Relaxed);
                        if was_writer {
                            session.writer_readonly_flag = None;
                            if let Some(prev_flag) = session.writer_stack.pop() {
                                prev_flag.store(false, std::sync::atomic::Ordering::Relaxed);
                                session.writer_readonly_flag = Some(prev_flag);
                            } else {
                                session.writer_attached = false;
                            }
                        }
                    }

                    let should_exit = !sessions.is_empty()
                        && sessions.values().all(|s| {
                            matches!(s.state, SessionState::Exited(_)) && s.client_count == 0
                        });
                    if should_exit {
                        sessions.clear();
                        drop(sessions);
                        let _ = std::fs::remove_file(socket_path());
                        std::process::exit(0);
                    }
                }

                match result {
                    StreamExit::SwitchTo(target) => {
                        current_name = target;
                    }
                    StreamExit::Disconnected => break,
                }
            }
        }
    }

    Ok(())
}

fn strip_sgr(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len());
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'[' {
            let start = i;
            i += 2;
            while i < data.len() && !data[i].is_ascii_alphabetic() {
                i += 1;
            }
            if i < data.len() {
                if data[i] == b'm' {
                    // SGR sequence — skip it
                    i += 1;
                    continue;
                }
                // Non-SGR escape sequence — keep it
                out.extend_from_slice(&data[start..=i]);
                i += 1;
            }
        } else {
            out.push(data[i]);
            i += 1;
        }
    }
    out
}

enum StreamExit {
    Disconnected,
    SwitchTo(String),
}

type SocketReader = BufReader<tokio::net::unix::OwnedReadHalf>;
type SocketWriter = BufWriter<tokio::net::unix::OwnedWriteHalf>;

async fn stream_session(
    mut reader: SocketReader,
    mut writer: SocketWriter,
    output_rx: &mut tokio::sync::broadcast::Receiver<Vec<u8>>,
    input_tx: &tokio::sync::mpsc::Sender<SessionCommand>,
    detach_notify: &tokio::sync::Notify,
    switch_notify: &tokio::sync::Notify,
    switch_target: &Arc<std::sync::Mutex<Option<String>>>,
    readonly_flag: &Arc<std::sync::atomic::AtomicBool>,
    sessions: Sessions,
    session_name: String,
    initial_cols: u16,
    initial_rows: u16,
) -> Result<(StreamExit, SocketReader, SocketWriter)> {
    let mut was_readonly = readonly_flag.load(std::sync::atomic::Ordering::Relaxed);
    let mut client_size: Option<(u16, u16)> = Some((initial_cols, initial_rows));
    loop {
        let readonly = readonly_flag.load(std::sync::atomic::Ordering::Relaxed);
        if readonly && !was_readonly {
            let screen_data = {
                let sessions = sessions.lock().await;
                sessions.get(&session_name).map(|s| s.screen_contents())
            };
            if let Some(data) = screen_data {
                write_frame(&mut writer, FRAME_DATA, &strip_sgr(&data)).await?;
            }
        } else if !readonly && was_readonly {
            // Promoted — resize PTY to this client's size, then re-render
            if let Some((cols, rows)) = client_size {
                let _ = input_tx.send(SessionCommand::Resize(cols, rows)).await;
            }
            let screen_data = {
                let sessions = sessions.lock().await;
                sessions.get(&session_name).map(|s| s.screen_contents())
            };
            if let Some(data) = screen_data {
                write_frame(&mut writer, FRAME_DATA, &data).await?;
            }
        }
        was_readonly = readonly;
        tokio::select! {
            _ = detach_notify.notified() => {
                return Ok((StreamExit::Disconnected, reader, writer));
            }
            _ = switch_notify.notified() => {
                let target = switch_target.lock().unwrap().take();
                if let Some(target) = target {
                    return Ok((StreamExit::SwitchTo(target), reader, writer));
                }
            }
            data = output_rx.recv() => {
                match data {
                    Ok(data) => {
                        let data = if readonly { strip_sgr(&data) } else { data };
                        write_frame(&mut writer, FRAME_DATA, &data).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("client lagged by {} messages", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        write_frame(&mut writer, FRAME_DATA, b"\r\n[session ended]\r\n").await?;
                        return Ok((StreamExit::Disconnected, reader, writer));
                    }
                }
            }

            frame = read_frame(&mut reader) => {
                match frame? {
                    Some(Frame::Data(data)) => {
                        if !readonly {
                            let _ = input_tx.send(SessionCommand::Input(data)).await;
                        }
                    }
                    Some(Frame::Resize { cols, rows }) => {
                        client_size = Some((cols, rows));
                        if !readonly {
                            let _ = input_tx.send(SessionCommand::Resize(cols, rows)).await;
                        }
                    }
                    Some(Frame::Control(_)) => {
                        return Ok((StreamExit::Disconnected, reader, writer));
                    }
                    None => {
                        return Ok((StreamExit::Disconnected, reader, writer));
                    }
                }
            }
        }
    }
}
