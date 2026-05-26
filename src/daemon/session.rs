use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::atomic::AtomicBool;
use std::sync::Arc;

use anyhow::Result;
use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::sys::termios::{self, OutputFlags, SetArg};
use nix::unistd::{self, ForkResult, Pid};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::{broadcast, mpsc, Notify};

use super::diff;
use super::protocol::SessionState;
use super::recording::{self, RecordEvent};

pub struct Session {
    pub name: String,
    pub command: String,
    pub pid: Pid,
    pub master_fd: i32,
    pub created_at: u64,
    pub state: SessionState,
    pub client_count: usize,
    pub output_tx: broadcast::Sender<Vec<u8>>,
    pub input_tx: mpsc::Sender<SessionCommand>,
    pub detach_notify: Arc<Notify>,
    pub switch_notify: Arc<Notify>,
    pub switch_target: Arc<std::sync::Mutex<Option<String>>>,
    pub writer_attached: bool,
    pub writer_readonly_flag: Option<Arc<AtomicBool>>,
    pub writer_stack: Vec<Arc<AtomicBool>>,
    pub return_stack: Vec<String>,
    parser: std::sync::Arc<std::sync::Mutex<vt100::Parser>>,
}

pub enum SessionCommand {
    Input(Vec<u8>),
    Resize(u16, u16),
}

impl Session {
    pub fn spawn(
        name: String,
        command: Option<Vec<String>>,
        cwd: String,
        cols: u16,
        rows: u16,
        env: HashMap<String, String>,
    ) -> Result<Self> {
        let winsize = nix::pty::Winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };

        let OpenptyResult { master, slave } = openpty(&winsize, None)?;

