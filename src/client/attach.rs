use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};

use anyhow::Result;
use nix::sys::termios::{self, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg};
use tokio::io::{BufReader, BufWriter};
use tokio::signal::unix::{signal, SignalKind};

use crate::daemon::protocol::{
    read_frame, write_control, write_frame, Frame, Request, Response, FRAME_DATA, FRAME_RESIZE,
};

use super::chooser::{Chooser, Outcome};
use super::launch;
use super::terminal_size;

struct RawModeGuard {
    original: termios::Termios,
    fd: i32,
}

impl RawModeGuard {
    fn enter() -> Option<Self> {
        let fd = std::io::stdin().as_raw_fd();
        let borrowed = unsafe { BorrowedFd::borrow_raw(fd) };
        let original = match termios::tcgetattr(borrowed) {
            Ok(t) => t,
            Err(_) => return None,
        };

        let mut raw = original.clone();
        raw.input_flags &= !(InputFlags::BRKINT
            | InputFlags::ICRNL
            | InputFlags::INPCK
            | InputFlags::ISTRIP
            | InputFlags::IXON);
        raw.output_flags &= !OutputFlags::OPOST;
        raw.control_flags |= ControlFlags::CS8;
        raw.local_flags &=
            !(LocalFlags::ECHO | LocalFlags::ICANON | LocalFlags::IEXTEN | LocalFlags::ISIG);

        if termios::tcsetattr(borrowed, SetArg::TCSAFLUSH, &raw).is_err() {
            return None;
        }

        Some(RawModeGuard { original, fd })
    }
}

impl Drop for RawModeGuard {
    fn drop(&mut self) {
        let borrowed = unsafe { BorrowedFd::borrow_raw(self.fd) };
        let _ = termios::tcsetattr(borrowed, SetArg::TCSANOW, &self.original);
    }
}

/// Returns the terminal to a sane state on *every* exit path.
///
/// The attach loop propagates errors with `?` — a daemon crash or broken pipe
/// returns straight out of `attach()` — which used to skip the explicit
/// cleanup entirely, leaving mouse tracking and the alternate screen enabled
/// and the pushed title unbalanced. Terminals cap the title stack, so repeated
/// broken attaches would push the real pre-attach title off the bottom.
///
/// Declared after `RawModeGuard` so it drops first: escape codes go out while
/// the terminal is still raw, then termios is restored.
struct TerminalCleanup;

impl Drop for TerminalCleanup {
    fn drop(&mut self) {
        let mut out = std::io::stdout();
        // Mouse tracking, alternate screen buffer, bracketed paste.
        let _ = out.write_all(super::TERMINAL_RESET);
        // Balance the title push from attach.
        let _ = out.write_all(b"\x1b[23;0t");
        let _ = out.flush();
    }
}

/// The keystroke that detaches the client, dtach/abduco-style. Ctrl-\ by
/// default: ISIG is cleared in raw mode, so it arrives as a plain byte, and
/// unlike Ctrl-Z it carries no job-control meaning worth preserving inside
/// the session.
const DEFAULT_DETACH_KEY: u8 = 0x1c;

/// Parse TRIP_DETACH_KEY: caret notation ("^\\", "^z"), or "none" to disable.
/// Unset means the default; unparseable values also fall back to the default
/// rather than silently disabling the key.
fn detach_key_from_env() -> Option<u8> {
    let value = match std::env::var("TRIP_DETACH_KEY") {
        Ok(v) => v,
        Err(_) => return Some(DEFAULT_DETACH_KEY),
    };
    parse_detach_key(&value)
}

fn parse_detach_key(value: &str) -> Option<u8> {
    let value = value.trim();
    if value.eq_ignore_ascii_case("none") || value.eq_ignore_ascii_case("off") {
        return None;
    }
    if let Some(rest) = value.strip_prefix('^') {
        let mut chars = rest.chars();
        if let (Some(c), None) = (chars.next(), chars.next()) {
            let c = c.to_ascii_uppercase();
            return match c {
                // What terminals send for Ctrl+- (aliased to Ctrl+_).
                '-' => Some(0x1f),
                // Ctrl+? is DEL by convention.
                '?' => Some(0x7f),
                // The & 0x1f trick is only valid in this range; outside it
                // ('^-' would become CR!) fall through to the default.
                '@'..='_' => Some((c as u8) & 0x1f),
                _ => Some(DEFAULT_DETACH_KEY),
            };
        }
    }
    Some(DEFAULT_DETACH_KEY)
}

