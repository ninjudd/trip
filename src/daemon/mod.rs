pub mod procinfo;
pub mod protocol;
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
    read_frame, write_control, write_frame, Frame, Request, Response, SessionInfo, SessionState,
    FRAME_DATA,
};
use session::{Session, SessionCommand};

type Sessions = Arc<Mutex<HashMap<String, Session>>>;

pub async fn run() -> Result<()> {
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
                    }
                    Ok(WaitStatus::Signaled(_, _, _)) => {
                        session.state = SessionState::Exited(-1);
                    }
                    _ => {}
                }
            }
        }

        let should_exit = !sessions.is_empty()
            && sessions.values().all(|s| {
                matches!(s.state, SessionState::Exited(_)) && s.client_count == 0
            });

        if should_exit {
            sessions.clear();
        }

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

        Request::GetLog { name, raw, follow } => {
            if !follow {
                let sessions = sessions.lock().await;
                let session = match sessions.get(&name) {
                    Some(s) => s,
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
                };

                let content = if raw {
                    String::from_utf8_lossy(&session.raw_transcript()).into_owned()
                } else {
                    session.screen_text()
                };

                write_control(&mut writer, &Response::LogData { content }).await?;
            } else {
                // Follow mode: send current screen, then stream updates
                let (initial, mut output_rx) = {
                    let sessions = sessions.lock().await;
                    let session = match sessions.get(&name) {
                        Some(s) => s,
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
                    };

                    let content = if raw {
                        String::from_utf8_lossy(&session.raw_transcript()).into_owned()
                    } else {
                        session.screen_text()
                    };
                    (content, session.output_tx.subscribe())
                };

                write_control(&mut writer, &Response::LogData { content: initial.clone() }).await?;

                if raw {
                    // Stream raw bytes
                    loop {
                        match output_rx.recv().await {
                            Ok(data) => {
                                let text = String::from_utf8_lossy(&data).into_owned();
                                write_control(&mut writer, &Response::LogData { content: text }).await?;
                            }
                            Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {}
                            Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                        }
                    }
                } else {
                    // Stream screen text updates when output settles
                    let mut last_screen = initial;
                    loop {
                        match output_rx.recv().await {
                            Ok(_) => {
                                // Wait for output to settle (500ms idle)
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
                                // Snapshot screen
                                let current = {
                                    let sessions = sessions.lock().await;
                                    match sessions.get(&name) {
                                        Some(s) => s.screen_text(),
                                        None => break,
                                    }
                                };
                                if current != last_screen {
                                    write_control(&mut writer, &Response::LogData { content: current.clone() }).await?;
                                    last_screen = current;
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

        Request::Attach { name, cols, rows } => {
            let (screen_data, mut output_rx, input_tx, detach_notify) = {
                let mut sessions = sessions.lock().await;
                let session = match sessions.get_mut(&name) {
                    Some(s) => s,
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
                };

                let _ = session
                    .input_tx
                    .send(SessionCommand::Resize(cols, rows))
                    .await;
                session.client_count += 1;
                let screen = session.screen_contents();
                let rx = session.output_tx.subscribe();
                let tx = session.input_tx.clone();
                let detach = session.detach_notify.clone();
                (screen, rx, tx, detach)
            };

            write_control(&mut writer, &Response::Attached).await?;
            write_frame(&mut writer, FRAME_DATA, &screen_data).await?;

            let result =
                stream_session(reader, writer, &mut output_rx, &input_tx, &detach_notify).await;

            let mut sessions = sessions.lock().await;
            if let Some(session) = sessions.get_mut(&name) {
                session.client_count = session.client_count.saturating_sub(1);
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

            result?;
        }
    }

    Ok(())
}

async fn stream_session(
    mut reader: BufReader<tokio::net::unix::OwnedReadHalf>,
    mut writer: BufWriter<tokio::net::unix::OwnedWriteHalf>,
    output_rx: &mut tokio::sync::broadcast::Receiver<Vec<u8>>,
    input_tx: &tokio::sync::mpsc::Sender<SessionCommand>,
    detach_notify: &tokio::sync::Notify,
) -> Result<()> {
    loop {
        tokio::select! {
            _ = detach_notify.notified() => {
                return Ok(());
            }
            data = output_rx.recv() => {
                match data {
                    Ok(data) => {
                        write_frame(&mut writer, FRAME_DATA, &data).await?;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(n)) => {
                        eprintln!("client lagged by {} messages", n);
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => {
                        write_frame(&mut writer, FRAME_DATA, b"\r\n[session ended]\r\n").await?;
                        return Ok(());
                    }
                }
            }

            frame = read_frame(&mut reader) => {
                match frame? {
                    Some(Frame::Data(data)) => {
                        let _ = input_tx.send(SessionCommand::Input(data)).await;
                    }
                    Some(Frame::Resize { cols, rows }) => {
                        let _ = input_tx.send(SessionCommand::Resize(cols, rows)).await;
                    }
                    Some(Frame::Control(_)) => {
                        return Ok(());
                    }
                    None => {
                        return Ok(());
                    }
                }
            }
        }
    }
}
