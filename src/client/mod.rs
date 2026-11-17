pub mod attach;
pub mod chooser;
pub mod launch;
pub mod upgrade;
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
    Ok(running_sessions().await?.unwrap_or_default())
}

/// The running daemon's sessions, or None when no daemon answers the socket.
async fn running_sessions() -> Result<Option<Vec<crate::daemon::protocol::SessionInfo>>> {
    let Ok(stream) = launch::try_connect().await else {
        return Ok(None);
    };
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);
    write_control(&mut writer, &Request::ListSessions).await?;
    match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::SessionList { sessions } => Ok(Some(sessions)),
                _ => Ok(Some(Vec::new())),
            }
        }
        _ => Ok(Some(Vec::new())),
    }
}

/// The running daemon's version, as far as the client can tell.
#[derive(Debug, PartialEq)]
pub enum DaemonVersion {
    Recorded(String),
    /// No record whose daemon still runs: a daemon from before releases,
    /// which writes none, or one whose record was left by a daemon since gone.
    BeforeReleases,
}

/// What `~/.trip/daemon.json` says about the running daemon. It says nothing
/// about whether one runs; only the socket answers that.
pub fn daemon_version() -> DaemonVersion {
    match crate::common::read_daemon_record() {
        Some(record) if pid_running(record.pid) => DaemonVersion::Recorded(record.version),
        _ => DaemonVersion::BeforeReleases,
    }
}

fn pid_running(pid: u32) -> bool {
    use nix::errno::Errno;
    use nix::sys::signal::kill;
    use nix::unistd::Pid;
    // Signal 0 only asks. Pid 0 and negative pids name process groups, which
    // a record never means.
    match i32::try_from(pid) {
        Ok(pid) if pid > 0 => matches!(kill(Pid::from_raw(pid), None), Ok(()) | Err(Errno::EPERM)),
        _ => false,
    }
}

/// A release version's numbers. A `-dev` build, or anything else that is not
/// `X.Y.Z`, has none, so it is never older or newer than a release.
pub fn release_numbers(version: &str) -> Option<[u64; 3]> {
    let mut parts = version.split('.').map(|part| part.parse::<u64>().ok());
    let numbers = [parts.next()??, parts.next()??, parts.next()??];
    parts.next().is_none().then_some(numbers)
}

/// Whether the daemon runs an older trip than `version`. A daemon from before
/// releases is older than anything.
pub fn daemon_is_older(daemon: &DaemonVersion, version: &str) -> bool {
    match daemon {
        DaemonVersion::BeforeReleases => true,
        DaemonVersion::Recorded(running) => {
            matches!((release_numbers(running), release_numbers(version)), (Some(a), Some(b)) if a < b)
        }
    }
}

/// The line that says the daemon is older than `version` and how it moves.
/// `sessions` is None when the daemon would not say how many it holds.
pub fn older_daemon_line(daemon: &DaemonVersion, version: &str, sessions: Option<usize>) -> String {
    let runs = match daemon {
        DaemonVersion::Recorded(running) => format!("the daemon still runs {}", running),
        DaemonVersion::BeforeReleases => "the daemon still runs a trip from before releases".into(),
    };
    let holding = match sessions {
        Some(1) => " with 1 session".to_string(),
        Some(n) => format!(" with {} sessions", n),
        None => String::new(),
    };
    format!(
        "{}{}; it moves to {} when its last session ends or you run trip shutdown --yes",
        runs, holding, version
    )
}

/// The older-daemon line when a running daemon is older than `version`,
/// asking it how many sessions it holds. No daemon, no line.
async fn older_daemon_note(version: &str) -> Option<String> {
    let daemon = daemon_version();
    if !daemon_is_older(&daemon, version) {
        return None;
    }
    let sessions = match running_sessions().await {
        Ok(None) => return None,
        Ok(Some(sessions)) => Some(sessions.len()),
        Err(_) => None,
    };
    Some(older_daemon_line(&daemon, version, sessions))
}

pub async fn warn_if_daemon_older(version: &str) {
    if let Some(line) = older_daemon_note(version).await {
        eprintln!("{}", line);
    }
}