        match unsafe { unistd::fork()? } {
            ForkResult::Child => {
                drop(master);

                unistd::setsid().ok();
                std::env::set_current_dir(&cwd).ok();

                // Ensure the slave has ONLCR set so \n → \r\n
                if let Ok(mut attrs) = termios::tcgetattr(&slave) {
                    attrs.output_flags |= OutputFlags::OPOST | OutputFlags::ONLCR;
                    let _ = termios::tcsetattr(&slave, SetArg::TCSANOW, &attrs);
                }

                unsafe { libc::ioctl(slave.as_raw_fd(), libc::TIOCSCTTY as _, 0) };

                unistd::dup2(slave.as_raw_fd(), 0).ok();
                unistd::dup2(slave.as_raw_fd(), 1).ok();
                unistd::dup2(slave.as_raw_fd(), 2).ok();

                if slave.as_raw_fd() > 2 {
                    drop(slave);
                }

                let (cmd, args) = match &command {
                    Some(parts) if !parts.is_empty() => {
                        let cmd = std::ffi::CString::new(parts[0].as_str()).unwrap();
                        let args: Vec<std::ffi::CString> = parts
                            .iter()
                            .map(|a| std::ffi::CString::new(a.as_str()).unwrap())
                            .collect();
                        (cmd, args)
                    }
                    _ => {
                        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
                        let cmd = std::ffi::CString::new(shell.as_str()).unwrap();
                        let basename = shell.rsplit('/').next().unwrap_or(&shell);
                        let login_name = std::ffi::CString::new(format!("-{}", basename)).unwrap();
                        let args = vec![login_name];
                        (cmd, args)
                    }
                };

                let filtered_env_keys: &[&str] = &[
                    "TERM_PROGRAM",
                    "TERM_PROGRAM_VERSION",
                    "COLORTERM",
                    "LC_TERMINAL",
                    "LC_TERMINAL_VERSION",
                ];
                let mut env_vars: Vec<std::ffi::CString> = std::env::vars()
                    .filter(|(k, _)| !filtered_env_keys.contains(&k.as_str()))
                    .filter(|(k, _)| k != "TERM" && k != "TRIP_SESSION")
                    .map(|(k, v)| std::ffi::CString::new(format!("{}={}", k, v)).unwrap())
                    .collect();
                env_vars.push(std::ffi::CString::new(format!("TRIP_SESSION={}", name)).unwrap());
                env_vars.push(std::ffi::CString::new("TERM=xterm-256color").unwrap());
                for (key, val) in &env {
                    env_vars.push(std::ffi::CString::new(format!("{}={}", key, val)).unwrap());
                }

                eprintln!("trip · {}", name);
                unistd::execve(&cmd, &args, &env_vars).ok();
                std::process::exit(1);
            }
            ForkResult::Parent { child } => {
                drop(slave);

                let raw_fd = master.as_raw_fd();
                unsafe {
                    let flags = libc::fcntl(raw_fd, libc::F_GETFL);
                    libc::fcntl(raw_fd, libc::F_SETFL, flags | libc::O_NONBLOCK);
                }

                let async_fd = AsyncFd::with_interest(master, Interest::READABLE)?;
                let parser = vt100::Parser::new(rows, cols, 0);
                let parser = std::sync::Arc::new(std::sync::Mutex::new(parser));

                let (output_tx, _) = broadcast::channel(256);
                let (input_tx, input_rx) = mpsc::channel(64);

                let cmd_str = command
                    .as_ref()
                    .map(|c| c.join(" "))
                    .unwrap_or_else(|| std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into()));

                let created_at = recording::now_ts() as u64;

                // Set up session directories
                let screens_dir = crate::common::screens_dir(&name);
                std::fs::create_dir_all(&screens_dir).ok();
                let log_path = crate::common::log_path(&name);

                // Spawn the PTY I/O task
                let output_tx_clone = output_tx.clone();
                let parser_clone = parser.clone();
                tokio::spawn(async move {
                    pty_io_loop(
                        async_fd,
                        input_rx,
                        output_tx_clone,
                        parser_clone,
                        screens_dir,
                        log_path,
                    )
                    .await;
                });

                Ok(Session {
                    name,
                    command: cmd_str,
                    pid: child,
                    master_fd: raw_fd,
                    created_at,
                    state: SessionState::Running,
                    client_count: 0,
                    output_tx,
                    input_tx,
                    detach_notify: Arc::new(Notify::new()),
                    switch_notify: Arc::new(Notify::new()),
                    switch_target: Arc::new(std::sync::Mutex::new(None)),
                    writer_attached: false,
                    writer_readonly_flag: None,
                    writer_stack: Vec::new(),
                    return_stack: Vec::new(),
                    parser,
                })
            }
        }
    }

    pub fn screen_text(&self) -> String {
        let parser = self.parser.lock().unwrap();
        parser.screen().contents()
    }

    pub fn screen_contents(&self) -> Vec<u8> {
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        let mut output = Vec::new();

        output.extend_from_slice(b"\x1b[2J\x1b[H");
        output.extend_from_slice(&screen.contents_formatted());

        let (row, col) = screen.cursor_position();
        output.extend_from_slice(format!("\x1b[{};{}H", row + 1, col + 1).as_bytes());

        if !screen.hide_cursor() {
            output.extend_from_slice(b"\x1b[?25h");
        } else {
            output.extend_from_slice(b"\x1b[?25l");
        }

        output
    }
}

fn take_snapshot(
    parser: &std::sync::Arc<std::sync::Mutex<vt100::Parser>>,
    screens_dir: &std::path::Path,
    log_path: &std::path::Path,
    screen_seq: &mut u32,
    last_screen: &mut String,
) {
    let screen_text = {
        let p = parser.lock().unwrap();
        let screen = p.screen();
        let (cursor_row, _) = screen.cursor_position();
        let text = screen.contents();
        text.lines()
            .enumerate()
            .filter(|(i, _)| *i != cursor_row as usize)
            .map(|(_, l)| l)
            .collect::<Vec<_>>()
            .join("\n")
    };
    let filtered = recording::clean_screen(&screen_text);
    if filtered != *last_screen {
        let path = screens_dir.join(format!("{:04}.txt", *screen_seq));
        std::fs::write(&path, &filtered).ok();
        *screen_seq += 1;

        let inserted = diff::inserted_lines(last_screen, &filtered);
        if !inserted.is_empty() {
            let diff_text = recording::clean_screen(&inserted.join("\n"));
            if !diff_text.trim().is_empty() {
                recording::append_event(
                    log_path,
                    &RecordEvent::Screen {
                        t: recording::now_ts(),
                        text: diff_text,
                    },
                );
            }
        }
        *last_screen = filtered;
    }
}

