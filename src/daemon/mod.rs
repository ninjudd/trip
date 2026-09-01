pub mod agent;
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

use crate::common::{lock_path, socket_path, terminal_env_path, trip_dir};
use protocol::{
    read_frame, write_control, write_frame, Frame, Request, Response, SessionInfo, SessionState,
    FRAME_DATA,
};
use recording::RecordEvent;
use session::{Session, SessionCommand};

fn format_events(events: &[RecordEvent], raw: bool, verbose: bool) -> String {
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
            match event {
                RecordEvent::Output { data, .. } => {
                    out.push_str(&recording::strip_escapes(data));
                }
                RecordEvent::Screen { text, .. } => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                    out.push('\n');
                }
                RecordEvent::AgentText { text, .. } => {
                    if !out.is_empty() {
                        out.push('\n');
                    }
                    out.push_str(text);
                    out.push('\n');
                }
                RecordEvent::AgentThinking { text, .. } => {
                    out.push_str("[thinking] ");
                    out.push_str(text.lines().next().unwrap_or(""));
                    out.push('\n');
                }
                RecordEvent::AgentToolCall { name, input, .. } => {
                    out.push_str(&format!("[tool] {}", name));
                    if let Some(cmd) = input.get("command").and_then(|v| v.as_str()) {
                        out.push_str(&format!(": {}", cmd));
                    } else if let Some(path) = input.get("file_path").and_then(|v| v.as_str()) {
                        out.push_str(&format!(": {}", path));
                    }
                    out.push('\n');
                }
                RecordEvent::AgentToolResult {
                    is_error, output, ..
                } if verbose => {
                    let prefix = if *is_error { "[error] " } else { "[result] " };
                    let summary = match output {
                        serde_json::Value::String(s) => {
                            let trimmed = s.trim();
                            if trimmed.len() > 200 {
                                format!("{}...", &trimmed[..200])
                            } else {
                                trimmed.to_string()
                            }
                        }
                        _ => String::new(),
                    };
                    if !summary.is_empty() {
                        out.push_str(prefix);
                        out.push_str(&summary);
                        out.push('\n');
                    }
                }
                RecordEvent::AgentTurnEnd {
                    duration_ms: Some(ms),
                    ..
                } => {
                    out.push_str(&format!("[turn] {}ms\n", ms));
                }
                _ => {}
            }
        }
        out
    }
}

type Sessions = Arc<Mutex<HashMap<String, Session>>>;

/// Append a timestamped line to ~/.trip/daemon.log. The daemon's stderr is
/// only captured when a client spawned it, so anything worth keeping goes
/// through here to reach the file no matter how the daemon was started.
pub fn dlog(msg: &str) {
    use std::io::Write;
    let ts = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(crate::common::daemon_log_path())
    {
        let _ = writeln!(f, "[{}] {}", ts, msg);
    }
}