/// Scans client input for the detach key while tracking bracketed paste, so a
/// pasted blob that happens to contain the byte is forwarded untouched. The
/// paste markers (ESC [ 2 0 0/1 ~) can split across read() chunks, so the
/// match state persists between calls.
struct DetachScanner {
    key: Option<u8>,
    in_paste: bool,
    matched: usize,
    digit: u8,
}

enum Scan {
    /// Forward the whole chunk.
    Forward,
    /// Forward only the bytes before the detach key, then detach.
    Detach(usize),
}

impl DetachScanner {
    fn new(key: Option<u8>) -> Self {
        DetachScanner {
            key,
            in_paste: false,
            matched: 0,
            digit: 0,
        }
    }

    fn scan(&mut self, data: &[u8]) -> Scan {
        let key = match self.key {
            Some(k) => k,
            None => return Scan::Forward,
        };
        for (i, &b) in data.iter().enumerate() {
            const MARKER: [u8; 4] = [0x1b, b'[', b'2', b'0'];
            self.matched = match self.matched {
                m @ 0..=3 if b == MARKER[m] => m + 1,
                4 if b == b'0' || b == b'1' => {
                    self.digit = b;
                    5
                }
                5 if b == b'~' => {
                    self.in_paste = self.digit == b'0';
                    0
                }
                // A failed match can still start a new one.
                _ if b == MARKER[0] => 1,
                _ => 0,
            };
            if !self.in_paste && b == key {
                return Scan::Detach(i);
            }
        }
        Scan::Forward
    }
}

/// Rewrites OSC title sequences on their way to the terminal so every title
/// starts with the workspace.
///
/// trip sets the terminal title once at attach, but anything running in the session
/// (Claude Code, an editor, a long build) sets its own afterwards and the
/// workspace is gone. Those titles are worth keeping — they say what the
/// session is *doing* — so prefix them rather than suppress them.
///
/// Sequences arrive split across reads, so the parser is a state machine that
/// holds a partial sequence between chunks. See `title_affixes` for config.
struct TitlePrefixer {
    /// Literal pieces to join the session's title with. One piece means the
    /// template had no `$TITLE`, so the title is replaced outright. Empty
    /// means titles pass through untouched.
    parts: Vec<String>,
    state: TitleState,
    /// The `ESC ] n ;` seen so far, replayed verbatim if this turns out not to
    /// be a title sequence after all.
    pending: Vec<u8>,
    title: Vec<u8>,
}

#[derive(PartialEq)]
enum TitleState {
    Normal,
    Esc,
    /// After `ESC ]`, reading the numeric parameter.
    Osc,
    /// Inside the title text of an OSC 0/1/2.
    Title,
    /// Saw ESC inside the title — either the ST terminator or a stray escape.
    TitleEsc,
}

/// Titles longer than this are passed through untouched; a runaway sequence
/// should not buffer without bound.
const MAX_TITLE: usize = 512;

impl TitlePrefixer {
    fn new(parts: Vec<String>) -> Self {
        TitlePrefixer {
            parts,
            state: TitleState::Normal,
            pending: Vec::new(),
            title: Vec::new(),
        }
    }

    fn enabled(&self) -> bool {
        self.parts.iter().any(|p| !p.is_empty()) || self.parts.len() > 1
    }

    /// The title shown before anything in the session sets one.
    fn initial(&self) -> String {
        self.parts.join("").trim().to_string()
    }

    /// Swap the prefix without disturbing the parser. A rename can land
    /// between the two halves of a split sequence; rebuilding the parser would
    /// discard the withheld bytes and print the tail of the title as raw
    /// output, bell included.
    fn set_parts(&mut self, parts: Vec<String>) {
        self.parts = parts;
    }

    fn decorate(&self, title: &[u8], out: &mut Vec<u8>) {
        if !self.enabled() {
            out.extend_from_slice(title);
            return;
        }
        // No `$TITLE` in the template: the title is replaced outright.
        if self.parts.len() == 1 {
            out.extend_from_slice(self.parts[0].as_bytes());
            return;
        }
        let text = String::from_utf8_lossy(title);
        let head = &self.parts[0];
        let tail = &self.parts[self.parts.len() - 1];
        // Leave an already-wrapped title alone, so reattaching or a second
        // client cannot nest the wrapping. An empty piece matches trivially.
        let wrapped = !text.is_empty()
            && (head.is_empty() || text.starts_with(head.as_str()))
            && (tail.is_empty() || text.ends_with(tail.as_str()));
        if wrapped {
            out.extend_from_slice(title);
            return;
        }
        for (i, part) in self.parts.iter().enumerate() {
            if i > 0 {
                out.extend_from_slice(title);
            }
            out.extend_from_slice(part.as_bytes());
        }
    }

