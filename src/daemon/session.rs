use std::collections::VecDeque;
use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::Serialize;

use anyhow::Result;
use nix::libc;
use nix::pty::{openpty, OpenptyResult};
use nix::sys::termios::{self, OutputFlags, SetArg};
use nix::unistd::{self, ForkResult, Pid};
use tokio::io::unix::AsyncFd;
use tokio::io::Interest;
use tokio::sync::{broadcast, mpsc, Notify};

use super::protocol::SessionState;

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
    parser: std::sync::Arc<std::sync::Mutex<vt100::Parser>>,
    pub recording: Arc<std::sync::Mutex<Recording>>,
}

#[derive(Clone, Serialize)]
#[serde(tag = "type")]
pub enum RecordEvent {
    #[serde(rename = "output")]
    Output { t: f64, data: String },
    #[serde(rename = "input")]
    Input { t: f64, data: String },
    #[serde(rename = "resize")]
    Resize { t: f64, cols: u16, rows: u16 },
    #[serde(rename = "screen")]
    Screen { t: f64, text: String },
}

impl RecordEvent {
    pub fn timestamp(&self) -> f64 {
        match self {
            RecordEvent::Output { t, .. } => *t,
            RecordEvent::Input { t, .. } => *t,
            RecordEvent::Resize { t, .. } => *t,
            RecordEvent::Screen { t, .. } => *t,
        }
    }

    fn data_len(&self) -> usize {
        match self {
            RecordEvent::Output { data, .. } => data.len(),
            RecordEvent::Input { data, .. } => data.len(),
            RecordEvent::Resize { .. } => 8,
            RecordEvent::Screen { text, .. } => text.len(),
        }
    }

    pub fn is_screen(&self) -> bool {
        matches!(self, RecordEvent::Screen { .. })
    }
}

pub struct Recording {
    events: VecDeque<RecordEvent>,
    total_bytes: usize,
    max_bytes: usize,
}

impl Recording {
    pub fn new(max_bytes: usize) -> Self {
        Self {
            events: VecDeque::new(),
            total_bytes: 0,
            max_bytes,
        }
    }

    pub fn push(&mut self, event: RecordEvent) {
        let len = event.data_len();
        self.total_bytes += len;
        self.events.push_back(event);
        while self.total_bytes > self.max_bytes {
            if let Some(old) = self.events.pop_front() {
                self.total_bytes -= old.data_len();
            } else {
                break;
            }
        }
    }

    pub fn events_since(&self, since: f64) -> Vec<RecordEvent> {
        self.events.iter().filter(|e| e.timestamp() >= since).cloned().collect()
    }

    pub fn all_events(&self) -> Vec<RecordEvent> {
        self.events.iter().cloned().collect()
    }
}

fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

pub enum SessionCommand {
    Input(Vec<u8>),
    Resize(u16, u16),
}

impl Session {
    pub fn spawn(name: String, command: Option<Vec<String>>, cwd: String, cols: u16, rows: u16) -> Result<Self> {
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
                std::env::set_var("DRIP_SESSION", &name);
                std::env::set_var("TERM", "xterm-256color");

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
                        let args = vec![cmd.clone()];
                        (cmd, args)
                    }
                };

                unistd::execvp(&cmd, &args).ok();
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
                let recording = Arc::new(std::sync::Mutex::new(Recording::new(
                    crate::common::DEFAULT_SCROLLBACK,
                )));

                let cmd_str = command
                    .as_ref()
                    .map(|c| c.join(" "))
                    .unwrap_or_else(|| {
                        std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into())
                    });

                let created_at = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs();

                // Spawn the PTY I/O task
                let output_tx_clone = output_tx.clone();
                let parser_clone = parser.clone();
                let recording_clone = recording.clone();
                tokio::spawn(async move {
                    pty_io_loop(async_fd, input_rx, output_tx_clone, parser_clone, recording_clone).await;
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
                    parser,
                    recording,
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

async fn pty_io_loop(
    master: AsyncFd<OwnedFd>,
    mut input_rx: mpsc::Receiver<SessionCommand>,
    output_tx: broadcast::Sender<Vec<u8>>,
    parser: std::sync::Arc<std::sync::Mutex<vt100::Parser>>,
    recording: Arc<std::sync::Mutex<Recording>>,
) {
    use std::time::Duration;
    use tokio::time::{sleep, Instant};

    let mut buf = vec![0u8; 4096];
    let idle_duration = Duration::from_millis(500);
    let mut idle_deadline = Box::pin(sleep(Duration::from_secs(86400)));
    let mut snapshot_pending = false;
    let mut last_screen = String::new();

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
                                {
                                    let mut rec = recording.lock().unwrap();
                                    rec.push(RecordEvent::Output {
                                        t: now_ts(),
                                        data: String::from_utf8_lossy(&data).into_owned(),
                                    });
                                }
                                {
                                    let mut p = parser.lock().unwrap();
                                    p.process(&data);
                                }
                                let _ = output_tx.send(data);
                                // Reset idle timer
                                idle_deadline.as_mut().reset(Instant::now() + idle_duration);
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
                let screen_text = {
                    let p = parser.lock().unwrap();
                    p.screen().contents()
                };
                if screen_text != last_screen {
                    let mut rec = recording.lock().unwrap();
                    rec.push(RecordEvent::Screen {
                        t: now_ts(),
                        text: screen_text.clone(),
                    });
                    last_screen = screen_text;
                }
                snapshot_pending = false;
                idle_deadline.as_mut().reset(Instant::now() + Duration::from_secs(86400));
            }

            cmd = input_rx.recv() => {
                match cmd {
                    Some(SessionCommand::Input(data)) => {
                        {
                            let mut rec = recording.lock().unwrap();
                            rec.push(RecordEvent::Input {
                                t: now_ts(),
                                data: String::from_utf8_lossy(&data).into_owned(),
                            });
                        }
                        let fd = master.get_ref().as_raw_fd();
                        unsafe {
                            libc::write(fd, data.as_ptr() as *const _, data.len());
                        }
                    }
                    Some(SessionCommand::Resize(cols, rows)) => {
                        {
                            let mut rec = recording.lock().unwrap();
                            rec.push(RecordEvent::Resize { t: now_ts(), cols, rows });
                        }
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
