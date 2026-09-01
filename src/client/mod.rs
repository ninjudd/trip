pub mod attach;
pub mod chooser;
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

/// Drive a [`Chooser`] from this process's own stdin, for the callers that
/// have a terminal to themselves. An attached client cannot use this — a
/// thread there already owns the fd — and feeds the chooser from its channel
/// instead.
///
/// Renders to stderr, so a chooser can run while stdout is a pipe.
fn run_chooser(chooser: &mut chooser::Chooser) -> Option<chooser::Outcome> {
    use nix::poll::{poll, PollFd, PollFlags, PollTimeout};
    use nix::sys::termios::{self, LocalFlags, SetArg};
    use std::io::Write;
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

    let result = loop {
        let painted = chooser.render();
        let mut err = std::io::stderr();
        err.write_all(&painted).ok();
        err.flush().ok();

        // A half-read escape is only resolved by what follows it, or by the
        // silence after it. Same 25ms the byte-at-a-time reader used to give.
        if chooser.pending_escape() {
            let mut fds = [PollFd::new(borrowed, PollFlags::POLLIN)];
            if !matches!(poll(&mut fds, PollTimeout::from(25u16)), Ok(n) if n > 0) {
                match chooser.tick() {
                    Some(outcome) => break Some(outcome),
                    None => continue,
                }
            }
        }

        let mut buf = [0u8; 64];
        match nix::unistd::read(fd, &mut buf) {
            Ok(0) | Err(_) => break None,
            Ok(n) => {
                if let Some(outcome) = chooser.feed(&buf[..n]) {
                    break Some(outcome);
                }
            }
        }
    };

    eprint!("\x1b[?25h");
    if let Some(ref orig) = original {
        termios::tcsetattr(borrowed, SetArg::TCSANOW, orig).ok();
    }
    result
}

/// How many rows the chooser may paint: the window, less the hint line, the
/// two truncation markers, and one spare so the list never scrolls the
/// terminal.
pub(crate) fn chooser_viewport() -> usize {
    (terminal_size().1 as usize).saturating_sub(4).max(3)
}

pub(crate) fn terminal_size() -> (u16, u16) {
    use nix::libc;
    use std::os::fd::AsRawFd;
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if ws.ws_col == 0 {
        (80, 24)
    } else {
        (ws.ws_col, ws.ws_row)
    }
}


/// Which sessions a chooser or listing covers.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Scope {
    /// Only the current workspace. What `--pwd` asks for.
    Pwd,
    /// Every workspace. The default.
    All,
}

/// Build the chooser rows — (name, what it is running, status tag) — and the
/// index to start on.
///
/// Pure, so the ordering and the preselection can be tested without a
/// terminal. `workspace` is always known and is not the same question as
/// `scope`: it says which group leads and which one gets the create row, even
/// when the list spans every workspace.
pub(crate) fn session_choices(
    sessions: &[crate::daemon::protocol::SessionInfo],
    workspace: &str,
    scope: Scope,
    current: Option<&str>,
) -> (Vec<(String, String, String)>, usize) {
    let mut group: Vec<_> = sessions
        .iter()
        .filter(|s| scope == Scope::All || session_base(&s.name) == workspace)
        .collect();
    // The current workspace leads, then the rest alphabetically, numbered
    // sessions after their canonical one. Your own sessions are where your
    // eyes already are, and they take the low digits.
    group.sort_by(|a, b| {
        let key = |name: &str| {
            (
                session_base(name) != workspace,
                session_base(name).to_string(),
                group_order(name),
            )
        };
        key(&a.name).cmp(&key(&b.name))
    });

    let mut choices: Vec<(String, String, String)> = Vec::new();

    // "The next session in this workspace": the canonical one while it is
    // missing, otherwise the number `trip new` would take. Always row 1, so
    // Up-then-Enter is a fixed gesture rather than one that moves with the
    // contents of the list.
    let next = if sessions.iter().any(|s| s.name == workspace) {
        next_available_name(sessions, workspace)
    } else {
        workspace.to_string()
    };
    choices.push((next, String::new(), "(new session)".to_string()));

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

    // The canonical session, failing that the workspace's first surviving one,
    // failing that the create row. The middle clause is `trip` killed while
    // `trip.1` lives: there is nothing canonical to select and the workspace
    // is not empty either. Landing on row 2 there is what keeps the create row
    // directly above the selection in every state.
    let preselected = group
        .iter()
        .position(|s| s.name == workspace)
        .or_else(|| {
            group
                .iter()
                .position(|s| session_base(&s.name) == workspace)
        })
        .map(|i| i + 1)
        .unwrap_or(0);

    (choices, preselected)
}