    fn process(&mut self, data: &[u8]) -> Vec<u8> {
        if !self.enabled() {
            return data.to_vec();
        }
        let mut out = Vec::with_capacity(data.len() + 32);
        for &b in data {
            match self.state {
                TitleState::Normal => {
                    if b == 0x1b {
                        self.state = TitleState::Esc;
                        self.pending.clear();
                        self.pending.push(b);
                    } else {
                        out.push(b);
                    }
                }
                TitleState::Esc => {
                    self.pending.push(b);
                    if b == b']' {
                        self.state = TitleState::Osc;
                    } else {
                        out.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.state = TitleState::Normal;
                    }
                }
                TitleState::Osc => {
                    self.pending.push(b);
                    // Only 0 (icon+window), 1 (icon) and 2 (window) are titles.
                    let head = &self.pending[2..];
                    if b == b';' {
                        if matches!(head, b"0;" | b"1;" | b"2;") {
                            self.state = TitleState::Title;
                            self.title.clear();
                        } else {
                            out.extend_from_slice(&self.pending);
                            self.pending.clear();
                            self.state = TitleState::Normal;
                        }
                    } else if !b.is_ascii_digit() || head.len() > 2 {
                        out.extend_from_slice(&self.pending);
                        self.pending.clear();
                        self.state = TitleState::Normal;
                    }
                }
                TitleState::Title => {
                    if b == 0x07 {
                        out.extend_from_slice(&self.pending);
                        let title = std::mem::take(&mut self.title);
                        self.decorate(&title, &mut out);
                        out.push(0x07);
                        self.pending.clear();
                        self.state = TitleState::Normal;
                    } else if b == 0x1b {
                        self.state = TitleState::TitleEsc;
                    } else if self.title.len() >= MAX_TITLE {
                        out.extend_from_slice(&self.pending);
                        out.extend_from_slice(&self.title);
                        out.push(b);
                        self.pending.clear();
                        self.title.clear();
                        self.state = TitleState::Normal;
                    } else {
                        self.title.push(b);
                    }
                }
                TitleState::TitleEsc => {
                    if b == b'\\' {
                        out.extend_from_slice(&self.pending);
                        let title = std::mem::take(&mut self.title);
                        self.decorate(&title, &mut out);
                        out.extend_from_slice(b"\x1b\\");
                        self.pending.clear();
                        self.state = TitleState::Normal;
                    } else {
                        // Not a terminator; the ESC was part of the title.
                        self.title.push(0x1b);
                        self.title.push(b);
                        self.state = TitleState::Title;
                    }
                }
            }
        }
        out
    }
}

/// Marker standing in for the session's own title while the template is
/// expanded. Control characters, so it cannot collide with anything a real
/// template produces.
const TITLE_MARK: &str = "\u{1}TRIP_TITLE\u{1}";

/// Expand `TRIP_TITLE` once and split it around the title, giving the literal
/// pieces to join the session's title with.
///
/// `TRIP_TITLE` is a shell string, expanded with `TRIP_WORKSPACE`,
/// `TRIP_SESSION` and `TITLE` in the environment, so the whole of shell
/// parameter expansion is available and there is nothing new to learn.
/// `$TITLE` is the one placeholder — named rather than positional, to match
/// its neighbours in the same string:
///
///   `${TRIP_WORKSPACE##*/} $TITLE`     webapp Deliberating   (the default)
///   `$TITLE @${TRIP_WORKSPACE##*/}`    Deliberating @webapp
///   `[$TITLE] ~${TRIP_WORKSPACE##*/}`  [Deliberating] ~webapp
///   `${TRIP_WORKSPACE##*/}`            webapp                (title dropped)
///
/// Prefer putting `$TITLE` first where the terminal truncates from the left and
/// keeps the tail — iTerm does — because whatever precedes it disappears.
///
/// Expanding once rather than per title keeps this to a single `sh` for the
/// lifetime of the attach; a title changes far too often to fork for each one.
/// The consequence is that `$TITLE` is substituted positionally: a bare
/// `$TITLE` works, but a construct that *transforms* it — `${TITLE:-idle}`,
/// `$(printf %s "$TITLE" | tr a-z A-Z)` — sees the marker rather than the real
/// title and will not behave as written.
fn title_parts(session: &str) -> Vec<String> {
    const DEFAULT: &str = "${TRIP_WORKSPACE##*/} $TITLE";
    let template = std::env::var("TRIP_TITLE").unwrap_or_else(|_| DEFAULT.to_string());
    expand_title(&template, session)
}

