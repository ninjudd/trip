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

/// Split a numbered session name (`foo.3` → `("foo", 3)`). Only a purely
/// numeric suffix counts as numbering — a workspace named `next.js` stays
/// whole.
fn split_numbered(name: &str) -> Option<(&str, u64)> {
    let (base, suffix) = name.rsplit_once('.')?;
    if base.is_empty() || suffix.is_empty() || !suffix.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    Some((base, suffix.parse().ok()?))
}

/// The workspace base name a session belongs to: its name with any `.N`
/// numbering stripped.
pub fn session_base(name: &str) -> &str {
    split_numbered(name).map(|(base, _)| base).unwrap_or(name)
}

/// Sort key that puts the canonical session first, then numbered sessions in
/// numeric order.
fn group_order(name: &str) -> Option<u64> {
    split_numbered(name).map(|(_, n)| n)
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

enum Key {
    Up,
    Down,
    Enter,
    Cancel,
    Digit(usize),
    Other,
}

/// Read one keypress from a raw-mode terminal, byte by byte so that several
/// keys arriving in one burst (held arrows, paste) parse individually.
fn read_key(fd: std::os::fd::BorrowedFd) -> Key {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};

    fn read_byte(fd: std::os::fd::BorrowedFd) -> Option<u8> {
        use std::os::fd::AsRawFd;
        let mut b = [0u8; 1];
        match nix::unistd::read(fd.as_raw_fd(), &mut b) {
            Ok(1) => Some(b[0]),
            _ => None,
        }
    }

    let Some(b) = read_byte(fd) else {
        return Key::Cancel;
    };
    match b {
        b'\r' | b'\n' => Key::Enter,
        0x03 | b'q' => Key::Cancel,
        b'k' => Key::Up,
        b'j' => Key::Down,
        d @ b'1'..=b'9' => Key::Digit((d - b'0') as usize),
        0x1b => {
            // Arrow keys arrive as ESC [ A/B (or ESC O A/B); a lone ESC is a
            // cancel. The terminal writes a whole arrow sequence at once, so
            // a short poll tells them apart.
            let mut fds = [PollFd::new(fd, PollFlags::POLLIN)];
            let pending = matches!(poll(&mut fds, PollTimeout::from(25u16)), Ok(n) if n > 0);
            if !pending {
                return Key::Cancel;
            }
            if !matches!(read_byte(fd), Some(b'[') | Some(b'O')) {
                return Key::Other;
            }
            match read_byte(fd) {
                Some(b'A') => Key::Up,
                Some(b'B') => Key::Down,
                _ => Key::Other,
            }
        }
        _ => Key::Other,
    }
}

/// Interactive list selector: ↑/↓ (or j/k) move the highlight, Enter
/// confirms, 1-9 jump directly, and q/Esc/Ctrl-C cancel. Renders to stderr,
/// redrawing the list in place. Returns the chosen index, or None on cancel.
fn select_choice(rows: &[String]) -> Option<usize> {
    use nix::sys::termios::{self, LocalFlags, SetArg};
    use std::os::fd::{AsRawFd, BorrowedFd};

    let fd = std::io::stdin().as_raw_fd();
    let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
    let original = termios::tcgetattr(borrowed).ok();
    if let Some(ref orig) = original {
        let mut raw = orig.clone();
        // ISIG off so Ctrl-C arrives as a byte and we can restore the
        // terminal instead of dying with the cursor hidden
        raw.local_flags &= !(LocalFlags::ICANON | LocalFlags::ECHO | LocalFlags::ISIG);
        termios::tcsetattr(borrowed, SetArg::TCSANOW, &raw).ok();
    }
    eprint!("\x1b[?25l");

    let render = |selected: usize, redraw: bool| {
        let mut out = String::new();
        if redraw {
            out.push_str(&format!("\r\x1b[{}A", rows.len()));
        }
        for (i, row) in rows.iter().enumerate() {
            if i == selected {
                out.push_str(&format!("\x1b[2K> \x1b[7m{}\x1b[0m\n", row));
            } else {
                out.push_str(&format!("\x1b[2K  {}\n", row));
            }
        }
        eprint!("{}", out);
    };

    let mut selected = 0usize;
    render(selected, false);

    let result = loop {
        match read_key(borrowed) {
            Key::Enter => break Some(selected),
            Key::Cancel => break None,
            Key::Up => {
                selected = if selected == 0 {
                    rows.len() - 1
                } else {
                    selected - 1
                };
            }
            Key::Down => {
                selected = (selected + 1) % rows.len();
            }
            Key::Digit(n) if n <= rows.len() => break Some(n - 1),
            _ => continue,
        }
        render(selected, true);
    };

    eprint!("\x1b[?25h");
    if let Some(ref orig) = original {
        termios::tcsetattr(borrowed, SetArg::TCSANOW, orig).ok();
    }
    result
}