/// Let the user pick which session to enter.
///
/// Returns None if the user cancels.
async fn pick_session(scope: Scope) -> Result<Option<String>> {
    use std::io::IsTerminal;
    let workspace = derive_session_name()?;

    // A script has nothing to choose with, and the answer `enter` gave before
    // there was a chooser is still the right one.
    if !std::io::stdin().is_terminal() || !std::io::stderr().is_terminal() {
        return Ok(Some(workspace));
    }

    let sessions = get_session_list().await?;

    // Skipping the chooser is the point of `--pwd`: with nothing but the
    // canonical session in the workspace there is nothing to choose between.
    if scope == Scope::Pwd {
        let mine: Vec<_> = sessions
            .iter()
            .filter(|s| session_base(&s.name) == workspace)
            .collect();
        if mine.is_empty() || (mine.len() == 1 && mine[0].name == workspace) {
            return Ok(Some(workspace));
        }
    }

    let current = std::env::var("TRIP_SESSION").ok();
    let (choices, preselected) =
        session_choices(&sessions, &workspace, scope, current.as_deref());

    match scope {
        Scope::Pwd => eprintln!(
            "sessions for '{}':  \x1b[2m↑/↓ + enter · 1-9 · q cancels\x1b[0m",
            workspace
        ),
        Scope::All => eprintln!("sessions:  \x1b[2m↑/↓ + enter · 1-9 · q cancels\x1b[0m"),
    }

    let mut chooser = chooser::Chooser::new(
        chooser_rows(&choices),
        preselected,
        chooser_viewport(),
        None,
    );
    match run_chooser(&mut chooser) {
        Some(chooser::Outcome::Pick(i)) => Ok(Some(choices[i].0.clone())),
        // No detach key is supplied here, so `Detach` cannot arrive; a
        // standalone chooser has no session to detach from.
        _ => {
            eprintln!("cancelled");
            Ok(None)
        }
    }
}

/// Lay the choices out in columns. The row carries no number: the chooser
/// numbers what it paints, which is the only numbering a digit can honour once
/// the list scrolls.
pub(crate) fn chooser_rows(choices: &[(String, String, String)]) -> Vec<String> {
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
    choices
        .iter()
        .map(|(name, cmd, tag)| {
            format!("{:<nw$}  {:<cw$}  {}", name, cmd, tag)
                .trim_end()
                .to_string()
        })
        .collect()
}

