use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use super::recording::{self, RecordEvent};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    pub kind: String,
    pub log_path: String,
}

pub fn agent_config_path(session_name: &str) -> PathBuf {
    crate::common::session_dir(session_name).join("agent.json")
}

pub fn read_agent_config(session_name: &str) -> Option<AgentConfig> {
    let path = agent_config_path(session_name);
    let data = std::fs::read_to_string(path).ok()?;
    serde_json::from_str(&data).ok()
}

fn parse_iso_timestamp(ts: Option<&serde_json::Value>) -> f64 {
    ts.and_then(|v| v.as_str())
        .and_then(|s| chrono::DateTime::parse_from_rfc3339(s).ok())
        .map(|dt| dt.timestamp_millis() as f64 / 1000.0)
        .unwrap_or_else(recording::now_ts)
}

fn parse_claude_line(line: &str, session_started: &mut bool) -> Vec<RecordEvent> {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let t = parse_iso_timestamp(v.get("timestamp"));
    let event_type = v.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut events = vec![];

    match event_type {
        "user" => {
            if !*session_started {
                *session_started = true;
                let session_id = v
                    .get("sessionId")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                events.push(RecordEvent::AgentSessionStart {
                    t,
                    continuation: session_id,
                });
            }
            if let Some(arr) = v["message"]["content"].as_array() {
                for block in arr {
                    if block.get("type").and_then(|v| v.as_str()) == Some("tool_result") {
                        events.push(RecordEvent::AgentToolResult {
                            t,
                            tool_call_id: block["tool_use_id"].as_str().unwrap_or("").to_string(),
                            output: block
                                .get("content")
                                .cloned()
                                .unwrap_or(serde_json::Value::Null),
                            is_error: block
                                .get("is_error")
                                .and_then(|v| v.as_bool())
                                .unwrap_or(false),
                        });
                    }
                }
            }
        }
        "assistant" => {
            events.push(RecordEvent::AgentActivity { t });
            if let Some(blocks) = v["message"]["content"].as_array() {
                for block in blocks {
                    match block.get("type").and_then(|v| v.as_str()) {
                        Some("text") => {
                            if let Some(text) = block.get("text").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    events.push(RecordEvent::AgentText {
                                        t,
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                        Some("thinking") => {
                            if let Some(text) = block.get("thinking").and_then(|v| v.as_str()) {
                                if !text.is_empty() {
                                    events.push(RecordEvent::AgentThinking {
                                        t,
                                        text: text.to_string(),
                                    });
                                }
                            }
                        }
                        Some("tool_use") => {
                            events.push(RecordEvent::AgentToolCall {
                                t,
                                id: block["id"].as_str().unwrap_or("").to_string(),
                                name: block["name"].as_str().unwrap_or("").to_string(),
                                input: block
                                    .get("input")
                                    .cloned()
                                    .unwrap_or(serde_json::Value::Null),
                            });
                        }
                        _ => {}
                    }
                }
            }
        }
        "system" => {
            if v.get("subtype").and_then(|v| v.as_str()) == Some("turn_duration") {
                let duration_ms = v.get("durationMs").and_then(|v| v.as_u64());
                events.push(RecordEvent::AgentTurnEnd { t, duration_ms });
            }
        }
        _ => {}
    }
    events
}

fn parse_codex_line(line: &str, session_started: &mut bool) -> Vec<RecordEvent> {
    let v: serde_json::Value = match serde_json::from_str(line) {
        Ok(v) => v,
        Err(_) => return vec![],
    };
    let t = parse_iso_timestamp(v.get("timestamp"));
    let top_type = v.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let mut events = vec![];

    match top_type {
        "session_meta" => {
            if !*session_started {
                *session_started = true;
                let id = v["payload"]["id"].as_str().unwrap_or("").to_string();
                events.push(RecordEvent::AgentSessionStart {
                    t,
                    continuation: id,
                });
            }
        }
        "event_msg" => {
            let payload = &v["payload"];
            match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "agent_reasoning" => {
                    if let Some(text) = payload.get("text").and_then(|v| v.as_str()) {
                        events.push(RecordEvent::AgentThinking {
                            t,
                            text: text.to_string(),
                        });
                    }
                }
                "agent_message" => {
                    if let Some(text) = payload.get("message").and_then(|v| v.as_str()) {
                        events.push(RecordEvent::AgentText {
                            t,
                            text: text.to_string(),
                        });
                    }
                }
                "task_complete" => {
                    events.push(RecordEvent::AgentTurnEnd {
                        t,
                        duration_ms: None,
                    });
                }
                "task_started" => {
                    events.push(RecordEvent::AgentActivity { t });
                }
                _ => {}
            }
        }
        "response_item" => {
            let payload = &v["payload"];
            match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "function_call" => {
                    events.push(RecordEvent::AgentToolCall {
                        t,
                        id: payload["call_id"].as_str().unwrap_or("").to_string(),
                        name: payload["name"].as_str().unwrap_or("").to_string(),
                        input: payload
                            .get("arguments")
                            .and_then(|v| v.as_str())
                            .and_then(|s| serde_json::from_str(s).ok())
                            .unwrap_or(serde_json::Value::Null),
                    });
                }
                // Codex records shell execution as custom_tool_call, not
                // function_call: in a real transcript it is 96 of them
                // against a single function_call. Dropping them meant a Codex
                // agent logged no agent_tool_call at all, so anything
                // deriving state from tool calls was blind to every command
                // it ran. `input` is a source string rather than JSON here,
                // so it is passed through as a string when it does not parse.
                "custom_tool_call" => {
                    let raw = payload.get("input").and_then(|v| v.as_str()).unwrap_or("");
                    events.push(RecordEvent::AgentToolCall {
                        t,
                        id: payload["call_id"].as_str().unwrap_or("").to_string(),
                        name: payload["name"].as_str().unwrap_or("").to_string(),
                        input: serde_json::from_str(raw)
                            .unwrap_or_else(|_| serde_json::Value::String(raw.to_string())),
                    });
                }
                "custom_tool_call_output" => {
                    events.push(RecordEvent::AgentToolResult {
                        t,
                        tool_call_id: payload["call_id"].as_str().unwrap_or("").to_string(),
                        output: payload
                            .get("output")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        is_error: payload
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    });
                }
                "function_call_output" => {
                    events.push(RecordEvent::AgentToolResult {
                        t,
                        tool_call_id: payload["call_id"].as_str().unwrap_or("").to_string(),
                        output: payload
                            .get("output")
                            .cloned()
                            .unwrap_or(serde_json::Value::Null),
                        is_error: payload
                            .get("is_error")
                            .and_then(|v| v.as_bool())
                            .unwrap_or(false),
                    });
                }
                "reasoning" => {
                    if let Some(arr) = payload.get("summary").and_then(|v| v.as_array()) {
                        let text: Vec<&str> = arr
                            .iter()
                            .filter_map(|item| item.get("text").and_then(|v| v.as_str()))
                            .collect();
                        if !text.is_empty() {
                            events.push(RecordEvent::AgentThinking {
                                t,
                                text: text.join("\n"),
                            });
                        }
                    }
                }
                _ => {}
            }
        }
        _ => {}
    }
    events
}

pub fn parse_line(kind: &str, line: &str, session_started: &mut bool) -> Vec<RecordEvent> {
    match kind {
        "claude" => parse_claude_line(line, session_started),
        "codex" => parse_codex_line(line, session_started),
        _ => vec![],
    }
}

/// Tail an agent's JSONL log file, appending normalized events to trip's log.
/// Returns when agent.json is removed (by preexec hook) or replaced (by new `trip on`).
pub async fn tail_agent_log(session_name: String) {
    let config = match read_agent_config(&session_name) {
        Some(c) => c,
        None => return,
    };

    let cli_log = PathBuf::from(&config.log_path);
    let trip_log = crate::common::log_path(&session_name);
    let mut last_size: u64 = 0;
    let mut session_started = false;

    loop {
        // Exit if agent.json was removed (by preexec hook) or replaced (by new trip on)
        match read_agent_config(&session_name) {
            None => break,
            Some(c) if c.log_path != config.log_path => break,
            _ => {}
        }

        let current_size = std::fs::metadata(&cli_log).map(|m| m.len()).unwrap_or(0);
        if current_size > last_size {
            if let Ok(mut file) = std::fs::File::open(&cli_log) {
                use std::io::{Read, Seek, SeekFrom};
                if file.seek(SeekFrom::Start(last_size)).is_ok() {
                    let mut new_bytes = String::new();
                    if file.read_to_string(&mut new_bytes).is_ok() {
                        for line in new_bytes.lines() {
                            let events = parse_line(&config.kind, line, &mut session_started);
                            for event in events {
                                recording::append_event(&trip_log, &event);
                            }
                        }
                    }
                }
            }
            last_size = current_size;
        }

        tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    }

    // Emit session end when the agent exits
    if session_started {
        recording::append_event(
            &trip_log,
            &RecordEvent::AgentSessionEnd {
                t: recording::now_ts(),
                stop_reason: "stop".to_string(),
            },
        );
    }
}

#[cfg(test)]
mod codex_parser_tests {
    use super::{parse_codex_line, RecordEvent};

    /// Verbatim from a real rollout transcript, trimmed to the fields the
    /// parser reads. That transcript held 96 custom_tool_call entries and one
    /// function_call, so this is the shape Codex actually emits for shell work.
    const CALL: &str = r#"{"type":"response_item","payload":{"type":"custom_tool_call","id":"ctc_085c","status":"completed","call_id":"call_94K4","name":"exec","input":"const r = await tools.exec_command({\"cmd\":\"ls -la\"});\ntext(r.output);\n"}}"#;
    const OUTPUT: &str = r#"{"type":"response_item","payload":{"type":"custom_tool_call_output","id":"ctco_01a0","call_id":"call_94K4","output":[{"type":"input_text","text":"Script completed"}]}}"#;

    #[test]
    fn custom_tool_call_becomes_a_tool_call() {
        let mut started = true;
        let events = parse_codex_line(CALL, &mut started);
        assert_eq!(events.len(), 1, "expected exactly one event");
        match &events[0] {
            RecordEvent::AgentToolCall { id, name, input, .. } => {
                assert_eq!(id, "call_94K4");
                assert_eq!(name, "exec");
                // input is a source string, not JSON, so it survives as one
                // rather than being dropped to null.
                assert!(input.as_str().unwrap().contains("exec_command"));
            }
            _ => panic!("expected AgentToolCall"),
        }
    }

    #[test]
    fn custom_tool_call_output_becomes_a_tool_result_on_the_same_id() {
        let mut started = true;
        let events = parse_codex_line(OUTPUT, &mut started);
        assert_eq!(events.len(), 1);
        match &events[0] {
            RecordEvent::AgentToolResult { tool_call_id, output, is_error, .. } => {
                // The call and its result correlate, which is what lets a
                // consumer tell a pending call from an answered one.
                assert_eq!(tool_call_id, "call_94K4");
                assert!(!is_error);
                assert!(output.is_array());
            }
            _ => panic!("expected AgentToolResult"),
        }
    }
}
