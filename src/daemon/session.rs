use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd};
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

fn resolve_command(cmd: &str, env: &HashMap<String, String>) -> String {
    if cmd.contains('/') {
        return cmd.to_string();
    }
    if let Some(path) = env.get("PATH") {
        for dir in path.split(':') {
            let full = format!("{}/{}", dir, cmd);
            if std::fs::metadata(&full).is_ok() {
                return full;
            }
        }
    }
    cmd.to_string()
}

pub struct Session {
    pub name: String,
    pub command: String,
    pub pid: Pid,
    pub master_fd: i32,
    pub created_at: u64,
    /// Monotonic stamp of the last time a client attached or switched in.
    /// Creation counts as the first opening; the chooser sorts by this.
    pub last_opened: u64,
    pub state: SessionState,
    pub output_tx: broadcast::Sender<Vec<u8>>,
    pub input_tx: mpsc::Sender<SessionCommand>,
    pub detach_notify: Arc<Notify>,
    pub switch_notify: Arc<Notify>,
    pub switch_target: Arc<std::sync::Mutex<Option<String>>>,
    /// Geometry of every attached client. The PTY is sized to the smallest of
    /// them, so a second terminal joining cannot reflow the session under
    /// someone already working in it.
    pub client_sizes: HashMap<u64, (u16, u16)>,
    pub next_client_id: u64,
    pub return_stack: Vec<String>,
    parser: std::sync::Arc<std::sync::Mutex<vt100::Parser>>,
}

pub enum SessionCommand {
    Input(Vec<u8>),
    Resize(u16, u16),
}

/// The escape sequences that put a fresh terminal into this screen's input
/// modes. Diffed against a pristine screen so vt100 does the encoding.
fn input_modes(screen: &vt100::Screen) -> Vec<u8> {
    let pristine = vt100::Parser::default();
    screen.input_mode_diff(pristine.screen())
}

/// One counter for the whole daemon: whoever was opened last sorts first.
fn next_open_stamp() -> u64 {
    use std::sync::atomic::{AtomicU64, Ordering};
    static OPEN_SEQ: AtomicU64 = AtomicU64::new(1);
    OPEN_SEQ.fetch_add(1, Ordering::Relaxed)
}

impl Session {
    /// Register a newly attached client's geometry and return its id.
    pub fn add_client(&mut self, cols: u16, rows: u16) -> u64 {
        let id = self.next_client_id;
        self.next_client_id += 1;
        self.client_sizes.insert(id, (cols, rows));
        // Attaching and switching in both land here; the cancel repaint,
        // deliberately, does not.
        self.last_opened = next_open_stamp();
        id
    }

    pub fn set_client_size(&mut self, id: u64, cols: u16, rows: u16) {
        if let Some(size) = self.client_sizes.get_mut(&id) {
            *size = (cols, rows);
        }
    }

    pub fn remove_client(&mut self, id: u64) {
        self.client_sizes.remove(&id);
    }

    pub fn client_count(&self) -> usize {
        self.client_sizes.len()
    }

    /// The largest geometry every attached client can render, i.e. the
    /// per-axis minimum. `None` when nobody is attached, in which case the PTY
    /// keeps whatever size it last had.
    pub fn effective_size(&self) -> Option<(u16, u16)> {
        self.client_sizes
            .values()
            .copied()
            .reduce(|(ac, ar), (c, r)| (ac.min(c), ar.min(r)))
    }

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

        // Resolve command before fork (safe to use std library here)
        let resolved_command = command
            .as_ref()
            .and_then(|parts| parts.first().map(|cmd| resolve_command(cmd, &env)));