async fn pty_io_loop(
    master: AsyncFd<OwnedFd>,
    mut input_rx: mpsc::Receiver<SessionCommand>,
    output_tx: broadcast::Sender<Vec<u8>>,
    parser: std::sync::Arc<std::sync::Mutex<vt100::Parser>>,
    screens_dir: std::path::PathBuf,
    log_path: std::path::PathBuf,
) {
    use std::time::Duration;
    use tokio::time::{sleep, Instant};

    let mut buf = vec![0u8; 4096];
    let idle_duration = Duration::from_millis(500);
    let max_interval = Duration::from_secs(5);
    let mut idle_deadline = Box::pin(sleep(Duration::from_secs(86400)));
    let mut max_deadline = Box::pin(sleep(Duration::from_secs(86400)));
    let mut snapshot_pending = false;
    let mut last_screen = String::new();
    let mut screen_seq: u32 = 0;

    loop {
        tokio::select! {
            readable = master.readable() => {
                match readable {
                    Ok(mut guard) => {
                        match guard.try_io(|inner| {
                            let fd = inner.get_ref().as_raw_fd();
                            let n = unsafe {
                                libc::read(fd, buf.as_mut_ptr() as *mut _, buf.len())
                            };
                            if n < 0 {
                                Err(std::io::Error::last_os_error())
                            } else {
                                Ok(n as usize)
                            }
                        }) {
                            Ok(Ok(0)) => break,
                            Ok(Ok(n)) => {
                                let data = buf[..n].to_vec();
                                recording::append_event(&log_path, &RecordEvent::Output {
                                    t: recording::now_ts(),
                                    data: String::from_utf8_lossy(&data).into_owned(),
                                });
                                {
                                    let mut p = parser.lock().unwrap();
                                    p.process(&data);
                                }
                                let _ = output_tx.send(data);
                                // Reset idle timer; start max timer if not already running
                                idle_deadline.as_mut().reset(Instant::now() + idle_duration);
                                if !snapshot_pending {
                                    max_deadline.as_mut().reset(Instant::now() + max_interval);
                                }
                                snapshot_pending = true;
                            }
                            Ok(Err(_)) => break,
                            Err(_would_block) => {}
                        }
                    }
                    Err(_) => break,
                }
            }

            _ = &mut idle_deadline, if snapshot_pending => {
                take_snapshot(&parser, &screens_dir, &log_path, &mut screen_seq, &mut last_screen);
                snapshot_pending = false;
                idle_deadline.as_mut().reset(Instant::now() + Duration::from_secs(86400));
                max_deadline.as_mut().reset(Instant::now() + Duration::from_secs(86400));
            }

            _ = &mut max_deadline, if snapshot_pending => {
                take_snapshot(&parser, &screens_dir, &log_path, &mut screen_seq, &mut last_screen);
                snapshot_pending = false;
                idle_deadline.as_mut().reset(Instant::now() + Duration::from_secs(86400));
                max_deadline.as_mut().reset(Instant::now() + Duration::from_secs(86400));
            }

            cmd = input_rx.recv() => {
                match cmd {
                    Some(SessionCommand::Input(data)) => {
                        recording::append_event(&log_path, &RecordEvent::Input {
                            t: recording::now_ts(),
                            data: String::from_utf8_lossy(&data).into_owned(),
                        });
                        let fd = master.get_ref().as_raw_fd();
                        unsafe {
                            libc::write(fd, data.as_ptr() as *const _, data.len());
                        }
                    }
                    Some(SessionCommand::Resize(cols, rows)) => {
                        recording::append_event(&log_path, &RecordEvent::Resize { t: recording::now_ts(), cols, rows });
                        let winsize = libc::winsize {
                            ws_row: rows,
                            ws_col: cols,
                            ws_xpixel: 0,
                            ws_ypixel: 0,
                        };
                        unsafe {
                            libc::ioctl(
                                master.get_ref().as_raw_fd(),
                                libc::TIOCSWINSZ,
                                &winsize,
                            );
                        }
                        let mut p = parser.lock().unwrap();
                        p.set_size(rows, cols);
                    }
                    None => break,
                }
            }
        }
    }
}