/// Build the picker rows: (session name, what it is running, status tag).
///
/// Pure so the list can be tested without a terminal. `scope` of `Some(base)`
/// restricts to one workspace and offers the canonical session as a
/// to-be-created first entry; `None` lists every workspace's sessions, where
/// there is no single canonical session to offer.
fn session_choices(
    sessions: &[crate::daemon::protocol::SessionInfo],
    scope: Option<&str>,
    current: Option<&str>,
) -> Vec<(String, String, String)> {
    let mut group: Vec<_> = sessions
        .iter()
        .filter(|s| scope.is_none_or(|base| session_base(&s.name) == base))
        .collect();
    // Grouped by workspace, then by number within it — the same order `ls`
    // uses, so the two views agree.
    group.sort_by(|a, b| {
        (session_base(&a.name), group_order(&a.name))
            .cmp(&(session_base(&b.name), group_order(&b.name)))
    });

    let mut choices: Vec<(String, String, String)> = Vec::new();
    if let Some(base) = scope {
        if !group.iter().any(|s| s.name == base) {
            choices.push((base.to_string(), String::new(), "(new session)".to_string()));
        }
    }
    for s in &group {
        let cmd = s
            .title
            .as_deref()
            .or(s.fg_command.as_deref())
            .unwrap_or(&s.command);
        let tag = if current == Some(s.name.as_str()) {
            "(current)"
        } else if s.attached {
            "(attached)"
        } else if matches!(s.state, SessionState::Exited(_)) {
            "(exited)"
        } else {
            ""
        };
        choices.push((s.name.clone(), cmd.to_string(), tag.to_string()));
    }
    choices
}

/// Let the user pick which session to enter.
///
/// `scope` of `Some(base)` is the default: only this workspace, and only when
/// it has sessions beyond the canonical one — otherwise plain `trip enter`
/// keeps its old behaviour of going straight to the canonical session,
/// creating it if missing. `None` is `--all`: every workspace's sessions, and
/// always a picker, because choosing is the point of asking.
///
/// Returns None if the user cancels.
async fn pick_session(scope: Option<&str>) -> Result<Option<String>> {
    use std::io::IsTerminal;
    let interactive = std::io::stdin().is_terminal() && std::io::stderr().is_terminal();

    if !interactive {
        return match scope {
            // A script running `trip enter` gets the canonical session.
            Some(base) => Ok(Some(base.to_string())),
            // ...but `--all` has no sensible default to fall back on.
            None => anyhow::bail!("trip enter --all needs a terminal to choose with"),
        };
    }

    let sessions = get_session_list().await?;
    let current = std::env::var("TRIP_SESSION").ok();
    let choices = session_choices(&sessions, scope, current.as_deref());

    match scope {
        // Nothing extra in this workspace: behave exactly as before the picker.
        Some(base) if !choices.iter().any(|(name, _, _)| name != base) => {
            return Ok(Some(base.to_string()));
        }
        None if choices.is_empty() => {
            eprintln!("no sessions");
            return Ok(None);
        }
        _ => {}
    }

    match scope {
        Some(base) => eprintln!(
            "sessions for '{}':  \x1b[2m↑/↓ + enter · 1-9 · q cancels\x1b[0m",
            base
        ),
        None => eprintln!("all sessions:  \x1b[2m↑/↓ + enter · 1-9 · q cancels\x1b[0m"),
    }

    let nw = choices
        .iter()
        .map(|(name, _, _)| name.len())
        .max()
        .unwrap_or(0);
    let cw = choices
        .iter()
        .map(|(_, cmd, _)| cmd.len())
        .max()
        .unwrap_or(0);
    let rows: Vec<String> = choices
        .iter()
        .enumerate()
        .map(|(i, (name, cmd, tag))| {
            format!("{}) {:<nw$}  {:<cw$}  {}", i + 1, name, cmd, tag)
                .trim_end()
                .to_string()
        })
        .collect();

    match select_choice(&rows) {
        Some(i) => Ok(Some(choices[i].0.clone())),
        None => {
            eprintln!("cancelled");
            Ok(None)
        }
    }
}