        // Build env vars before fork
        let mut env_vars: Vec<std::ffi::CString> = env
            .iter()
            .filter(|(k, _)| {
                k.as_str() != "TERM"
                    && k.as_str() != "TRIP_SESSION"
                    && k.as_str() != "TRIP_WORKSPACE"
            })
            .map(|(k, v)| std::ffi::CString::new(format!("{}={}", k, v)).unwrap())
            .collect();
        env_vars.push(std::ffi::CString::new(format!("TRIP_SESSION={}", name)).unwrap());
        // The workspace this session belongs to — TRIP_SESSION with any .N
        // numbering stripped. Exported separately so prompts and scripts can
        // key on the workspace without reimplementing the split (and getting
        // a name like `next.js` wrong).
        env_vars.push(
            std::ffi::CString::new(format!(
                "TRIP_WORKSPACE={}",
                crate::client::session_base(&name)
            ))
            .unwrap(),
        );
        env_vars.push(std::ffi::CString::new("TERM=xterm-256color").unwrap());

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
                        let resolved = resolved_command.unwrap_or_else(|| parts[0].clone());
                        let cmd = std::ffi::CString::new(resolved).unwrap();
                        let args: Vec<std::ffi::CString> = parts
                            .iter()
                            .map(|a| std::ffi::CString::new(a.as_str()).unwrap())
                            .collect();
                        (cmd, args)
                    }
                    _ => {
                        // An empty SHELL is as useless as an absent one, and
                        // execve on "" fails silently: the child exits, the
                        // daemon reaps the session, and whoever created it is
                        // left attaching to something that no longer exists.
                        let shell = env
                            .get("SHELL")
                            .filter(|s| !s.is_empty())
                            .cloned()
                            .unwrap_or_else(|| "/bin/sh".into());
                        let cmd = std::ffi::CString::new(shell.as_str()).unwrap();
                        let basename = shell.rsplit('/').next().unwrap_or(&shell);
                        let login_name = std::ffi::CString::new(format!("-{}", basename)).unwrap();
                        let args = vec![login_name];
                        (cmd, args)
                    }
                };

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
                    .unwrap_or_else(|| {
                        std::env::var("SHELL")
                            .ok()
                            .filter(|s| !s.is_empty())
                            .unwrap_or_else(|| "/bin/sh".into())
                    });

                let created_at = recording::now_ts() as u64;

                // Set up session directory
                let session_dir = crate::common::session_dir(&name);
                std::fs::create_dir_all(&session_dir).ok();
                let log_path = crate::common::log_path(&name);

                crate::common::write_session_meta(&crate::common::SessionMeta {
                    name: name.clone(),
                    command: command.clone(),
                    cwd: cwd.clone(),
                    created_at,
                });

                // Spawn the PTY I/O task
                let output_tx_clone = output_tx.clone();
                let parser_clone = parser.clone();
                let pty_session_name = name.clone();
                tokio::spawn(async move {
                    pty_io_loop(
                        async_fd,
                        input_rx,
                        output_tx_clone,
                        parser_clone,
                        pty_session_name,
                        log_path,
                    )
                    .await;
                });

                // Watch for agent.json and tail agent logs when present
                let agent_session_name = name.clone();
                tokio::spawn(async move {
                    loop {
                        if super::agent::read_agent_config(&agent_session_name).is_some() {
                            super::agent::tail_agent_log(agent_session_name.clone()).await;
                        }
                        tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                    }
                });

                Ok(Session {
                    name,
                    command: cmd_str,
                    pid: child,
                    master_fd: raw_fd,
                    created_at,
                    last_opened: next_open_stamp(),
                    state: SessionState::Running,
                    output_tx,
                    input_tx,
                    detach_notify: Arc::new(Notify::new()),
                    switch_notify: Arc::new(Notify::new()),
                    switch_target: Arc::new(std::sync::Mutex::new(None)),
                    client_sizes: HashMap::new(),
                    next_client_id: 0,
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

    pub fn title(&self) -> String {
        let parser = self.parser.lock().unwrap();
        parser.screen().title().to_string()
    }

    pub fn screen_contents(&self) -> Vec<u8> {
        let parser = self.parser.lock().unwrap();
        let screen = parser.screen();
        let mut output = Vec::new();

        output.extend_from_slice(b"\x1b[2J\x1b[H");
        output.extend_from_slice(&screen.contents_formatted());

        // Cells are not the whole screen. `contents_formatted` writes what is
        // *in* the grid and nothing about the modes the program running inside
        // turned on -- bracketed paste, application keypad and cursor, mouse
        // reporting. That was invisible while a re-render only ever happened
        // on attach, into a terminal that had just been reset anyway. It stops
        // being invisible the moment a re-render lands on a live app, which is
        // what cancelling out of the chooser does: vim would come back without
        // mouse reporting, and a paste into it would no longer be bracketed.
        //
        // Diffing against a pristine screen yields exactly the sequences that
        // turn this screen's modes on, using vt100's own encoder.
        // `state_formatted()` would cover the modes too, but it also emits a
        // title, which would race the client's TitlePrefixer and name sessions
        // that never set one.
        output.extend_from_slice(&input_modes(screen));

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

fn has_cursor_positioning(data: &[u8]) -> bool {
    let mut i = 0;
    while i < data.len() {
        if data[i] == 0x1b && i + 1 < data.len() && data[i + 1] == b'[' {
            i += 2;
            // Skip numeric params and semicolons
            while i < data.len() && (data[i].is_ascii_digit() || data[i] == b';') {
                i += 1;
            }
            if i < data.len() {
                match data[i] {
                    // H/f = cursor position, A/B = up/down (not just scrolling — used with params)
                    b'H' | b'f' => return true,
                    _ => {}
                }
                i += 1;
            }
        } else {
            i += 1;
        }
    }
    false
}

fn take_snapshot(
    parser: &std::sync::Arc<std::sync::Mutex<vt100::Parser>>,
    session_name: &str,
    log_path: &std::path::Path,
    last_screen: &mut String,
    tui_mode: bool,
) {
    if !tui_mode {
        return;
    }

    let p = parser.lock().unwrap();
    let screen = p.screen();

    if super::agent::agent_config_path(session_name).exists() {
        return;
    }

    let (cursor_row, _) = screen.cursor_position();
    let screen_text = screen
        .contents()
        .lines()
        .enumerate()
        .filter(|(i, _)| *i != cursor_row as usize)
        .map(|(_, l)| l)
        .collect::<Vec<_>>()
        .join("\n");
    drop(p);

    let filtered = recording::clean_screen(&screen_text);
    if filtered != *last_screen {
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
    session_name: String,
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
    let tui_mode_path = crate::common::session_dir(&session_name).join("tui_mode");
    let mut tui_mode = false;

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

                                // Detect TUI mode from cursor positioning
                                if !tui_mode && has_cursor_positioning(&data) {
                                    tui_mode = true;
                                    std::fs::write(&tui_mode_path, "").ok();
                                }
                                // Reset TUI mode when preexec removes the marker
                                if tui_mode && !tui_mode_path.exists() {
                                    tui_mode = false;
                                    last_screen.clear();
                                }

                                {
                                    let mut p = parser.lock().unwrap();
                                    p.process(&data);
                                }

                                let agent_active =
                                    super::agent::agent_config_path(&session_name).exists();

                                if !tui_mode && !agent_active {
                                    recording::append_event(&log_path, &RecordEvent::Output {
                                        t: recording::now_ts(),
                                        data: String::from_utf8_lossy(&data).into_owned(),
                                    });
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
                take_snapshot(&parser, &session_name, &log_path, &mut last_screen, tui_mode);
                snapshot_pending = false;
                idle_deadline.as_mut().reset(Instant::now() + Duration::from_secs(86400));
                max_deadline.as_mut().reset(Instant::now() + Duration::from_secs(86400));
            }

            _ = &mut max_deadline, if snapshot_pending => {
                take_snapshot(&parser, &session_name, &log_path, &mut last_screen, tui_mode);
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

#[cfg(test)]
mod tests {
    use super::input_modes;

    fn modes(input: &[u8]) -> Vec<u8> {
        let mut parser = vt100::Parser::default();
        parser.process(input);
        input_modes(parser.screen())
    }

    #[test]
    fn a_pristine_screen_asks_for_nothing() {
        assert!(modes(b"hello").is_empty());
    }

    #[test]
    fn bracketed_paste_survives_a_re_render() {
        // The regression this guards: a screen re-rendered into a live app
        // used to carry cells only, so Esc out of the chooser left the editor
        // underneath without bracketed paste.
        let out = modes(b"\x1b[?2004h");
        assert_eq!(out, b"\x1b[?2004h");
    }

    #[test]
    fn mouse_reporting_and_its_encoding_survive() {
        let out = modes(b"\x1b[?1002h\x1b[?1006h");
        let out = String::from_utf8_lossy(&out).into_owned();
        assert!(out.contains("\x1b[?1002h"), "button-motion tracking: {:?}", out);
        assert!(out.contains("\x1b[?1006h"), "SGR encoding: {:?}", out);
    }

    #[test]
    fn application_cursor_and_keypad_survive() {
        let out = String::from_utf8_lossy(&modes(b"\x1b[?1h\x1b=")).into_owned();
        assert!(out.contains("\x1b[?1h"), "application cursor: {:?}", out);
        assert!(out.contains("\x1b="), "application keypad: {:?}", out);
    }

    #[test]
    fn a_mode_turned_back_off_is_not_asked_for() {
        assert!(modes(b"\x1b[?2004h\x1b[?2004l").is_empty());
    }
}
