use std::io::Write;
use std::os::fd::{AsRawFd, BorrowedFd};

use anyhow::Result;
use nix::libc;
use nix::sys::termios::{self, ControlFlags, InputFlags, LocalFlags, OutputFlags, SetArg};
use tokio::io::{BufReader, BufWriter};
use tokio::signal::unix::{signal, SignalKind};

use crate::daemon::protocol::{
    read_frame, write_control, write_frame, Frame, Request, Response, FRAME_DATA, FRAME_RESIZE,
};

use super::launch;

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
    prefix: String,
    suffix: String,
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
    fn new(prefix: String, suffix: String) -> Self {
        TitlePrefixer {
            prefix,
            suffix,
            state: TitleState::Normal,
            pending: Vec::new(),
            title: Vec::new(),
        }
    }

    fn enabled(&self) -> bool {
        !self.prefix.is_empty() || !self.suffix.is_empty()
    }

    /// Swap the prefix without disturbing the parser. A rename can land
    /// between the two halves of a split sequence; rebuilding the parser would
    /// discard the withheld bytes and print the tail of the title as raw
    /// output, bell included.
    fn set_affixes(&mut self, prefix: String, suffix: String) {
        self.prefix = prefix;
        self.suffix = suffix;
    }

    fn decorate(&self, title: &[u8], out: &mut Vec<u8>) {
        if !self.enabled() {
            out.extend_from_slice(title);
            return;
        }
        let text = String::from_utf8_lossy(title);
        // Leave an already-decorated title alone, so reattaching or a second
        // client cannot stack affixes. An empty affix matches trivially.
        let has_prefix = self.prefix.is_empty() || text.starts_with(self.prefix.as_str());
        let has_suffix = self.suffix.is_empty() || text.ends_with(self.suffix.as_str());
        if !text.is_empty() && has_prefix && has_suffix {
            out.extend_from_slice(title);
            return;
        }
        // Both affixes are complete, separators included, so concatenate.
        out.extend_from_slice(self.prefix.as_bytes());
        out.extend_from_slice(title);
        out.extend_from_slice(self.suffix.as_bytes());
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

/// Expand one affix value with `sh`, with `TRIP_WORKSPACE` and `TRIP_SESSION`
/// in the environment, so the whole of shell parameter expansion is available
/// and there is nothing new to learn. Expanded once per attach, not per title.
fn expand_affix(var: &str, default: &str, session: &str, workspace: &str) -> String {
    let template = std::env::var(var).unwrap_or_else(|_| default.to_string());
    if template.is_empty() {
        return String::new();
    }
    std::process::Command::new("sh")
        .arg("-c")
        .arg(r#"eval "printf %s "$TRIP_AFFIX"""#)
        .env("TRIP_AFFIX", &template)
        .env("TRIP_WORKSPACE", workspace)
        .env("TRIP_SESSION", session)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        // A broken template should not leave the title unset.
        .unwrap_or_else(|| format!("{} ", workspace))
}

/// What wraps every title: `TRIP_TITLE_PREFIX` before it, `TRIP_TITLE_SUFFIX`
/// after it. Both are shell strings expanded once at attach.
///
///   `TRIP_TITLE_PREFIX='${TRIP_WORKSPACE##*/} '`   webapp Deliberating   (the default)
///   `TRIP_TITLE_PREFIX='@${TRIP_WORKSPACE##*/} '`  @webapp Deliberating
///   `TRIP_TITLE_SUFFIX=' @${TRIP_WORKSPACE##*/}'`  Deliberating @webapp
///
/// A suffix is worth preferring where the terminal truncates a long title from
/// the left and keeps the tail — iTerm does — because a prefix is then the part
/// that disappears.
///
/// Each is the *complete* affix: trip adds no separator of its own, so spacing
/// and any divider are part of the value. Both empty leaves titles untouched.
fn title_affixes(session: &str) -> (String, String) {
    let workspace = super::session_base(session);
    (
        expand_affix(
            "TRIP_TITLE_PREFIX",
            "${TRIP_WORKSPACE##*/} ",
            session,
            workspace,
        ),
        expand_affix("TRIP_TITLE_SUFFIX", "", session, workspace),
    )
}

fn terminal_size() -> (u16, u16) {
    let mut ws: libc::winsize = unsafe { std::mem::zeroed() };
    let fd = std::io::stdout().as_raw_fd();
    unsafe { libc::ioctl(fd, libc::TIOCGWINSZ, &mut ws) };
    if ws.ws_col == 0 {
        (80, 24)
    } else {
        (ws.ws_col, ws.ws_row)
    }
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
    let (prefix, suffix) = title_affixes(&name);
    let initial = if prefix.is_empty() && suffix.is_empty() {
        name.clone()
    } else {
        format!("{}{}", prefix, suffix)
    };
    print!("\x1b]1;{}\x07", initial.trim());
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
    let mut titles = TitlePrefixer::new(prefix, suffix);
    let mut detached = false;

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
                                    // Renaming moves the workspace too.
                                    let (prefix, suffix) = title_affixes(&name);
                                    let shown = if prefix.is_empty() && suffix.is_empty() {
                                        name.clone()
                                    } else {
                                        format!("{}{}", prefix, suffix)
                                    };
                                    titles.set_affixes(prefix, suffix);
                                    let title = format!("\x1b]1;{}\x07", shown.trim());
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
                                detached = true;
                                break;
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
        eprintln!("[detached: {}] — trip enter to resume", name);
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
        let mut p = TitlePrefixer::new("proj \u{b7} ".into(), String::new());
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
        let mut p = TitlePrefixer::new("old ".into(), String::new());
        let mut out = Vec::new();
        out.extend(p.process(b"before\x1b]0;par"));
        p.set_affixes("new ".into(), String::new());
        out.extend(p.process(b"tial\x07after"));
        assert_eq!(
            String::from_utf8_lossy(&out),
            "before\u{1b}]0;new partial\u{7}after"
        );
    }

    #[test]
    fn appends_a_suffix() {
        let mut p = TitlePrefixer::new(String::new(), " @webapp".into());
        let out = p.process(b"\x1b]0;Deliberating\x07");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "\u{1b}]0;Deliberating @webapp\u{7}"
        );
    }

    #[test]
    fn wraps_with_both_affixes() {
        let mut p = TitlePrefixer::new("~ ".into(), " @webapp".into());
        let out = p.process(b"\x1b]0;build\x07");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "\u{1b}]0;~ build @webapp\u{7}"
        );
    }

    #[test]
    fn does_not_stack_a_suffix() {
        let mut p = TitlePrefixer::new(String::new(), " @webapp".into());
        let out = p.process(b"\x1b]0;Deliberating @webapp\x07");
        assert_eq!(
            String::from_utf8_lossy(&out),
            "\u{1b}]0;Deliberating @webapp\u{7}"
        );
    }

    #[test]
    fn empty_title_still_gets_affixes() {
        let mut p = TitlePrefixer::new(String::new(), " @webapp".into());
        let out = p.process(b"\x1b]0;\x07");
        assert_eq!(String::from_utf8_lossy(&out), "\u{1b}]0; @webapp\u{7}");
    }

    #[test]
    fn prefix_is_concatenated_verbatim() {
        // trip adds no separator of its own — the prefix carries it, so a
        // prefix ending in a space or an arrow lands exactly as written.
        let mut p = TitlePrefixer::new("~ ".into(), String::new());
        let out = p.process(b"\x1b]0;build\x07");
        assert_eq!(String::from_utf8_lossy(&out), "\u{1b}]0;~ build\u{7}");
    }

    #[test]
    fn disabled_passes_everything_through() {
        let mut p = TitlePrefixer::new(String::new(), String::new());
        let out = p.process(b"\x1b]0;editing\x07");
        assert_eq!(String::from_utf8_lossy(&out), "\u{1b}]0;editing\u{7}");
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
