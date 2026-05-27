use std::collections::HashMap;

use serde::{Deserialize, Serialize};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub const FRAME_CONTROL: u8 = 0x01;
pub const FRAME_DATA: u8 = 0x02;
pub const FRAME_RESIZE: u8 = 0x03;

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Request {
    CreateSession {
        name: String,
        command: Option<Vec<String>>,
        cwd: String,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    ListSessions,
    Attach {
        name: String,
        cols: u16,
        rows: u16,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    GetScreen {
        name: String,
        watch: bool,
    },
    ListScreens {
        name: String,
    },
    GetScreenAt {
        name: String,
        index: usize,
    },
    GetLog {
        name: String,
        raw: bool,
        follow: bool,
        since: Option<f64>,
    },
    SendInput {
        name: String,
        data: Vec<u8>,
    },
    TakeOver {
        name: String,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    SwitchSession {
        from: String,
        to: String,
        command: Option<Vec<String>>,
        cwd: String,
        #[serde(default)]
        env: HashMap<String, String>,
    },
    ReturnSession {
        name: String,
    },
    DetachSession {
        name: String,
    },
    KillSession {
        name: String,
    },
    Shutdown,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum SessionState {
    Running,
    Exited(i32),
}

#[derive(Debug, Serialize, Deserialize)]
pub struct SessionInfo {
    pub name: String,
    pub command: String,
    pub pid: u32,
    pub created_at: u64,
    pub state: SessionState,
    pub attached: bool,
    pub cwd: Option<String>,
    pub fg_command: Option<String>,
    pub git_branch: Option<String>,
    pub title: Option<String>,
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Response {
    Ok,
    Error { message: String },
    SessionCreated { name: String, pid: u32 },
    SessionList { sessions: Vec<SessionInfo> },
    Attached { readonly: bool },
    SessionName { name: String },
    ScreenData { content: String },
    ScreenList { screens: Vec<ScreenEntry> },
    LogData { content: String },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ScreenEntry {
    pub index: usize,
    pub timestamp: String,
    pub lines: usize,
}

pub async fn write_frame<W: AsyncWrite + Unpin>(
    writer: &mut W,
    frame_type: u8,
    payload: &[u8],
) -> anyhow::Result<()> {
    let len = (payload.len() as u32) + 1;
    writer.write_all(&len.to_be_bytes()).await?;
    writer.write_all(&[frame_type]).await?;
    writer.write_all(payload).await?;
    writer.flush().await?;
    Ok(())
}

pub async fn write_control<W: AsyncWrite + Unpin, T: Serialize>(
    writer: &mut W,
    msg: &T,
) -> anyhow::Result<()> {
    let payload = serde_json::to_vec(msg)?;
    write_frame(writer, FRAME_CONTROL, &payload).await
}

pub enum Frame {
    Control(Vec<u8>),
    Data(Vec<u8>),
    Resize { cols: u16, rows: u16 },
}

pub async fn read_frame<R: AsyncRead + Unpin>(reader: &mut R) -> anyhow::Result<Option<Frame>> {
    let mut len_buf = [0u8; 4];
    match reader.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e.into()),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Err(anyhow::anyhow!("empty frame"));
    }

    let mut buf = vec![0u8; len];
    reader.read_exact(&mut buf).await?;

    let frame_type = buf[0];
    let payload = buf[1..].to_vec();

    match frame_type {
        FRAME_CONTROL => Ok(Some(Frame::Control(payload))),
        FRAME_DATA => Ok(Some(Frame::Data(payload))),
        FRAME_RESIZE => {
            if payload.len() >= 4 {
                let cols = u16::from_be_bytes([payload[0], payload[1]]);
                let rows = u16::from_be_bytes([payload[2], payload[3]]);
                Ok(Some(Frame::Resize { cols, rows }))
            } else {
                Err(anyhow::anyhow!("invalid resize frame"))
            }
        }
        _ => Err(anyhow::anyhow!("unknown frame type: {}", frame_type)),
    }
}