pub async fn enter(name: Option<String>, all: bool, command: Option<Vec<String>>) -> Result<()> {
    let name = match name {
        Some(n) => n,
        None => {
            // --all widens the picker past this workspace; clap already
            // rejects it alongside an explicit name.
            let base = if all {
                None
            } else {
                Some(derive_session_name()?)
            };
            match pick_session(base.as_deref()).await? {
                Some(n) => n,
                None => return Ok(()),
            }
        }
    };

    if let Ok(current) = std::env::var("TRIP_SESSION") {
        if current == name {
            println!("already in session '{}'", name);
            return Ok(());
        }
        // Set the terminal title before switching
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

    let missing = format!("session '{}' not found", name);

    // The list answers one question only: is someone else holding this
    // session. Whether it needs creating is left to the attach below, which
    // is the one observation that cannot be stale.
    if session.map(|s| s.attached) == Some(true) {
        eprint!("session '{}' is in use. take over? [y/n] ", name);
        if read_yn() {
            // This prompt waits on a human, so the session can be long gone by
            // the time we act on the answer — a far wider window than the
            // millisecond ones elsewhere here. Nothing to take over is not a
            // failure: fall through and let the attach below create it.
            match take_over(name.clone()).await {
                Ok(()) => {}
                Err(e) if e.to_string() == missing => {}
                Err(e) => return Err(e),
            }
        } else {
            eprintln!();
        }
    }

    // A name with no session and a session that went away before we reached it
    // are the same situation here: the attach says it is not there, so create
    // it and attach. Deciding that from the list instead would decide it on an
    // older round trip, and that gap is what made `enter` report "not found"
    // instead of honouring its create-or-attach contract.
    //
    // Matched by message because the daemon reports errors as strings; keep in
    // step with the Attach handler in daemon/mod.rs. A drift in wording gives
    // back the old error rather than misfiring.
    match attach::attach(name.clone()).await {
        Err(e) if e.to_string() == missing => {
            // It can equally appear between that failed attach and this
            // create — another `enter`, or the switch path — and the daemon
            // rejects the duplicate. Attaching is what we wanted anyway.
            let exists = format!("session '{}' already exists", name);
            match create_session(name.clone(), command).await {
                Ok(()) => {}
                Err(e) if e.to_string() == exists => {}
                Err(e) => return Err(e),
            }
            attach::attach(name).await
        }
        other => other,
    }
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

pub async fn list_sessions(all: bool, attached_only: bool) -> Result<()> {
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
                    // "which sessions are attached" is a cross-workspace
                    // question, so --attached implies the all-workspace view
                    let grouped = all || attached_only;
                    let scope = if grouped {
                        None
                    } else {
                        Some(derive_session_name()?)
                    };
                    let mut sessions: Vec<_> = sessions
                        .into_iter()
                        .filter(|s| !attached_only || s.attached)
                        .filter(|s| {
                            scope
                                .as_deref()
                                .is_none_or(|base| session_base(&s.name) == base)
                        })
                        .collect();
                    if sessions.is_empty() {
                        match &scope {
                            Some(base) => {
                                println!("no sessions for '{}' (try: trip ls -a)", base)
                            }
                            None if attached_only => println!("no attached sessions"),
                            None => println!("no sessions"),
                        }
                        return Ok(());
                    }
                    sessions.sort_by(|a, b| {
                        (session_base(&a.name), group_order(&a.name))
                            .cmp(&(session_base(&b.name), group_order(&b.name)))
                    });

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
                                session_base(&s.name).to_string(),
                                marker.to_string(),
                                s.name.clone(),
                                cmd.to_string(),
                                branch.to_string(),
                                cwd,
                            )
                        })
                        .collect();

                    let nw = rows.iter().map(|r| r.2.len()).max().unwrap_or(0);
                    let cw = rows.iter().map(|r| r.3.len()).max().unwrap_or(0);
                    let bw = rows.iter().map(|r| r.4.len()).max().unwrap_or(0);

                    let indent = if grouped { "  " } else { "" };
                    let mut last_base: Option<&str> = None;
                    for (base, marker, name, cmd, branch, cwd) in &rows {
                        if grouped && last_base != Some(base.as_str()) {
                            if last_base.is_some() {
                                println!();
                            }
                            println!("{}", base);
                            last_base = Some(base);
                        }
                        println!(
                            "{indent}{} {:<nw$}  {:<cw$}  {:<bw$}  {}",
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
    let session_name = match std::env::var("TRIP_SESSION") {
        Ok(name) => name,
        Err(_) => return Ok(()),
    };

    let (kind, log_path) = if let Some((k, p)) = read_hook_stdin()? {
        (k, p)
    } else if let Ok(session_id) = std::env::var("CLAUDE_CODE_SESSION_ID") {
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
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
        return Ok(());
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

fn read_hook_stdin() -> Result<Option<(String, String)>> {
    use std::io::IsTerminal;
    if std::io::stdin().is_terminal() {
        return Ok(None);
    }
    let mut input = String::new();
    std::io::Read::read_to_string(&mut std::io::stdin(), &mut input)?;
    if input.is_empty() {
        return Ok(None);
    }
    let v: serde_json::Value = serde_json::from_str(&input)?;
    if let Some(path) = v.get("transcript_path").and_then(|p| p.as_str()) {
        return Ok(Some((kind_for_transcript(path), path.to_string())));
    }
    Ok(None)
}

/// Which engine wrote the transcript a hook just handed us.
///
/// The hook payload does not say. It used to be assumed to be codex, but
/// Claude Code's `SessionStart` hook passes exactly this shape — it is the
/// setup trip's own README recommends — so a Claude agent registered through
/// a hook got an `agent.json` claiming codex against a Claude transcript. The
/// codex parser keys on `session_meta`/`event_msg`/`response_item`, none of
/// which a Claude transcript contains, so the session registered cleanly and
/// then produced no agent events at all.
///
/// The transcript's own location is the most direct evidence, since it names
/// the file we are about to parse. The engines' environment markers are the
/// fallback for a transcript kept somewhere else.
fn kind_for_transcript(path: &str) -> String {
    if path.contains("/.codex/") {
        return "codex".to_string();
    }
    if path.contains("/.claude/") {
        return "claude".to_string();
    }
    if std::env::var("CODEX_THREAD_ID").is_ok() {
        return "codex".to_string();
    }
    "claude".to_string()
}

#[cfg(test)]
mod name_tests {
    use super::{group_order, session_base, split_numbered};

    #[test]
    fn numbered_names_split_into_base_and_number() {
        assert_eq!(split_numbered("trip.1"), Some(("trip", 1)));
        assert_eq!(
            split_numbered("ninjudd/trip.12"),
            Some(("ninjudd/trip", 12))
        );
        assert_eq!(session_base("ninjudd/trip.3"), "ninjudd/trip");
        assert_eq!(session_base("ninjudd/trip"), "ninjudd/trip");
    }

    #[test]
    fn a_dotted_workspace_name_is_not_numbering() {
        // ~/next.js derives the session name `next.js`; splitting on the dot
        // would wrongly group it under `next`.
        assert_eq!(split_numbered("next.js"), None);
        assert_eq!(session_base("next.js"), "next.js");
    }

    #[test]
    fn an_all_numeric_dotted_suffix_is_ambiguous_and_reads_as_numbering() {
        // A workspace literally named `v2.0` collides with numbering; the
        // numeric reading wins because `.N` is trip's own convention.
        assert_eq!(session_base("v2.0"), "v2");
    }

    #[test]
    fn degenerate_names_stay_whole() {
        assert_eq!(session_base("trip."), "trip.");
        assert_eq!(session_base(".1"), ".1");
    }

    #[test]
    fn canonical_sorts_before_numbered() {
        let mut names = vec!["trip.10", "trip.2", "trip"];
        names.sort_by_key(|n| group_order(n));
        assert_eq!(names, vec!["trip", "trip.2", "trip.10"]);
    }
}

#[cfg(test)]
mod kind_tests {
    use super::kind_for_transcript;

    #[test]
    fn claude_transcripts_are_claude() {
        // The regression: this path used to resolve to codex, and the codex
        // parser then dropped every event in it.
        assert_eq!(
            kind_for_transcript(
                "/Users/x/.claude/projects/-Users-x-repo/65056f26-3825-446a-a866-ee21e6cb1220.jsonl"
            ),
            "claude"
        );
    }

    #[test]
    fn codex_transcripts_are_codex() {
        assert_eq!(
            kind_for_transcript("/Users/x/.codex/sessions/2026/08/22/rollout-abc.jsonl"),
            "codex"
        );
    }

    #[test]
    fn an_unplaceable_transcript_falls_back_to_the_environment() {
        // Neither home directory appears, so the engine's own marker decides.
        std::env::set_var("CODEX_THREAD_ID", "t-1");
        assert_eq!(kind_for_transcript("/tmp/elsewhere.jsonl"), "codex");
        std::env::remove_var("CODEX_THREAD_ID");
        assert_eq!(kind_for_transcript("/tmp/elsewhere.jsonl"), "claude");
    }
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::daemon::protocol::{SessionInfo, SessionState};

    fn session(name: &str, attached: bool) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            command: "bash".to_string(),
            pid: 1,
            created_at: 0,
            state: SessionState::Running,
            attached,
            cwd: None,
            fg_command: None,
            git_branch: None,
            title: None,
        }
    }

    fn names(choices: &[(String, String, String)]) -> Vec<&str> {
        choices.iter().map(|(n, _, _)| n.as_str()).collect()
    }

    #[test]
    fn scoped_choices_cover_only_that_workspace() {
        let sessions = vec![
            session("acme/webapp", false),
            session("acme/webapp.2", false),
            session("other/api", false),
        ];
        let c = session_choices(&sessions, Some("acme/webapp"), None);
        assert_eq!(names(&c), vec!["acme/webapp", "acme/webapp.2"]);
    }

    #[test]
    fn scoped_choices_offer_the_canonical_session_when_missing() {
        // Only a numbered session exists, so entering should still be able to
        // create the canonical one — it leads the list.
        let sessions = vec![session("acme/webapp.2", false)];
        let c = session_choices(&sessions, Some("acme/webapp"), None);
        assert_eq!(names(&c), vec!["acme/webapp", "acme/webapp.2"]);
        assert_eq!(c[0].2, "(new session)");
    }

    #[test]
    fn all_choices_span_workspaces_grouped_and_ordered() {
        let sessions = vec![
            session("other/api.2", false),
            session("acme/webapp.10", false),
            session("other/api", false),
            session("acme/webapp", false),
            session("acme/webapp.2", false),
        ];
        let c = session_choices(&sessions, None, None);
        assert_eq!(
            names(&c),
            vec![
                "acme/webapp",
                "acme/webapp.2",
                "acme/webapp.10",
                "other/api",
                "other/api.2",
            ]
        );
    }

    #[test]
    fn all_choices_never_offer_a_new_session() {
        // Across workspaces there is no single canonical session to create.
        let sessions = vec![session("acme/webapp.2", false)];
        let c = session_choices(&sessions, None, None);
        assert_eq!(names(&c), vec!["acme/webapp.2"]);
        assert!(c.iter().all(|(_, _, tag)| tag != "(new session)"));
    }

    #[test]
    fn tags_mark_current_and_attached() {
        let sessions = vec![session("acme/webapp", true), session("other/api", false)];
        let c = session_choices(&sessions, None, Some("other/api"));
        assert_eq!(c[0].2, "(attached)");
        assert_eq!(c[1].2, "(current)");
    }

    #[test]
    fn no_sessions_gives_no_choices() {
        assert!(session_choices(&[], None, None).is_empty());
    }
}