/// Explain a request that failed because the daemon is older than this
/// client. An older daemon closes the connection on a request it cannot
/// decode, which every request reports as `unexpected frame`, and a reply
/// this client cannot decode fails in `serde_json`. Any other failure, or
/// one from a daemon that is not older, stays as it was.
pub async fn explain_daemon_failure(error: anyhow::Error) -> anyhow::Error {
    let undecoded = error.to_string() == "unexpected frame"
        || error.chain().any(|cause| {
            cause.is::<serde_json::Error>()
                || cause.downcast_ref::<std::io::Error>().is_some_and(|e| {
                    matches!(
                        e.kind(),
                        std::io::ErrorKind::UnexpectedEof
                            | std::io::ErrorKind::ConnectionReset
                            | std::io::ErrorKind::BrokenPipe
                    )
                })
        });
    if !undecoded {
        return error;
    }
    match older_daemon_note(crate::cli::VERSION).await {
        Some(line) => anyhow::anyhow!("the daemon could not answer this request: {}", line),
        None => error,
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

/// The chooser's window: rows it may paint — the terminal, less the hint
/// line, the two truncation markers, and one spare so the list never scrolls
/// — and the columns it may fill.
pub(crate) fn chooser_geometry() -> (usize, usize) {
    let (cols, rows) = terminal_size();
    ((rows as usize).saturating_sub(4).max(3), cols as usize)
}

pub(crate) fn terminal_size() -> (u16, u16) {
    use nix::libc;
    use std::os::fd::AsRawFd;
    // Whichever standard stream is still a terminal answers. The chooser
    // renders to stderr precisely so that stdout may be a pipe, and sizing
    // from the pipe would hand back the fallback on the one path that
    // advertises working without a tty on stdout.
    for fd in [
        std::io::stdout().as_raw_fd(),
        std::io::stderr().as_raw_fd(),
        std::io::stdin().as_raw_fd(),
    ] {
        let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
        unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
        if ws.ws_col != 0 {
            return (ws.ws_col, ws.ws_row);
        }
    }
    (80, 24)
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
    // Most recently opened first — recency of *entering*, not of output, so
    // the list works like a task switcher: the sessions you are bouncing
    // between hold the low digits, wherever their workspaces are. Ties (from
    // daemons predating the stamp) fall back to workspace order.
    group.sort_by(|a, b| {
        b.last_opened.cmp(&a.last_opened).then_with(|| {
            (session_base(&a.name), group_order(&a.name))
                .cmp(&(session_base(&b.name), group_order(&b.name)))
        })
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

    // The canonical session, failing that the workspace's most recently
    // opened one, failing that the create row. The middle clause is `trip`
    // killed while `trip.1` lives: nothing canonical to select, and the
    // workspace not empty either.
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
/// Returns None if the user cancels. Picking the numbered create row creates
/// the session *here*, so the returned name always exists: the name was
/// computed when the list was drawn, and another terminal can take it before
/// Enter lands — `enter`'s create-or-attach would then silently join that
/// terminal's session, and "new session" means new. On a collision the number
/// is recomputed and creation retried, the same shape as the daemon-side
/// `allocate` that covers the keystroke path.
async fn pick_session(scope: Scope, command: &Option<Vec<String>>) -> Result<Option<String>> {
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
    let (choices, preselected) = session_choices(&sessions, &workspace, scope, current.as_deref());

    match scope {
        Scope::Pwd => eprintln!(
            "sessions for '{}':  \x1b[2m↑/↓ + enter · 0-9 · q cancels\x1b[0m",
            workspace
        ),
        Scope::All => eprintln!("sessions:  \x1b[2m↑/↓ + enter · 0-9 · q cancels\x1b[0m"),
    }

    let mut chooser = chooser::Chooser::new(
        chooser_rows(&choices),
        preselected,
        chooser_geometry(),
        None,
    )
    .with_zero_row();
    match run_chooser(&mut chooser) {
        Some(chooser::Outcome::Pick(i)) => {
            let mut name = choices[i].0.clone();
            if i == 0 && name != workspace {
                // The numbered create row. The canonical one (name ==
                // workspace) keeps create-or-attach semantics: joining the
                // session somebody else just made is the right answer there.
                for _ in 0..5 {
                    let exists = format!("session '{}' already exists", name);
                    match create_session(name.clone(), command.clone()).await {
                        Ok(()) => return Ok(Some(name)),
                        Err(e) if e.to_string() == exists => {
                            let sessions = get_session_list().await?;
                            name = next_available_name(&sessions, &workspace);
                        }
                        Err(e) => return Err(e),
                    }
                }
                anyhow::bail!("no free session number for '{}'", workspace);
            }
            Ok(Some(name))
        }
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

pub async fn enter(name: Option<String>, scope: Scope, command: Option<Vec<String>>) -> Result<()> {
    let name = match name {
        Some(n) => n,
        None => match pick_session(scope, &command).await? {
            Some(n) => n,
            None => return Ok(()),
        },
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
    // it and attach. So is a session whose command has exited: the attach asks
    // for a live one, and the daemon clears the corpse and says it is missing.
    // Deciding that from the list instead would decide it on an older round
    // trip, and that gap is what made `enter` report "not found" instead of
    // honouring its create-or-attach contract.
    //
    // Matched by message because the daemon reports errors as strings; keep in
    // step with the Attach handler in daemon/mod.rs. A drift in wording gives
    // back the old error rather than misfiring.
    match attach::attach(name.clone(), true).await {
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
            // Whatever is there now is what this create asked for, even if
            // its command has already exited, so show it.
            attach::attach(name, false).await
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
    attach::attach(name, false).await?;
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

pub async fn list_sessions(scope: Scope, attached_only: bool) -> Result<()> {
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
                    let total = sessions.len();
                    print_sessions(sessions, scope, attached_only)?;
                    let daemon = daemon_version();
                    if daemon_is_older(&daemon, crate::cli::VERSION) {
                        eprintln!(
                            "{}",
                            older_daemon_line(&daemon, crate::cli::VERSION, Some(total))
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

fn print_sessions(
    sessions: Vec<crate::daemon::protocol::SessionInfo>,
    scope: Scope,
    attached_only: bool,
) -> Result<()> {
    // Headers earn their place only across workspaces.
    let grouped = scope == Scope::All;
    let workspace = match scope {
        Scope::Pwd => Some(derive_session_name()?),
        Scope::All => None,
    };
    let mut sessions: Vec<_> = sessions
        .into_iter()
        .filter(|s| !attached_only || s.attached)
        .filter(|s| {
            workspace
                .as_deref()
                .is_none_or(|base| session_base(&s.name) == base)
        })
        .collect();
    if sessions.is_empty() {
        match &workspace {
            Some(base) => {
                println!("no sessions for '{}' (try: trip ls)", base)
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
    Ok(())
}

pub const TERMINAL_RESET: &[u8] =
    b"\x1b[?1049l\x1b[?1000l\x1b[?1002l\x1b[?1003l\x1b[?1006l\x1b[?2004l\x1b[>4;0m\x1b[=0;1u\x1b[?25h\x1b[H\x1b[2J";

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
        opened(name, attached, 0)
    }

    /// A session last opened at monotonic stamp `at` — bigger is newer.
    fn opened(name: &str, attached: bool, at: u64) -> SessionInfo {
        SessionInfo {
            name: name.to_string(),
            command: "bash".to_string(),
            pid: 1,
            created_at: 0,
            last_opened: at,
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
    fn all_choices_are_ordered_by_when_they_were_last_opened() {
        // Rewritten twice: alphabetical, then current-workspace-first, now
        // recency of *entering* — the list works like a task switcher, so the
        // sessions you are bouncing between hold the low digits, wherever
        // their workspaces live. Not recency of output: a chatty background
        // build must not shuffle the list.
        let sessions = vec![
            opened("other/api.2", false, 3),
            opened("acme/webapp.10", false, 9),
            opened("other/api", false, 5),
            opened("acme/webapp", false, 1),
        ];
        let (c, _) = session_choices(&sessions, "other/api", Scope::All, None);
        assert_eq!(
            names(&c),
            vec![
                "other/api.1", // the create row
                "acme/webapp.10",
                "other/api",
                "other/api.2",
                "acme/webapp",
            ]
        );
    }

    #[test]
    fn sessions_a_daemon_never_stamped_keep_workspace_order() {
        // last_opened defaults to 0 against an older daemon; the tie falls
        // back to grouping instead of shuffling arbitrarily.
        let sessions = vec![
            session("other/api", false),
            session("acme/webapp.2", false),
            session("acme/webapp", false),
        ];
        let (c, _) = session_choices(&sessions, "acme/webapp", Scope::All, None);
        assert_eq!(
            names(&c),
            vec!["acme/webapp.1", "acme/webapp", "acme/webapp.2", "other/api"]
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
            c.iter()
                .filter(|(_, _, tag)| tag == "(new session)")
                .count(),
            1
        );
    }

    #[test]
    fn the_canonical_session_is_preselected_wherever_recency_put_it() {
        let sessions = vec![
            opened("acme/webapp", false, 1),
            opened("acme/webapp.1", false, 7),
        ];
        let (c, pre) = session_choices(&sessions, "acme/webapp", Scope::All, None);
        assert_eq!(c[pre].0, "acme/webapp");
        assert_eq!(pre, 2, "the fresher numbered session outranks it");
    }

    #[test]
    fn a_workspace_whose_canonical_session_died_preselects_its_freshest() {
        // `trip kill trip` with `trip.1` still alive. Nothing canonical to
        // select, and the workspace is not empty either.
        let sessions = vec![
            opened("acme/webapp.1", false, 2),
            opened("acme/webapp.2", false, 8),
            opened("other/api", false, 9),
        ];
        let (c, pre) = session_choices(&sessions, "acme/webapp", Scope::All, None);
        assert_eq!(
            c[0].0, "acme/webapp",
            "the create row offers the canonical back"
        );
        assert_eq!(c[pre].0, "acme/webapp.2", "the workspace's most recent");
    }

    #[test]
    fn the_create_row_is_always_row_zero() {
        // Recency reorders everything below it; the create row is an action
        // and holds position (and the digit 0) in every state.
        let states: Vec<Vec<SessionInfo>> = vec![
            vec![],
            vec![opened("w", false, 5)],
            vec![opened("w", false, 1), opened("w.1", false, 9)],
            vec![opened("w.1", false, 4), opened("w.2", false, 2)],
            vec![opened("other", false, 3)],
        ];
        for sessions in states {
            let (c, pre) = session_choices(&sessions, "w", Scope::All, None);
            assert_eq!(c[0].2, "(new session)");
            assert!(pre < c.len(), "selection in range for {:?}", names(&c));
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
        let tag = |name: &str| c.iter().find(|(n, _, _)| n == name).unwrap().2.clone();
        assert_eq!(tag("other/api"), "(current)");
        assert_eq!(tag("acme/webapp"), "(attached)");
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

#[cfg(test)]
mod daemon_version_tests {
    use super::{daemon_is_older, older_daemon_line, pid_running, release_numbers, DaemonVersion};

    fn recorded(version: &str) -> DaemonVersion {
        DaemonVersion::Recorded(version.into())
    }

    #[test]
    fn versions_compare_by_number_not_by_text() {
        assert_eq!(release_numbers("0.10.2"), Some([0, 10, 2]));
        assert!(daemon_is_older(&recorded("0.9.0"), "0.10.0"));
        assert!(daemon_is_older(&recorded("0.2.9"), "0.3.0"));
        assert!(daemon_is_older(&recorded("1.2.3"), "1.2.4"));
        assert!(!daemon_is_older(&recorded("0.3.0"), "0.3.0"));
        assert!(!daemon_is_older(&recorded("0.10.0"), "0.9.0"));
    }

    #[test]
    fn a_dev_version_is_never_older_or_newer_than_a_release() {
        assert_eq!(release_numbers("0.2.0-dev"), None);
        assert!(!daemon_is_older(&recorded("0.1.0-dev"), "0.3.0"));
        assert!(!daemon_is_older(&recorded("0.1.0"), "0.3.0-dev"));
        assert!(!daemon_is_older(&recorded("0.1.0-dev"), "0.3.0-dev"));
    }

    #[test]
    fn anything_but_three_numbers_is_not_a_release() {
        for version in ["", "0.2", "0.2.0.1", "v0.2.0", "0.2.x", "0..2"] {
            assert_eq!(release_numbers(version), None, "{:?}", version);
            assert!(
                !daemon_is_older(&recorded(version), "9.9.9"),
                "{:?}",
                version
            );
        }
    }

    #[test]
    fn a_daemon_from_before_releases_is_older_than_anything() {
        assert!(daemon_is_older(&DaemonVersion::BeforeReleases, "0.2.0"));
        assert!(daemon_is_older(&DaemonVersion::BeforeReleases, "0.2.0-dev"));
    }

    #[test]
    fn the_line_names_both_versions_and_the_sessions() {
        assert_eq!(
            older_daemon_line(&recorded("0.2.0"), "0.3.0", Some(4)),
            "the daemon still runs 0.2.0 with 4 sessions; it moves to 0.3.0 \
             when its last session ends or you run trip shutdown --yes"
        );
        assert_eq!(
            older_daemon_line(&DaemonVersion::BeforeReleases, "0.3.0", Some(1)),
            "the daemon still runs a trip from before releases with 1 session; \
             it moves to 0.3.0 when its last session ends or you run trip shutdown --yes"
        );
        assert_eq!(
            older_daemon_line(&recorded("0.2.0"), "0.3.0", None),
            "the daemon still runs 0.2.0; it moves to 0.3.0 \
             when its last session ends or you run trip shutdown --yes"
        );
    }

    #[test]
    fn only_a_running_pid_vouches_for_a_record() {
        assert!(pid_running(std::process::id()));
        let mut child = std::process::Command::new("true").spawn().unwrap();
        let exited = child.id();
        child.wait().unwrap();
        assert!(!pid_running(exited));
        // 0 and anything past i32::MAX would signal a process group.
        assert!(!pid_running(0));
        assert!(!pid_running(u32::MAX));
    }
}