/// The expansion itself, separate from reading the environment so it can be
/// tested directly — it shells out, which is exactly why it needs covering.
fn expand_title(template: &str, session: &str) -> Vec<String> {
    if template.is_empty() {
        return Vec::new();
    }
    let workspace = super::session_base(session);

    let expanded = std::process::Command::new("sh")
        .arg("-c")
        // The inner escaped quotes are load-bearing: without them the value is
        // field-split before eval sees it and printf reuses %s across the
        // pieces, silently collapsing every space in the template.
        .arg(r#"eval "printf %s \"$TRIP_TITLE\"""#)
        .env("TRIP_TITLE", template)
        .env("TRIP_WORKSPACE", workspace)
        .env("TRIP_SESSION", session)
        .env("TITLE", TITLE_MARK)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        // A broken template should not leave the title unset.
        // Degrade to what the default template would have produced: the last
        // workspace segment, not the whole path.
        .unwrap_or_else(|| {
            let leaf = workspace.rsplit('/').next().unwrap_or(workspace);
            format!("{} {}", leaf, TITLE_MARK)
        });

    expanded.split(TITLE_MARK).map(|p| p.to_string()).collect()
}

/// Caret notation for a control byte, so the hint can name whatever key the
/// user actually bound.
fn caret(key: u8) -> String {
    match key {
        0x7f => "^?".to_string(),
        b if b < 0x20 => format!("^{}", (b | 0x40) as char),
        b => (b as char).to_string(),
    }
}

/// How the chooser ended, from the attach loop's point of view.
enum ChooserExit {
    /// A switch was requested on the socket; the daemon re-renders next.
    Switched,
    /// The detach key again.
    Detached,
    /// The daemon went away while the chooser was up.
    Gone,
}

/// Run the chooser over an attached session, driven by the client's existing
/// stdin channel rather than the fd.
///
/// Session output keeps arriving while this runs and is dropped on the floor.
/// The socket still has to be drained or the daemon blocks writing to it, but
/// nothing it sends is worth keeping: every way out of here ends in a full
/// re-render.
async fn run_attached_chooser(
    reader: &mut BufReader<tokio::net::unix::OwnedReadHalf>,
    writer: &mut BufWriter<tokio::net::unix::OwnedWriteHalf>,
    stdin_rx: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    sigwinch: &mut tokio::signal::unix::Signal,
    current: &str,
    detach_key: Option<u8>,
) -> Result<ChooserExit> {
    let sessions = super::get_session_list().await?;
    // The workspace is the session's, not the client's. An attach client's cwd
    // is wherever its terminal was launched, which says nothing about where
    // the session has since been.
    let workspace = super::session_base(current).to_string();
    let (choices, preselected) =
        super::session_choices(&sessions, &workspace, super::Scope::All, Some(current));

    let mut chooser = Chooser::new(
        super::chooser_rows(&choices),
        preselected,
        super::chooser_viewport(),
        detach_key,
    );

    let hint = match detach_key {
        Some(key) => format!(
            "sessions:  \x1b[2m↑/↓ + enter · 1-9 · esc back · {} detach\x1b[0m",
            caret(key)
        ),
        None => "sessions:  \x1b[2m↑/↓ + enter · 1-9 · esc back\x1b[0m".to_string(),
    };

    let mut stdout = std::io::stdout();
    let mut repaint = |chooser: &mut Chooser, full: bool| -> Result<()> {
        if full {
            stdout.write_all(b"\x1b[?25l\x1b[2J\x1b[H")?;
            stdout.write_all(hint.as_bytes())?;
            stdout.write_all(b"\r\n")?;
        }
        stdout.write_all(&chooser.render())?;
        stdout.flush()?;
        Ok(())
    };
    repaint(&mut chooser, true)?;

    let outcome = loop {
        let decided = tokio::select! {
            frame = read_frame(reader) => {
                match frame? {
                    Some(_) => None,
                    None => return Ok(ChooserExit::Gone),
                }
            }

            data = stdin_rx.recv() => {
                match data {
                    Some(data) => chooser.feed(&data),
                    None => return Ok(ChooserExit::Gone),
                }
            }

            _ = sigwinch.recv() => {
                let (cols, rows) = terminal_size();
                let mut payload = Vec::with_capacity(4);
                payload.extend_from_slice(&cols.to_be_bytes());
                payload.extend_from_slice(&rows.to_be_bytes());
                write_frame(writer, FRAME_RESIZE, &payload).await?;
                chooser.resize(super::chooser_viewport());
                repaint(&mut chooser, true)?;
                None
            }

            // A lone Esc is only knowable by the silence after it.
            _ = tokio::time::sleep(std::time::Duration::from_millis(25)),
                if chooser.pending_escape() => { chooser.tick() }
        };

        match decided {
            Some(outcome) => break outcome,
            None => repaint(&mut chooser, false)?,
        }
    };

    // Give the cursor back before anything repaints over the list.
    stdout.write_all(b"\x1b[?25h")?;
    stdout.flush()?;

    let (to, allocate) = match outcome {
        Outcome::Detach => return Ok(ChooserExit::Detached),
        // Cancel is a switch to the session we are already on: the daemon's
        // re-render is exactly the repaint a cancel needs, and it keeps one
        // code path behind both.
        Outcome::Cancel => (current.to_string(), false),
        Outcome::Pick(i) => {
            // The create row is always row 1. Naming a number the chooser
            // painted would race another terminal for it, so let the daemon
            // allocate one; the canonical session is named outright, because
            // joining the one somebody else just made is the right answer.
            let allocate = i == 0 && choices[i].0 != workspace;
            (choices[i].0.clone(), allocate)
        }
    };

    write_control(
        writer,
        &Request::SwitchTo {
            to,
            allocate,
            command: None,
            cwd: std::env::current_dir()
                .map(|p| p.to_string_lossy().into_owned())
                .unwrap_or_default(),
            env: super::terminal_env(),
        },
    )
    .await?;
    Ok(ChooserExit::Switched)
}

pub async fn attach(name: String) -> Result<()> {
    let stream = launch::connect().await?;
    let (reader, writer) = stream.into_split();
    let mut reader = BufReader::new(reader);
    let mut writer = BufWriter::new(writer);

    let (cols, rows) = terminal_size();

    write_control(
        &mut writer,
        &Request::Attach {
            name: name.clone(),
            cols,
            rows,
            env: super::terminal_env(),
        },
    )
    .await?;

    let readonly = match read_frame(&mut reader).await? {
        Some(Frame::Control(payload)) => {
            let response: Response = serde_json::from_slice(&payload)?;
            match response {
                Response::Attached { readonly } => readonly,
                Response::Error { message } => {
                    anyhow::bail!("{}", message);
                }
                _ => anyhow::bail!("unexpected response"),
            }
        }
        _ => anyhow::bail!("unexpected frame"),
    };

    if readonly {
        eprintln!("[read-only]");
    }

    // Push the terminal's current title so detaching can put it back. The
    // XTerm title stack (CSI 22/23 t) is what iTerm and friends implement;
    // terminals without it ignore both halves harmlessly.
    print!("\x1b[22;0t");

    // Set the title to the same prefix the rewriter applies, so it reads
    // consistently from the moment of attach rather than showing the raw session
    // name until something in the session happens to set a title.
    let mut titles = TitlePrefixer::new(title_parts(&name));
    let initial = if titles.enabled() {
        titles.initial()
    } else {
        name.clone()
    };
    print!("\x1b]1;{}\x07", initial);
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let _guard = RawModeGuard::enter();
    // Drops before _guard, so the escape codes are written before termios is
    // restored — and runs on the `?` paths out of the loop below.
    let _cleanup = TerminalCleanup;

    let mut sigwinch = signal(SignalKind::window_change())?;

    let (stdin_tx, mut stdin_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
    std::thread::spawn(move || {
        use std::io::Read;
        let mut buf = [0u8; 1024];
        let stdin = std::io::stdin();
        let mut handle = stdin.lock();
        loop {
            match handle.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if stdin_tx.blocking_send(buf[..n].to_vec()).is_err() {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
    });

    let mut stdout = std::io::stdout();
    let mut scanner = DetachScanner::new(detach_key_from_env());
    let mut detached = false;
    // The daemon can move this client to another session, so the name it
    // started with is not the name it has.
    let mut current = name.clone();

    loop {
        tokio::select! {
            frame = read_frame(&mut reader) => {
                match frame? {
                    Some(Frame::Data(data)) => {
                        stdout.write_all(&titles.process(&data))?;
                        stdout.flush()?;
                    }
                    Some(Frame::Control(payload)) => {
                        if let Ok(response) = serde_json::from_slice::<Response>(&payload) {
                            match response {
                                Response::SessionName { name } => {
                                    current = name.clone();
                                    // Renaming moves the workspace too.
                                    titles.set_parts(title_parts(&name));
                                    let shown = if titles.enabled() {
                                        titles.initial()
                                    } else {
                                        name.clone()
                                    };
                                    let title = format!("\x1b]1;{}\x07", shown);
                                    stdout.write_all(title.as_bytes())?;
                                    stdout.flush()?;
                                }
                                _ => break,
                            }
                        } else {
                            break;
                        }
                    }
                    None => {
                        break;
                    }
                    _ => {}
                }
            }

            result = stdin_rx.recv() => {
                match result {
                    Some(data) => {
                        match scanner.scan(&data) {
                            Scan::Forward => {
                                write_frame(&mut writer, FRAME_DATA, &data).await?;
                            }
                            Scan::Detach(at) => {
                                if at > 0 {
                                    write_frame(&mut writer, FRAME_DATA, &data[..at]).await?;
                                }
                                use tokio::io::AsyncWriteExt;
                                writer.flush().await.ok();

                                // The key's first stop is the chooser; the
                                // second press, from inside it, is the detach
                                // this used to be.
                                match run_attached_chooser(
                                    &mut reader,
                                    &mut writer,
                                    &mut stdin_rx,
                                    &mut sigwinch,
                                    &current,
                                    detach_key_from_env(),
                                )
                                .await?
                                {
                                    ChooserExit::Switched => continue,
                                    ChooserExit::Detached => {
                                        detached = true;
                                        break;
                                    }
                                    ChooserExit::Gone => break,
                                }
                            }
                        }
                    }
                    None => {
                        break;
                    }
                }
            }

            _ = sigwinch.recv() => {
                let (cols, rows) = terminal_size();
                let mut payload = Vec::with_capacity(4);
                payload.extend_from_slice(&cols.to_be_bytes());
                payload.extend_from_slice(&rows.to_be_bytes());
                write_frame(&mut writer, FRAME_RESIZE, &payload).await?;
            }
        }
    }

    // exit() below skips destructors, so run them explicitly and in order.
    drop(_cleanup);
    drop(_guard);

    if detached {
        eprintln!("[detached: {}] — trip enter to resume", current);
    }

    // The stdin reader thread may be blocked on read(); exit the process
    // to avoid hanging.
    std::process::exit(0);
}

#[cfg(test)]
mod tests {
    use super::*;

    fn scan_all(scanner: &mut DetachScanner, chunks: &[&[u8]]) -> Option<(usize, usize)> {
        for (n, chunk) in chunks.iter().enumerate() {
            if let Scan::Detach(at) = scanner.scan(chunk) {
                return Some((n, at));
            }
        }
        None
    }

    #[test]
    fn detaches_on_key() {
        let mut s = DetachScanner::new(Some(0x1c));
        assert_eq!(scan_all(&mut s, &[b"hello\x1cworld"]), Some((0, 5)));
    }

    #[test]
    fn disabled_key_forwards_everything() {
        let mut s = DetachScanner::new(None);
        assert_eq!(scan_all(&mut s, &[b"\x1c\x1c"]), None);
    }

    #[test]
    fn key_inside_paste_is_forwarded() {
        let mut s = DetachScanner::new(Some(0x1c));
        assert_eq!(
            scan_all(&mut s, &[b"\x1b[200~data\x1cdata\x1b[201~"]),
            None
        );
    }

    #[test]
    fn key_after_paste_detaches() {
        let mut s = DetachScanner::new(Some(0x1c));
        assert_eq!(
            scan_all(&mut s, &[b"\x1b[200~\x1c\x1b[201~", b"\x1c"]),
            Some((1, 0))
        );
    }

    #[test]
    fn paste_marker_split_across_chunks() {
        let mut s = DetachScanner::new(Some(0x1c));
        assert_eq!(
            scan_all(&mut s, &[b"\x1b[2", b"00~in-paste\x1c", b"\x1b[20", b"1~", b"\x1c"]),
            Some((4, 0))
        );
    }

    #[test]
    fn abandoned_marker_prefix_still_detaches() {
        let mut s = DetachScanner::new(Some(0x1c));
        assert_eq!(scan_all(&mut s, &[b"\x1b[2x\x1c"]), Some((0, 4)));
    }

    fn prefixed(chunks: &[&[u8]]) -> String {
        let mut p = TitlePrefixer::new(vec!["proj \u{b7} ".into(), String::new()]);
        let mut out = Vec::new();
        for c in chunks {
            out.extend(p.process(c));
        }
        String::from_utf8_lossy(&out).into_owned()
    }

    #[test]
    fn prefixes_bel_terminated_title() {
        assert_eq!(
            prefixed(&[b"\x1b]0;editing\x07"]),
            "\u{1b}]0;proj · editing\u{7}"
        );
    }

    #[test]
    fn prefixes_st_terminated_title() {
        assert_eq!(
            prefixed(&[b"\x1b]2;build\x1b\\"]),
            "\u{1b}]2;proj · build\u{1b}\\"
        );
    }

    #[test]
    fn passes_surrounding_output_through() {
        assert_eq!(
            prefixed(&[b"before\x1b]1;x\x07after"]),
            "before\u{1b}]1;proj · x\u{7}after"
        );
    }

    #[test]
    fn does_not_stack_prefixes() {
        assert_eq!(
            prefixed(&[b"\x1b]0;proj \xc2\xb7 already\x07"]),
            "\u{1b}]0;proj · already\u{7}"
        );
    }

    #[test]
    fn handles_sequence_split_across_chunks() {
        assert_eq!(
            prefixed(&[b"\x1b]0", b";edi", b"ting\x07rest"]),
            "\u{1b}]0;proj · editing\u{7}rest"
        );
    }

    #[test]
    fn leaves_non_title_osc_alone() {
        // OSC 6 is the tab color; it must not be rewritten.
        let s = prefixed(&[b"\x1b]6;1;bg;red;brightness;40\x07"]);
        assert_eq!(s, "\u{1b}]6;1;bg;red;brightness;40\u{7}");
    }

    #[test]
    fn leaves_plain_escapes_alone() {
        assert_eq!(prefixed(&[b"\x1b[31mred\x1b[0m"]), "\u{1b}[31mred\u{1b}[0m");
    }

    #[test]
    fn empty_title_becomes_the_prefix() {
        assert_eq!(prefixed(&[b"\x1b]0;\x07"]), "\u{1b}]0;proj \u{b7} \u{7}");
    }

    #[test]
    fn rename_preserves_in_flight_sequence() {
        // A rename between the halves of a split title must not drop the
        // withheld bytes; the completed title takes the new prefix.
        let mut p = TitlePrefixer::new(vec!["old ".into(), String::new()]);
        let mut out = Vec::new();
        out.extend(p.process(b"before\x1b]0;par"));
        p.set_parts(vec!["new ".into(), String::new()]);
        out.extend(p.process(b"tial\x07after"));
        assert_eq!(
            String::from_utf8_lossy(&out),
            "before\u{1b}]0;new partial\u{7}after"
        );
    }

    #[test]
    fn appends_a_suffix() {
        let mut p = TitlePrefixer::new(vec![String::new(), " @webapp".into()]);
        let out = p.process(b"\x1b]0;Deliberating\x07");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "\u{1b}]0;Deliberating @webapp\u{7}"
        );
    }

    #[test]
    fn wraps_with_both_affixes() {
        let mut p = TitlePrefixer::new(vec!["~ ".into(), " @webapp".into()]);
        let out = p.process(b"\x1b]0;build\x07");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "\u{1b}]0;~ build @webapp\u{7}"
        );
    }

    #[test]
    fn does_not_stack_a_suffix() {
        let mut p = TitlePrefixer::new(vec![String::new(), " @webapp".into()]);
        let out = p.process(b"\x1b]0;Deliberating @webapp\x07");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "\u{1b}]0;Deliberating @webapp\u{7}"
        );
    }

    #[test]
    fn empty_title_still_gets_affixes() {
        let mut p = TitlePrefixer::new(vec![String::new(), " @webapp".into()]);
        let out = p.process(b"\x1b]0;\x07");
        assert_eq!(String::from_utf8_lossy(&out), "\u{1b}]0; @webapp\u{7}");
    }

    // These shell out on purpose: the expansion had a quoting bug that no
    // in-process test could have caught, because nothing exercised it.

    #[test]
    fn expansion_preserves_spacing() {
        // The bug this guards: field splitting collapsed every space, so
        // "webapp $TITLE" expanded to parts joining as "webappDeliberating".
        assert_eq!(
            expand_title("${TRIP_WORKSPACE##*/} $TITLE", "acme/webapp.2"),
            vec!["webapp ".to_string(), String::new()]
        );
    }

    #[test]
    fn expansion_preserves_interior_separators() {
        assert_eq!(
            expand_title("$TRIP_WORKSPACE - $TITLE", "acme/webapp"),
            vec!["acme/webapp - ".to_string(), String::new()]
        );
    }

    #[test]
    fn expansion_puts_title_last_for_a_suffix() {
        assert_eq!(
            expand_title("$TITLE @${TRIP_WORKSPACE##*/}", "acme/webapp"),
            vec![String::new(), " @webapp".to_string()]
        );
    }

    #[test]
    fn expansion_without_title_yields_one_piece() {
        assert_eq!(
            expand_title("${TRIP_WORKSPACE##*/}", "acme/webapp"),
            vec!["webapp".to_string()]
        );
    }

    #[test]
    fn empty_template_disables_rewriting() {
        assert!(expand_title("", "acme/webapp").is_empty());
    }

    #[test]
    fn title_in_the_middle_wraps_both_sides() {
        let mut p = TitlePrefixer::new(vec!["[".into(), "] ~webapp".into()]);
        let out = p.process(b"\x1b]0;build\x07");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "\u{1b}]0;[build] ~webapp\u{7}"
        );
    }

    #[test]
    fn template_without_title_replaces_it() {
        // One piece means the template had no $TITLE at all.
        let mut p = TitlePrefixer::new(vec!["idle-webapp".into()]);
        let out = p.process(b"\x1b]0;Deliberating\x07");
        assert_eq!(String::from_utf8_lossy(&out), "\u{1b}]0;idle-webapp\u{7}");
    }

    #[test]
    fn initial_title_joins_the_pieces() {
        let p = TitlePrefixer::new(vec!["webapp ".into(), String::new()]);
        assert_eq!(p.initial(), "webapp");
        let p = TitlePrefixer::new(vec![String::new(), " @webapp".into()]);
        assert_eq!(p.initial(), "@webapp");
        let p = TitlePrefixer::new(vec!["[".into(), "] ~webapp".into()]);
        assert_eq!(p.initial(), "[] ~webapp");
    }

    #[test]
    fn prefix_is_concatenated_verbatim() {
        // trip adds no separator of its own — the prefix carries it, so a
        // prefix ending in a space or an arrow lands exactly as written.
        let mut p = TitlePrefixer::new(vec!["~ ".into(), String::new()]);
        let out = p.process(b"\x1b]0;build\x07");
        assert_eq!(String::from_utf8_lossy(&out), "\u{1b}]0;~ build\u{7}");
    }

    #[test]
    fn disabled_passes_everything_through() {
        let mut p = TitlePrefixer::new(Vec::new());
        let out = p.process(b"\x1b]0;editing\x07");
        assert_eq!(String::from_utf8_lossy(&out), "\u{1b}]0;editing\u{7}");
    }

    #[test]
    fn caret_notation_round_trips_the_configured_key() {
        // The hint has to name whatever key the user bound, not a hardcoded
        // ^\\.
        assert_eq!(caret(0x1c), "^\\");
        assert_eq!(caret(0x1f), "^_");
        assert_eq!(caret(0x1a), "^Z");
        assert_eq!(caret(0x7f), "^?");
        for key in [0x1cu8, 0x1f, 0x1a, 0x7f] {
            assert_eq!(parse_detach_key(&caret(key)), Some(key));
        }
    }

    #[test]
    fn parses_caret_notation() {
        assert_eq!(parse_detach_key("^\\"), Some(0x1c));
        assert_eq!(parse_detach_key("^z"), Some(0x1a));
        assert_eq!(parse_detach_key("^Z"), Some(0x1a));
        assert_eq!(parse_detach_key("^_"), Some(0x1f));
        assert_eq!(parse_detach_key("^?"), Some(0x7f));
        assert_eq!(parse_detach_key("none"), None);
        assert_eq!(parse_detach_key("OFF"), None);
        assert_eq!(parse_detach_key("garbage"), Some(DEFAULT_DETACH_KEY));
        assert_eq!(parse_detach_key(""), Some(DEFAULT_DETACH_KEY));
    }

    #[test]
    fn ctrl_dash_is_0x1f_not_cr() {
        assert_eq!(parse_detach_key("^-"), Some(0x1f));
        // '1' & 0x1f would be 0x11 (^Q); out-of-range chars keep the default
        // instead of binding a surprise key.
        assert_eq!(parse_detach_key("^1"), Some(DEFAULT_DETACH_KEY));
    }
}
