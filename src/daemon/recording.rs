use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

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
}

pub fn now_ts() -> f64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs_f64()
}

pub fn append_event(log_path: &std::path::Path, event: &RecordEvent) {
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
    !trimmed.is_empty()
        && trimmed.chars().all(|c| {
            matches!(
                c,
                '─' | '━'
                    | '═'
                    | '│'
                    | '┃'
                    | '║'
                    | '┌'
                    | '┐'
                    | '└'
                    | '┘'
                    | '├'
                    | '┤'
                    | '┬'
                    | '┴'
                    | '┼'
                    | '╔'
                    | '╗'
                    | '╚'
                    | '╝'
                    | '╠'
                    | '╣'
                    | '╦'
                    | '╩'
                    | '╬'
                    | '╭'
                    | '╮'
                    | '╰'
                    | '╯'
                    | '▔'
                    | '▁'
                    | ' '
            )
        })
}

pub fn clean_screen(text: &str) -> String {
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
