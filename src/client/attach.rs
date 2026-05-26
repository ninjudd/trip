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
                        write_frame(&mut writer, FRAME_DATA, &data).await?;
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

    // The stdin reader thread may be blocked on read(); exit the process
    // to avoid hanging.
    std::process::exit(0);
}