pub async fn run() -> Result<()> {
    let dir = trip_dir();
    std::fs::create_dir_all(&dir)?;

    // Every session dies with this process, so a silent death is a real
    // loss — record panics even from tasks that don't take the daemon down.
    std::panic::set_hook(Box::new(|info| {
        dlog(&format!("panic: {}", info));
        eprintln!("panic: {}", info);
    }));

    // Detach from controlling terminal so closing a tab doesn't kill us.
    // Fails when we are already a process group leader (started by hand
    // from a shell) — that daemon stays tied to its terminal's lifetime.
    if let Err(e) = nix::unistd::setsid() {
        dlog(&format!(
            "setsid failed ({}); daemon still tied to the terminal that started it",
            e
        ));
    }

    let lock_file = std::fs::File::create(lock_path())?;
    let ret = unsafe { libc::flock(lock_file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) };
    if ret != 0 {
        anyhow::bail!("daemon already running");
    }

    let sock_path = socket_path();
    let _ = std::fs::remove_file(&sock_path);
    let listener = UnixListener::bind(&sock_path)?;

    dlog(&format!(
        "daemon started (pid {}, version {})",
        std::process::id(),
        env!("CARGO_PKG_VERSION")
    ));

    let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));

    let sessions_reaper = sessions.clone();
    tokio::spawn(async move {
        reap_children(sessions_reaper).await;
    });

    loop {
        // One failed accept (fd exhaustion, an aborted connection) must not
        // take down the daemon: it would kill every session with it.
        let (stream, _) = match listener.accept().await {
            Ok(pair) => pair,
            Err(e) => {
                dlog(&format!("accept error: {}", e));
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                continue;
            }
        };
        let sessions = sessions.clone();
        tokio::spawn(async move {
            if let Err(e) = handle_client(stream, sessions).await {
                dlog(&format!("client error: {}", e));
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
                        crate::common::remove_session_meta(&session.name);
                        session.detach_notify.notify_waiters();
                    }
                    Ok(WaitStatus::Signaled(_, _, _)) => {
                        session.state = SessionState::Exited(-1);
                        crate::common::remove_session_meta(&session.name);
                        session.detach_notify.notify_waiters();
                    }
                    _ => {}
                }
            }
        }

        // Remove exited sessions with no clients
        sessions
            .retain(|_, s| !(matches!(s.state, SessionState::Exited(_)) && s.client_count() == 0));

        if sessions.is_empty() {
            dlog("last session exited; daemon exiting");
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
        Request::CreateSession {
            name,
            command,
            cwd,
            env,
        } => {
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

            let session = Session::spawn(name.clone(), command, cwd, 80, 24, env.clone())?;
            let pid = session.pid.as_raw() as u32;
            sessions.insert(name.clone(), session);
            write_terminal_env(&name, &env);

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
                    let git_branch = cwd.as_ref().and_then(|p| procinfo::get_git_branch(p));

                    let title = s.title();
                    let title = if title.is_empty() { None } else { Some(title) };

                    SessionInfo {
                        name: s.name.clone(),
                        command: s.command.clone(),
                        pid: s.pid.as_raw() as u32,
                        created_at: s.created_at,
                        last_opened: s.last_opened,
                        state: s.state.clone(),
                        attached: s.client_count() > 0,
                        cwd: cwd.map(|p| p.to_string_lossy().into_owned()),
                        fg_command,
                        git_branch,
                        title,
                    }
                })
                .collect();
            write_control(&mut writer, &Response::SessionList { sessions: list }).await?;
        }

        Request::GetScreen { name, watch } => {
            if !watch {
                let sessions = sessions.lock().await;
                match sessions.get(&name) {
                    Some(s) => {
                        write_control(
                            &mut writer,
                            &Response::ScreenData {
                                content: s.screen_text(),
                            },
                        )
                        .await?;
                    }
                    None => {
                        write_control(
                            &mut writer,
                            &Response::Error {
                                message: format!("session '{}' not found", name),
                            },
                        )
                        .await?;
                    }
                }
            } else {
                let mut output_rx = {
                    let sessions = sessions.lock().await;
                    match sessions.get(&name) {
                        Some(s) => {
                            write_control(
                                &mut writer,
                                &Response::ScreenData {
                                    content: s.screen_text(),
                                },
                            )
                            .await?;
                            s.output_tx.subscribe()
                        }
                        None => {
                            write_control(
                                &mut writer,
                                &Response::Error {
                                    message: format!("session '{}' not found", name),
                                },
                            )
                            .await?;
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
                                )
                                .await
                                {
                                    Ok(Ok(_)) => continue,
                                    Ok(Err(tokio::sync::broadcast::error::RecvError::Lagged(
                                        _,
                                    ))) => continue,
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
                                write_control(
                                    &mut writer,
                                    &Response::ScreenData {
                                        content: current.clone(),
                                    },
                                )
                                .await?;
                                last_screen = current;
                            }
                        }
                        Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                        Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                    }
                }
            }
        }

        Request::GetLog {
            name,
            raw,
            verbose,
            follow,
            since,
        } => {
            let log_path = crate::common::log_path(&name);

            // Read existing log from disk
            let events = if log_path.exists() {
                let content = std::fs::read_to_string(&log_path).unwrap_or_default();
                content
                    .lines()
                    .filter_map(|line| serde_json::from_str::<RecordEvent>(line).ok())
                    .filter(|e| since.is_none_or(|ts| e.timestamp() >= ts))
                    .collect::<Vec<_>>()
            } else {
                Vec::new()
            };

            let content = format_events(&events, raw, verbose);
            if !content.is_empty() {
                write_control(&mut writer, &Response::LogData { content }).await?;
            }

            if follow {
                // Follow by watching log file for new lines
                let output_rx = {
                    let sessions = sessions.lock().await;
                    sessions.get(&name).map(|s| s.output_tx.subscribe())
                };
                if let Some(mut rx) = output_rx {
                    let mut last_size = std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
                    loop {
                        match rx.recv().await {
                            Ok(_) => {
                                tokio::time::sleep(std::time::Duration::from_millis(600)).await;
                                let current_size =
                                    std::fs::metadata(&log_path).map(|m| m.len()).unwrap_or(0);
                                if current_size > last_size {
                                    let content =
                                        std::fs::read_to_string(&log_path).unwrap_or_default();
                                    let new_lines: String = content
                                        .bytes()
                                        .skip(last_size as usize)
                                        .map(|b| b as char)
                                        .collect();
                                    let new_events: Vec<RecordEvent> = new_lines
                                        .lines()
                                        .filter_map(|line| serde_json::from_str(line).ok())
                                        .collect();
                                    let formatted = format_events(&new_events, raw, verbose);
                                    if !formatted.is_empty() {
                                        write_control(
                                            &mut writer,
                                            &Response::LogData { content: formatted },
                                        )
                                        .await?;
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

        Request::SwitchSession {
            from,
            to,
            command,
            cwd,
            env,
        } => {
            let mut sessions = sessions.lock().await;

            if !sessions.contains_key(&to) {
                match Session::spawn(to.clone(), command, cwd, 80, 24, env.clone()) {
                    Ok(session) => {
                        sessions.insert(to.clone(), session);
                        write_terminal_env(&to, &env);
                    }
                    Err(e) => {
                        write_control(
                            &mut writer,
                            &Response::Error {
                                message: format!("failed to create session: {}", e),
                            },
                        )
                        .await?;
                        return Ok(());
                    }
                }
            }

            // Push onto target's return stack
            if let Some(target) = sessions.get_mut(&to) {
                target.return_stack.push(from.clone());
            }

            // Signal the attach client on `from` to switch
            if let Some(session) = sessions.get(&from) {
                *session.switch_target.lock().unwrap() = Some(to.clone());
                session.switch_notify.notify_waiters();
                write_control(&mut writer, &Response::Ok).await?;
            } else {
                write_control(
                    &mut writer,
                    &Response::Error {
                        message: format!("session '{}' not found", from),
                    },
                )
                .await?;
            }
        }

        // Only meaningful inside an attached stream, where the socket
        // identifies the client. On a fresh connection there is no client to
        // move.
        Request::SwitchTo { .. } => {
            write_control(
                &mut writer,
                &Response::Error {
                    message: "switch_to is only valid on an attached session".to_string(),
                },
            )
            .await?;
        }

        Request::ReturnSession { name } => {
            let mut sessions = sessions.lock().await;
            let stack = sessions
                .get_mut(&name)
                .map(|s| std::mem::take(&mut s.return_stack));
            if let Some(mut stack) = stack {
                let target = loop {
                    match stack.pop() {
                        Some(t) if sessions.contains_key(&t) => {
                            // Put remaining entries back
                            if let Some(session) = sessions.get_mut(&name) {
                                session.return_stack = stack;
                            }
                            break Some(t);
                        }
                        Some(_) => continue,
                        None => break None,
                    }
                };
                if let Some(target) = target {
                    if let Some(session) = sessions.get(&name) {
                        *session.switch_target.lock().unwrap() = Some(target);
                        session.switch_notify.notify_waiters();
                    }
                    write_control(&mut writer, &Response::Ok).await?;
                } else {
                    write_control(
                        &mut writer,
                        &Response::Error {
                            message: "no session to return to".to_string(),
                        },
                    )
                    .await?;
                }
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
                // The reaper never sees this session (it's out of the map
                // before the SIGCHLD lands), so clean up its meta here.
                crate::common::remove_session_meta(&name);
                sessions.remove(&name);

                if sessions.is_empty() {
                    drop(sessions);
                    dlog("last session killed; daemon exiting");
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
            dlog(&format!(
                "shutdown requested; killing {} session(s)",
                sessions.len()
            ));
            for session in sessions.values() {
                nix::sys::signal::kill(session.pid, nix::sys::signal::Signal::SIGHUP).ok();
                crate::common::remove_session_meta(&session.name);
            }
            sessions.clear();
            drop(sessions);
            let _ = std::fs::remove_file(socket_path());
            write_control(&mut writer, &Response::Ok).await?;
            tokio::time::sleep(std::time::Duration::from_millis(50)).await;
            std::process::exit(0);
        }

        Request::Attach {
            name,
            cols,
            rows,
            env: client_env,
        } => {
            let mut current_name = name;
            let mut current_cols = cols;
            let mut current_rows = rows;
            let mut first = true;
            let mut client_id: u64;

            loop {
                let (
                    screen_data,
                    mut output_rx,
                    input_tx,
                    detach_notify,
                    switch_notify,
                    switch_target,
                    readonly,
                    readonly_flag,
                ) = {
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

                    // Every client attaches writable. readonly stays on the
                    // wire and in the stream loop so a deliberate read-only
                    // attach can set it later; nothing sets it today.
                    let readonly_flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
                    client_id = session.add_client(current_cols, current_rows);
                    write_terminal_env(&current_name, &client_env);
                    if let Some((cols, rows)) = session.effective_size() {
                        let _ = session
                            .input_tx
                            .send(SessionCommand::Resize(cols, rows))
                            .await;
                    }
                    let screen = session.screen_contents();
                    let rx = session.output_tx.subscribe();
                    let tx = session.input_tx.clone();
                    let detach = session.detach_notify.clone();
                    let sw_notify = session.switch_notify.clone();
                    let sw_target = session.switch_target.clone();
                    (
                        screen,
                        rx,
                        tx,
                        detach,
                        sw_notify,
                        sw_target,
                        readonly_flag.load(std::sync::atomic::Ordering::Relaxed),
                        readonly_flag,
                    )
                };

                if first {
                    write_control(&mut writer, &Response::Attached { readonly }).await?;
                    first = false;
                }
                let screen_data = if readonly {
                    strip_sgr(&screen_data)
                } else {
                    screen_data
                };
                write_frame(&mut writer, FRAME_DATA, &screen_data).await?;

                let streamed = stream_session(
                    reader,
                    writer,
                    &mut output_rx,
                    &input_tx,
                    &detach_notify,
                    &switch_notify,
                    &switch_target,
                    &readonly_flag,
                    sessions.clone(),
                    current_name.clone(),
                    client_id,
                    current_cols,
                    current_rows,
                )
                .await;

                // Detach bookkeeping has to happen however the stream ended.
                // `?` here skipped all of it on the error path -- a client
                // dying while the daemon is mid-write gives a failed socket
                // write rather than a clean EOF -- leaving the session counted
                // as attached and its geometry pinned to a terminal nobody is
                // looking at any more.
                {
                    let mut sessions = sessions.lock().await;
                    if let Some(session) = sessions.get_mut(&current_name) {
                        // Carry this client's *live* geometry forward. On a
                        // switch the next iteration re-registers it, and the
                        // attach-time size is stale by then -- which, now that
                        // the map feeds a shared minimum, would hold an
                        // unrelated client at a size neither terminal has.
                        if let Some(&(c, r)) = session.client_sizes.get(&client_id) {
                            current_cols = c;
                            current_rows = r;
                        }
                        session.remove_client(client_id);
                        // A big terminal leaving should give the remaining
                        // clients their room back.
                        if let Some((cols, rows)) = session.effective_size() {
                            let _ = session
                                .input_tx
                                .send(SessionCommand::Resize(cols, rows))
                                .await;
                        }
                    }

                    try_gc_session(&mut sessions, &current_name);

                    let should_exit = !sessions.is_empty()
                        && sessions.values().all(|s| {
                            matches!(s.state, SessionState::Exited(_)) && s.client_count() == 0
                        });
                    if should_exit {
                        sessions.clear();
                        drop(sessions);
                        dlog("last client detached from exited session(s); daemon exiting");
                        let _ = std::fs::remove_file(socket_path());
                        std::process::exit(0);
                    }
                }

                let (result, r, w) = streamed?;
                reader = r;
                writer = w;

                match result {
                    StreamExit::SwitchTo(target) => {
                        // Delayed GC — let trip enter exit before checking
                        let old_name = current_name.clone();
                        let sessions_gc = sessions.clone();
                        tokio::spawn(async move {
                            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                            let mut sessions = sessions_gc.lock().await;
                            try_gc_session(&mut sessions, &old_name);
                        });
                        current_name = target;
                        write_control(
                            &mut writer,
                            &Response::SessionName {
                                name: current_name.clone(),
                            },
                        )
                        .await?;
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

const TERMINAL_ENV_KEYS: &[&str] = &[
    "TERM_PROGRAM",
    "TERM_PROGRAM_VERSION",
    "COLORTERM",
    "LC_TERMINAL",
    "LC_TERMINAL_VERSION",
];

fn write_terminal_env(name: &str, env: &HashMap<String, String>) {
    let path = terminal_env_path(name);
    let mut content = String::new();
    for &key in TERMINAL_ENV_KEYS {
        if let Some(val) = env.get(key) {
            content.push_str(&format!("export {}={}\n", key, shell_escape(val)));
        } else {
            content.push_str(&format!("unset {}\n", key));
        }
    }
    let tmp = path.with_extension("tmp");
    if std::fs::write(&tmp, &content).is_ok() {
        std::fs::rename(&tmp, &path).ok();
    }
}

fn shell_escape(s: &str) -> String {
    if s.chars()
        .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-' || c == '.' || c == '/')
    {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "'\\''"))
    }
}

fn is_numbered_session(name: &str) -> bool {
    // One rule for `.N`, owned by the client's name helpers. This used to be
    // a third copy, and it disagreed with the others: an empty suffix is
    // vacuously all-digits, so `trip.` was GC-eligible as a numbered session
    // while being its own workspace everywhere else.
    crate::client::session_base(name) != name
}

fn try_gc_session(sessions: &mut HashMap<String, Session>, name: &str) {
    let should_kill = sessions.get(name).is_some_and(|s| {
        is_numbered_session(name)
            && s.client_count() == 0
            && matches!(s.state, SessionState::Exited(_))
    });
    if should_kill {
        sessions.remove(name);
    }
}

/// Create the target if it is missing and record where the client came from,
/// for a switch that one client asked for on its own socket.
///
/// The return stack is not updated when `to` is the session the client is
/// already on. The chooser routes both Cancel and picking the current session
/// through a self-switch, and a self-entry is one today's flow cannot produce
/// -- `SwitchSession` only ever pushes a different `from`, and `enter` returns
/// early rather than switching a session to itself. `ReturnSession` pops the
/// topmost entry that still *exists*, so a self-entry would pass that check
/// and make `trip return` a no-op that quietly consumes one entry per cancel.
async fn prepare_switch_target(
    sessions: &Sessions,
    from: &str,
    to: &str,
    allocate: bool,
    command: Option<Vec<String>>,
    cwd: String,
    env: HashMap<String, String>,
) -> Result<String> {
    let mut sessions = sessions.lock().await;

    // Resolved while the table is locked, so the name cannot be taken between
    // choosing it and creating it.
    let to = &if allocate {
        next_free_name(&sessions, crate::client::session_base(to))
    } else {
        to.to_string()
    };

    if !sessions.contains_key(to) {
        // A session made from the chooser belongs where the session it was
        // opened from is standing, the way `trip new` inherits the shell's
        // directory. The client's own cwd is wherever its terminal was
        // launched, which may be far staler.
        let cwd = sessions
            .get(from)
            .and_then(|s| procinfo::get_foreground_pid(s.master_fd))
            .and_then(procinfo::get_cwd)
            .map(|p| p.to_string_lossy().into_owned())
            .unwrap_or(cwd);
        let session = Session::spawn(to.to_string(), command, cwd, 80, 24, env.clone())?;
        sessions.insert(to.to_string(), session);
        write_terminal_env(to, &env);
    }

    if from != to {
        if let Some(target) = sessions.get_mut(to) {
            target.return_stack.push(from.to_string());
        }
    }
    Ok(to.to_string())
}

fn next_free_name(sessions: &HashMap<String, Session>, base: &str) -> String {
    (1..)
        .map(|n| format!("{}.{}", base, n))
        .find(|candidate| !sessions.contains_key(candidate))
        .expect("an unbounded search always finds a free name")
}

enum StreamExit {
    Disconnected,
    SwitchTo(String),
}

type SocketReader = BufReader<tokio::net::unix::OwnedReadHalf>;
type SocketWriter = BufWriter<tokio::net::unix::OwnedWriteHalf>;

#[allow(clippy::too_many_arguments)]
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
    client_id: u64,
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
                        let ro = readonly_flag.load(std::sync::atomic::Ordering::Relaxed);
                        let data = if ro { strip_sgr(&data) } else { data };
                        write_frame(&mut writer, FRAME_DATA, &data).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        dlog(&format!("client lagged by {} messages", n));
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
                        if !readonly_flag.load(std::sync::atomic::Ordering::Relaxed) {
                            let _ = input_tx.send(SessionCommand::Input(data)).await;
                        }
                    }
                    Some(Frame::Resize { cols, rows }) => {
                        client_size = Some((cols, rows));
                        // Record this client's new geometry, then re-fit the
                        // PTY to the smallest attached client rather than to
                        // whoever resized last.
                        let fitted = {
                            let mut sessions = sessions.lock().await;
                            match sessions.get_mut(&session_name) {
                                Some(session) => {
                                    session.set_client_size(client_id, cols, rows);
                                    session.effective_size()
                                }
                                None => None,
                            }
                        };
                        if let Some((cols, rows)) = fitted {
                            let _ = input_tx.send(SessionCommand::Resize(cols, rows)).await;
                        }
                    }
                    Some(Frame::Control(payload)) => {
                        // A client switching itself, rather than a hangup.
                        // Everything else on this socket still means goodbye.
                        match serde_json::from_slice::<Request>(&payload) {
                            Ok(Request::SwitchTo {
                                to,
                                allocate,
                                command,
                                cwd,
                                env,
                            }) => {
                                let repaint = |sessions: &HashMap<String, Session>| {
                                    sessions.get(&session_name).map(|s| s.screen_contents())
                                };
                                match prepare_switch_target(
                                    &sessions,
                                    &session_name,
                                    &to,
                                    allocate,
                                    command,
                                    cwd,
                                    env,
                                )
                                .await
                                {
                                    // A switch to the session the client is
                                    // already on -- the chooser's Cancel, and
                                    // picking the row you are on. Repaint in
                                    // place rather than detaching and
                                    // re-attaching: the full round trip runs
                                    // detach bookkeeping, which resizes the
                                    // shared PTY twice under every other
                                    // client, and try_gc_session can reap an
                                    // *exited* session in the gap where this
                                    // client is not counted -- Esc would
                                    // destroy the session it was declining to
                                    // leave.
                                    Ok(target) if target == session_name => {
                                        let screen = repaint(&*sessions.lock().await);
                                        if let Some(data) = screen {
                                            write_frame(&mut writer, FRAME_DATA, &data).await?;
                                        }
                                        write_control(
                                            &mut writer,
                                            &Response::SessionName {
                                                name: session_name.clone(),
                                            },
                                        )
                                        .await?;
                                        continue;
                                    }
                                    Ok(target) => {
                                        return Ok((StreamExit::SwitchTo(target), reader, writer))
                                    }
                                    // The client tore its chooser down when it
                                    // sent the request and is waiting for a
                                    // repaint that a bare error line is not.
                                    // Put the session's screen back, then say
                                    // what happened on top of it.
                                    Err(e) => {
                                        let screen = repaint(&*sessions.lock().await);
                                        if let Some(data) = screen {
                                            write_frame(&mut writer, FRAME_DATA, &data).await?;
                                        }
                                        let msg = format!("\r\n[switch failed: {}]\r\n", e);
                                        write_frame(&mut writer, FRAME_DATA, msg.as_bytes())
                                            .await?;
                                        write_control(
                                            &mut writer,
                                            &Response::SessionName {
                                                name: session_name.clone(),
                                            },
                                        )
                                        .await?;
                                        continue;
                                    }
                                }
                            }
                            _ => return Ok((StreamExit::Disconnected, reader, writer)),
                        }
                    }
                    None => {
                        return Ok((StreamExit::Disconnected, reader, writer));
                    }
                }
            }
        }
    }
}
