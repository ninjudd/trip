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

    // Set tab title to session name
    print!("\x1b]1;{}\x07", name);
    std::io::Write::flush(&mut std::io::stdout()).ok();

    let _guard = RawModeGuard::enter();

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

    loop {
        tokio::select! {
            frame = read_frame(&mut reader) => {
                match frame? {
                    Some(Frame::Data(data)) => {
                        stdout.write_all(&data)?;
                        stdout.flush()?;
                    }
                    Some(Frame::Control(payload)) => {
                        if let Ok(response) = serde_json::from_slice::<Response>(&payload) {
                            match response {
                                Response::SessionName { name } => {
                                    let title = format!("\x1b]1;{}\x07", name);
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

    // Reset terminal modes that the session's app may have enabled
    // (mouse tracking, alternate screen buffer, bracketed paste)
    stdout.write_all(super::TERMINAL_RESET).ok();
    stdout.flush().ok();

    // Restore original terminal settings
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
