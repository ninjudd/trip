use std::os::fd::{AsRawFd, OwnedFd};
use std::sync::Arc;
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
}

#[derive(Clone, Serialize, Deserialize)]
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

fn append_event(log_path: &std::path::Path, event: &RecordEvent) {
    if let Ok(line) = serde_json::to_string(event) {
        use std::io::Write;
        if let Ok(mut f) = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(log_path)
        {
            let _ = writeln!(f, "{}", line);
        }
    }
}

fn is_decorative(line: &str) -> bool {
    let trimmed = line.trim();
    !trimmed.is_empty() && trimmed.chars().all(|c| matches!(c, '─' | '━' | '═' | '│' | '┃' | '║' | '┌' | '┐' | '└' | '┘' | '├' | '┤' | '┬' | '┴' | '┼' | '╔' | '╗' | '╚' | '╝' | '╠' | '╣' | '╦' | '╩' | '╬' | '▔' | '▁' | '─' | ' '))
}

fn clean_screen(text: &str) -> String {
    let mut out = String::new();
    let mut prev_empty = false;
    for line in text.lines() {
        if is_decorative(line) {
            continue;
        }
        let empty = line.trim().is_empty();
        if empty && prev_empty {
            continue;
        }
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(line);
        prev_empty = empty;
    }
    out
}

fn diff_inserted_lines(old: &str, new: &str) -> Vec<String> {
    let old_lines: Vec<&str> = old.lines().collect();
    let new_lines: Vec<&str> = new.lines().collect();

    let m = old_lines.len();
    let n = new_lines.len();
    let mut dp = vec![vec![0u32; n + 1]; m + 1];
    for i in 1..=m {
        for j in 1..=n {
            if old_lines[i - 1] == new_lines[j - 1] {
                dp[i][j] = dp[i - 1][j - 1] + 1;
            } else {
                dp[i][j] = dp[i - 1][j].max(dp[i][j - 1]);
            }
        }
    }

    let mut inserted = Vec::new();
    let mut i = m;
    let mut j = n;
    while i > 0 && j > 0 {
        if old_lines[i - 1] == new_lines[j - 1] {
            i -= 1;
            j -= 1;
        } else if dp[i - 1][j] > dp[i][j - 1] {
            i -= 1;
        } else if dp[i][j - 1] > dp[i - 1][j] {
            inserted.push(new_lines[j - 1].to_string());
            j -= 1;
        } else {
            i -= 1;
            j -= 1;
        }
    }
    while j > 0 {
        inserted.push(new_lines[j - 1].to_string());
        j -= 1;
    }

    inserted.reverse();
    inserted
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

                // Set up session directories
                let screens_dir = crate::common::screens_dir(&name);
                std::fs::create_dir_all(&screens_dir).ok();
                let log_path = crate::common::log_path(&name);

                // Spawn the PTY I/O task
                let output_tx_clone = output_tx.clone();
                let parser_clone = parser.clone();
                tokio::spawn(async move {
                    pty_io_loop(async_fd, input_rx, output_tx_clone, parser_clone, screens_dir, log_path).await;
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
    screens_dir: std::path::PathBuf,
    log_path: std::path::PathBuf,
) {
    use std::time::Duration;
    use tokio::time::{sleep, Instant};

    let mut buf = vec![0u8; 4096];
    let idle_duration = Duration::from_millis(500);
    let mut idle_deadline = Box::pin(sleep(Duration::from_secs(86400)));
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
                                append_event(&log_path, &RecordEvent::Output {
                                    t: now_ts(),
                                    data: String::from_utf8_lossy(&data).into_owned(),
                                });
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
                let filtered = clean_screen(&screen_text);
                if filtered != last_screen {
                    // Write full snapshot to disk
                    let path = screens_dir.join(format!("{:04}.txt", screen_seq));
                    std::fs::write(&path, &filtered).ok();
                    screen_seq += 1;

                    // Compute diff and append to log file
                    let inserted = diff_inserted_lines(&last_screen, &filtered);
                    if !inserted.is_empty() {
                        let diff_text = clean_screen(&inserted.join("\n"));
                        if !diff_text.trim().is_empty() {
                            append_event(&log_path, &RecordEvent::Screen {
                                t: now_ts(),
                                text: diff_text,
                            });
                        }
                    }
                    last_screen = filtered;
                }
                snapshot_pending = false;
                idle_deadline.as_mut().reset(Instant::now() + Duration::from_secs(86400));
            }

            cmd = input_rx.recv() => {
                match cmd {
                    Some(SessionCommand::Input(data)) => {
                        append_event(&log_path, &RecordEvent::Input {
                            t: now_ts(),
                            data: String::from_utf8_lossy(&data).into_owned(),
                        });
                        let fd = master.get_ref().as_raw_fd();
                        unsafe {
                            libc::write(fd, data.as_ptr() as *const _, data.len());
                        }
                    }
                    Some(SessionCommand::Resize(cols, rows)) => {
                        append_event(&log_path, &RecordEvent::Resize { t: now_ts(), cols, rows });
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
