use crate::models::{MessageCounts, QueueOp, Session, SessionStatus, TokenCounts};
use chrono::{DateTime, TimeDelta, Utc};
use serde::Deserialize;
use std::path::{Path, PathBuf};

// ============================================================================
// JSONL Parsing
// ============================================================================

#[derive(Debug, Deserialize)]
pub(crate) struct JsonlMessage {
    #[serde(rename = "type")]
    pub(crate) msg_type: String,
    pub(crate) message: Option<MessageContent>,
    pub(crate) operation: Option<String>,
    pub(crate) cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    pub(crate) git_branch: Option<String>,
    pub(crate) timestamp: Option<String>,
    #[serde(rename = "uuid")]
    pub(crate) uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct MessageContent {
    pub(crate) usage: Option<Usage>,
    #[serde(rename = "model")]
    pub(crate) model: Option<String>,
    #[serde(rename = "content")]
    pub(crate) content: Option<serde_json::Value>, // Can be string or array
}

#[derive(Debug, Deserialize)]
pub(crate) struct Usage {
    #[serde(rename = "input_tokens")]
    pub(crate) input_tokens: Option<u64>,
    #[serde(rename = "output_tokens")]
    pub(crate) output_tokens: Option<u64>,
    #[serde(rename = "cache_read_input_tokens")]
    pub(crate) cache_read_input_tokens: Option<u64>,
    #[serde(rename = "cache_creation_input_tokens")]
    pub(crate) cache_creation_input_tokens: Option<u64>,
}

pub fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

pub fn derive_session_status(ops: &[QueueOp], last_active: DateTime<Utc>) -> SessionStatus {
    let now = Utc::now();
    let idle_minutes = (now - last_active).num_minutes();

    if let Some(last_op) = ops.last() {
        match last_op.operation.as_str() {
            "running" => return SessionStatus::InProgress,
            "enqueue" => {
                let has_resolution = ops
                    .iter()
                    .rev()
                    .skip(1)
                    .any(|op| op.operation == "complete" || op.operation == "dequeue");
                if !has_resolution {
                    if idle_minutes > 10 {
                        return SessionStatus::Idle;
                    }
                    return SessionStatus::Pending;
                }
            }
            "complete" => return SessionStatus::Done,
            "failed" => return SessionStatus::Error,
            _ => {}
        }
    }

    if idle_minutes > 10 {
        SessionStatus::Idle
    } else if idle_minutes > 0 {
        SessionStatus::Pending
    } else {
        SessionStatus::InProgress
    }
}

pub fn scan_all_sessions(max_age_days: i64) -> Vec<Session> {
    let base_path = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claude")
        .join("projects");

    if !base_path.exists() {
        return Vec::new();
    }

    let cutoff = Utc::now() - TimeDelta::try_days(max_age_days).unwrap_or_default();
    let mut sessions = Vec::new();

    let Ok(projects_dirs) = std::fs::read_dir(&base_path) else {
        return Vec::new();
    };

    for project_dir in projects_dirs.flatten() {
        let project_path = project_dir.path();
        if !project_path.is_dir() {
            continue;
        }

        let project_name = project_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();

        let Ok(jsonl_files) = std::fs::read_dir(&project_path) else {
            continue;
        };

        for jsonl_entry in jsonl_files.flatten() {
            let jsonl_path = jsonl_entry.path();
            if jsonl_path.extension().map(|e| e != "jsonl").unwrap_or(true) {
                continue;
            }

            if let Some(session) = parse_session_jsonl(&jsonl_path, &project_name, &project_path) {
                // Filter: only keep sessions active within max_age_days
                if session.last_active >= cutoff {
                    sessions.push(session);
                }
            }
        }
    }

    sessions.sort_by(|a, b| b.last_active.cmp(&a.last_active));
    sessions
}

pub fn parse_session_jsonl(path: &Path, project: &str, project_path: &Path) -> Option<Session> {
    let content = std::fs::read_to_string(path).ok()?;
    let session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut message_counts = MessageCounts::default();
    let mut token_counts = TokenCounts::default();
    let mut queue_ops = Vec::new();
    let mut last_cwd = String::new();
    let mut last_branch: Option<String> = None;
    let mut last_active = Utc::now();
    let mut last_model: Option<String> = None;
    let mut last_user_msg: Option<DateTime<Utc>> = None;
    let mut last_asst_msg: Option<DateTime<Utc>> = None;

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        let Ok(msg) = serde_json::from_str::<JsonlMessage>(line) else {
            continue;
        };

        if let Some(ts) = msg.timestamp.as_ref().and_then(|s| parse_timestamp(s)) {
            last_active = ts;
        }

        match msg.msg_type.as_str() {
            "assistant" => {
                message_counts.assistant += 1;
                // Capture timestamp and latest model from assistant messages
                if let Some(ts) = msg.timestamp.as_ref().and_then(|s| parse_timestamp(s)) {
                    last_asst_msg = Some(ts);
                }
                if let Some(ref m) = msg.message {
                    if let Some(ref model) = m.model {
                        last_model = Some(model.clone());
                    }
                }
                // Only skip the Claude home dir; real project paths get stored
                if let Some(ref cwd) = msg.cwd {
                    let home = std::env::var("HOME").unwrap_or_default();
                    // Prefer non-home dirs; only use home dirs if nothing else seen
                    if *cwd != format!("{}/.claude", home) && *cwd != home || last_cwd.is_empty() {
                        last_cwd = cwd.clone();
                    }
                }
                last_branch = msg.git_branch.clone().or(last_branch);

                if let Some(usage) = msg.message.as_ref().and_then(|m| m.usage.as_ref()) {
                    token_counts.input_tokens += usage.input_tokens.unwrap_or(0);
                    token_counts.output_tokens += usage.output_tokens.unwrap_or(0);
                    token_counts.cache_read_input_tokens +=
                        usage.cache_read_input_tokens.unwrap_or(0);
                    token_counts.cache_creation_input_tokens +=
                        usage.cache_creation_input_tokens.unwrap_or(0);
                }
            }
            "user" => {
                message_counts.user += 1;
                // Capture user message timestamp only if it's not a tool result
                // Tool results have all content blocks with type "tool_result"
                // Content can be string (normal msg) or array (tool_result or mixed)
                let is_tool_result = msg.message.as_ref()
                    .and_then(|m| m.content.as_ref())
                    .map(|v| {
                        if let Some(arr) = v.as_array() {
                            arr.iter().all(|item| {
                                item.get("type")
                                    .and_then(|t| t.as_str())
                                    .map(|t| t == "tool_result")
                                    .unwrap_or(false)
                            })
                        } else {
                            false // String content means it's a normal user message
                        }
                    })
                    .unwrap_or(false);
                if !is_tool_result {
                    if let Some(ts) = msg.timestamp.as_ref().and_then(|s| parse_timestamp(s)) {
                        last_user_msg = Some(ts);
                    }
                }
                // Skip only ~/.claude and $HOME; real project paths get stored
                if let Some(ref cwd) = msg.cwd {
                    let home = std::env::var("HOME").unwrap_or_default();
                    // Prefer non-home dirs; only use home dirs if nothing else seen
                    if *cwd != format!("{}/.claude", home) && *cwd != home || last_cwd.is_empty() {
                        last_cwd = cwd.clone();
                    }
                }
                last_branch = msg.git_branch.clone().or(last_branch);
            }
            "system" => {
                message_counts.system += 1;
            }
            "queue-operation" => {
                if let Some(op) = msg.operation {
                    queue_ops.push(QueueOp {
                        operation: op,
                        timestamp: last_active,
                    });
                }
            }
            _ => {}
        }
    }

    let status = derive_session_status(&queue_ops, last_active);

    Some(Session {
        id: session_id,
        project: project.to_string(),
        project_path: project_path.to_string_lossy().to_string(),
        cwd: last_cwd,
        git_branch: last_branch,
        status,
        last_active,
        message_counts,
        token_counts,
        queue_ops,
        model: last_model,
        last_user_msg,
        last_asst_msg,
    })
}