pub async fn enter(name: Option<String>, all: bool, command: Option<Vec<String>>) -> Result<()> {
    let name = match name {
        Some(n) => n,
        None => {
            // --all widens the picker past this workspace; clap already
            // rejects it alongside an explicit name.
            let scope = if all { Scope::All } else { Scope::Pwd };
            match pick_session(scope).await? {
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

    // No session list is needed here. Whether the session exists is a question
    // only the attach can answer without being stale, and terminals no longer
    // contend for one: several can hold the same session at once.
    let missing = format!("session '{}' not found", name);

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
        let (c, _) = session_choices(&sessions, "acme/webapp", Scope::Pwd, None);
        assert_eq!(
            names(&c),
            vec!["acme/webapp.1", "acme/webapp", "acme/webapp.2"]
        );
    }

    #[test]
    fn the_create_row_offers_the_canonical_session_when_it_is_missing() {
        let sessions = vec![session("acme/webapp.2", false)];
        let (c, _) = session_choices(&sessions, "acme/webapp", Scope::Pwd, None);
        assert_eq!(names(&c), vec!["acme/webapp", "acme/webapp.2"]);
        assert_eq!(c[0].2, "(new session)");
    }

    #[test]
    fn the_create_row_offers_the_next_number_when_the_canonical_exists() {
        let sessions = vec![
            session("acme/webapp", false),
            session("acme/webapp.1", false),
            session("acme/webapp.2", false),
        ];
        let (c, _) = session_choices(&sessions, "acme/webapp", Scope::Pwd, None);
        assert_eq!(c[0].0, "acme/webapp.3");
        assert_eq!(c[0].2, "(new session)");
    }

    #[test]
    fn all_choices_put_the_current_workspace_first() {
        // Rewritten: the wide list used to be strictly alphabetical. It leads
        // with the workspace you launched from, so Enter lands where it landed
        // before there was a wide list at all.
        let sessions = vec![
            session("other/api.2", false),
            session("acme/webapp.10", false),
            session("other/api", false),
            session("acme/webapp", false),
            session("acme/webapp.2", false),
        ];
        let (c, _) = session_choices(&sessions, "other/api", Scope::All, None);
        assert_eq!(
            names(&c),
            vec![
                "other/api.1",
                "other/api",
                "other/api.2",
                "acme/webapp",
                "acme/webapp.2",
                "acme/webapp.10",
            ]
        );
    }

    #[test]
    fn all_choices_offer_a_new_session_for_the_current_workspace() {
        // Rewritten: the wide list used to have no create affordance, on the
        // grounds that no single workspace owned it. The launch directory
        // names one, and without the row a first tab in a fresh repo has
        // nothing to pick.
        let sessions = vec![session("other/api", false)];
        let (c, pre) = session_choices(&sessions, "acme/webapp", Scope::All, None);
        assert_eq!(names(&c), vec!["acme/webapp", "other/api"]);
        assert_eq!(c[0].2, "(new session)");
        assert_eq!(pre, 0, "nothing of ours to select, so the create row");
    }

    #[test]
    fn only_the_current_workspace_gets_a_create_row() {
        let sessions = vec![session("acme/webapp", false), session("other/api", false)];
        let (c, _) = session_choices(&sessions, "acme/webapp", Scope::All, None);
        assert_eq!(
            c.iter().filter(|(_, _, tag)| tag == "(new session)").count(),
            1
        );
    }

    #[test]
    fn the_canonical_session_is_preselected() {
        let sessions = vec![
            session("acme/webapp", false),
            session("acme/webapp.1", false),
        ];
        let (c, pre) = session_choices(&sessions, "acme/webapp", Scope::All, None);
        assert_eq!(c[pre].0, "acme/webapp");
        assert_eq!(pre, 1, "directly below the create row");
    }

    #[test]
    fn a_workspace_whose_canonical_session_died_preselects_the_survivor() {
        // `trip kill trip` with `trip.1` still alive. Nothing canonical to
        // select, and the workspace is not empty either.
        let sessions = vec![
            session("acme/webapp.1", false),
            session("acme/webapp.2", false),
        ];
        let (c, pre) = session_choices(&sessions, "acme/webapp", Scope::All, None);
        assert_eq!(c[0].0, "acme/webapp", "row 1 offers the canonical back");
        assert_eq!(c[pre].0, "acme/webapp.1");
        assert_eq!(pre, 1, "still directly below the create row");
    }

    #[test]
    fn the_create_row_always_sits_directly_above_the_selection() {
        // The invariant the Up-then-Enter gesture rests on, in every state a
        // workspace can be in.
        let states: Vec<Vec<SessionInfo>> = vec![
            vec![],
            vec![session("w", false)],
            vec![session("w", false), session("w.1", false)],
            vec![session("w.1", false), session("w.2", false)],
            vec![session("other", false)],
        ];
        for sessions in states {
            let (c, pre) = session_choices(&sessions, "w", Scope::All, None);
            assert_eq!(c[0].2, "(new session)");
            assert!(pre == 0 || pre == 1, "selection at {} for {:?}", pre, names(&c));
        }
    }

    #[test]
    fn an_empty_daemon_still_offers_the_workspace() {
        let (c, pre) = session_choices(&[], "acme/webapp", Scope::All, None);
        assert_eq!(names(&c), vec!["acme/webapp"]);
        assert_eq!(pre, 0);
    }

    #[test]
    fn tags_mark_current_and_attached() {
        let sessions = vec![session("acme/webapp", true), session("other/api", false)];
        let (c, _) = session_choices(&sessions, "other/api", Scope::All, Some("other/api"));
        assert_eq!(c[1].2, "(current)");
        assert_eq!(c[2].2, "(attached)");
    }

    #[test]
    fn rows_are_laid_out_without_their_own_numbering() {
        // The chooser numbers what it paints; a number baked in here would be
        // wrong the moment the list scrolls.
        let rows = chooser_rows(&[
            ("a".to_string(), "zsh".to_string(), "(current)".to_string()),
            ("bbb".to_string(), "vim".to_string(), String::new()),
        ]);
        assert_eq!(rows[0], "a    zsh  (current)");
        assert_eq!(rows[1], "bbb  vim");
    }
}
