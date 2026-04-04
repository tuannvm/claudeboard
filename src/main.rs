use chrono::{DateTime, TimeDelta, Utc};
use clap::Parser;
use crossterm::event::{KeyCode, KeyEventKind};
use crossterm::terminal::{EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode};
use parking_lot::RwLock;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use ratatui::style::Styled;
use serde::Deserialize;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;

// ============================================================================
// Helpers
// ============================================================================

fn truncate_from_end(s: &str, max_chars: usize) -> String {
    let count = s.chars().count();
    if count <= max_chars {
        return s.to_string();
    }
    s.chars().rev().take(max_chars).collect::<Vec<_>>().into_iter().rev().collect()
}

// ============================================================================
// CLI
// ============================================================================

#[derive(Parser, Debug, Clone)]
#[command(name = "claude-watch")]
#[command(about = "TUI dashboard for monitoring Claude Code via tmux")]
struct Args {
    #[arg(long, default_value = "5", help = "Refresh interval in seconds")]
    refresh_interval: u64,

    #[arg(long, help = "tmux socket path")]
    tmux_socket: Option<String>,
}

// ============================================================================
// Color Theme (Tokyo Night)
// ============================================================================

mod colors {
    use ratatui::style::Color;

    pub const BG: Color        = Color::Rgb(0x1a, 0x1b, 0x26); // dark navy
    pub const SURFACE: Color    = Color::Rgb(0x24, 0x28, 0x3b); // panel bg
    pub const PRIMARY: Color   = Color::Rgb(0xc0, 0xca, 0xf5); // main text
    pub const SECONDARY: Color = Color::Rgb(0x56, 0x5f, 0x89); // dim text
    pub const ACCENT: Color    = Color::Rgb(0x7a, 0xa2, 0xf7); // blue highlight
    pub const GREEN: Color     = Color::Rgb(0x9e, 0xce, 0x6a); // running/active
    pub const YELLOW: Color    = Color::Rgb(0xe0, 0xaf, 0x68); // pending/idle
    pub const RED: Color       = Color::Rgb(0xf7, 0x76, 0x8e);  // failed/error
    pub const CYAN: Color      = Color::Rgb(0x73, 0xda, 0xca); // info
    pub const PURPLE: Color    = Color::Rgb(0xbb, 0x9a, 0xf7);  // special
    pub const BORDER: Color    = Color::Rgb(0x41, 0x48, 0x68);  // borders
}

// ============================================================================
// Data Models
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[derive(Default)]
pub enum SessionStatus {
    InProgress,
    Pending,
    #[default]
    Idle,
    Done,
    Error,
}

#[derive(Debug, Clone, Default)]
pub struct MessageCounts {
    pub assistant: u64,
    pub user: u64,
    pub system: u64,
}

#[derive(Debug, Clone, Default)]
pub struct TokenCounts {
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_input_tokens: u64,
    pub cache_creation_input_tokens: u64,
}

impl TokenCounts {
    pub fn total(&self) -> u64 {
        self.input_tokens
            + self.output_tokens
            + self.cache_read_input_tokens
            + self.cache_creation_input_tokens
    }
}

#[derive(Debug, Clone)]
pub struct QueueOp {
    pub operation: String,
    pub timestamp: DateTime<Utc>,
}

#[derive(Debug, Clone)]
pub struct Session {
    pub id: String,
    pub project: String,
    pub project_path: String,
    pub cwd: String,
    pub git_branch: Option<String>,
    pub status: SessionStatus,
    pub last_active: DateTime<Utc>,
    pub message_counts: MessageCounts,
    pub token_counts: TokenCounts,
    pub queue_ops: Vec<QueueOp>,
}

#[derive(Debug, Clone)]
pub struct TokenUsageEntry {
    pub timestamp: DateTime<Utc>,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_tokens: u64,
    pub total_tokens: u64,
    pub cost: f64,
}

#[derive(Debug, Clone, Default)]
pub struct AggregatedTokens {
    pub total_tokens: u64,
    pub total_cost: f64,
    pub today_tokens: u64,
    pub today_cost: f64,
    pub entries_today: Vec<TokenUsageEntry>,
    pub hourly_rates: Vec<u64>,
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PaneKey {
    pub session: String,
    pub window: String,
    pub pane: String,
}

#[derive(Debug, Clone)]
pub struct TmuxPane {
    pub pane_id: String,
    pub window_name: String,
    pub window_index: String,
    pub session_name: String,
    pub cwd: String,
    pub running_cmd: Option<String>,
    pub pane_title: Option<String>,
    pub pane_dead: bool, // true if pane is dead (process exited)
}

/// Check if a pane is running a coding agent (Claude Code, Codex, or Gemini)
/// Uses only the running_cmd (foreground process) - does NOT use pane_title.
/// This relies on the actual command binary name being detected.
fn is_coding_agent(pane: &TmuxPane) -> bool {
    // Dead panes are not running anything
    if pane.pane_dead {
        return false;
    }

    // Check running_cmd - this is the primary indicator of what's actually running
    if let Some(ref cmd) = pane.running_cmd {
        let cmd_lower = cmd.to_lowercase();

        // Known coding agent commands (check the binary name, not path)
        let cmd_basename = std::path::Path::new(cmd)
            .file_name()
            .and_then(|n| n.to_str())
            .map(|n| n.to_lowercase())
            .unwrap_or(cmd_lower);

        let is_agent_cmd = cmd_basename.contains("claude")
            || cmd_basename.contains("codex")
            || cmd_basename.contains("gemini")
            || cmd_basename.contains("anthropic");

        // Claude Code versions are like "2.1.89", "3.5.0" etc
        // Check for pattern: number.number (allow multiple parts)
        let is_version = {
            let parts: Vec<&str> = cmd_basename.split('.').collect();
            parts.len() >= 2
                && parts.iter().all(|p| p.chars().all(|c| c.is_ascii_digit()))
                && parts[0].chars().all(|c| c.is_ascii_digit())
        };

        if is_version || is_agent_cmd {
            return true;
        }
    }

    false
}

#[derive(Debug, Clone)]
pub struct TmuxSession {
    pub name: String,
    pub group: Option<String>, // None means standalone (no group), Some means part of a group
    pub panes: Vec<TmuxPane>,
}

#[derive(Debug, Clone)]
pub struct TmuxWorkspace {
    pub sessions: Vec<TmuxSession>,
    pub total_panes: usize,
}

#[derive(Debug, Clone, Default)]
pub struct AppState {
    // TMUX navigation
    pub tmux_workspace: Option<TmuxWorkspace>,
    pub selected_pane_idx: usize, // linear index across agent panes only
    pub agent_pane_count: usize,   // total count of agent panes (for navigation bounds)
    // Session data (filtered to ≤7 days)
    pub sessions: Vec<Session>,
    pub session_by_pane: HashMap<PaneKey, usize>, // pane key -> session index
    // Token data
    pub aggregated_tokens: AggregatedTokens,
    // Refresh
    pub refresh_countdown: u64,
    // tmux socket (needed for live pane capture)
    pub tmux_socket: Option<String>,
}

// ============================================================================
// JSONL Parsing
// ============================================================================

#[derive(Debug, Deserialize)]
struct JsonlMessage {
    #[serde(rename = "type")]
    msg_type: String,
    message: Option<MessageContent>,
    operation: Option<String>,
    cwd: Option<String>,
    #[serde(rename = "gitBranch")]
    git_branch: Option<String>,
    timestamp: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageContent {
    usage: Option<Usage>,
}

#[derive(Debug, Deserialize)]
struct Usage {
    #[serde(rename = "input_tokens")]
    input_tokens: Option<u64>,
    #[serde(rename = "output_tokens")]
    output_tokens: Option<u64>,
    #[serde(rename = "cache_read_input_tokens")]
    cache_read_input_tokens: Option<u64>,
    #[serde(rename = "cache_creation_input_tokens")]
    cache_creation_input_tokens: Option<u64>,
}

fn parse_timestamp(s: &str) -> Option<DateTime<Utc>> {
    DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

fn derive_session_status(ops: &[QueueOp], last_active: DateTime<Utc>) -> SessionStatus {
    let now = Utc::now();
    let idle_minutes = (now - last_active).num_minutes();

    if let Some(last_op) = ops.last() {
        match last_op.operation.as_str() {
            "running" => return SessionStatus::InProgress,
            "enqueue" => {
                let has_resolution = ops.iter().rev().skip(1).any(|op| {
                    op.operation == "complete" || op.operation == "dequeue"
                });
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

fn scan_all_sessions(max_age_days: i64) -> Vec<Session> {
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

fn parse_session_jsonl(path: &PathBuf, project: &str, project_path: &PathBuf) -> Option<Session> {
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
                // Only skip the Claude home dir; real project paths get stored
                if let Some(ref cwd) = msg.cwd {
                    let home = std::env::var("HOME").unwrap_or_default();
                    if *cwd != format!("{}/.claude", home) && *cwd != home {
                        last_cwd = cwd.clone();
                    } else if last_cwd.is_empty() {
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
                // Skip only ~/.claude and $HOME; real project paths get stored
                if let Some(ref cwd) = msg.cwd {
                    let home = std::env::var("HOME").unwrap_or_default();
                    if *cwd != format!("{}/.claude", home) && *cwd != home {
                        last_cwd = cwd.clone();
                    } else if last_cwd.is_empty() {
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
    })
}

// ============================================================================
// Token Log Parsing
// ============================================================================

fn parse_token_logs() -> AggregatedTokens {
    let mut aggregated = AggregatedTokens::default();
    let logs_dir = PathBuf::from(std::env::var("HOME").unwrap_or_default())
        .join(".claude")
        .join("logs");

    if !logs_dir.exists() {
        return aggregated;
    }

    let Ok(entries) = std::fs::read_dir(&logs_dir) else {
        return aggregated;
    };

    let today = Utc::now().date_naive();
    let mut hourly_tokens: HashMap<i64, u64> = HashMap::new();

    for entry in entries.flatten() {
        let path = entry.path();
        if !path
            .file_name()
            .map(|n| n.to_string_lossy().starts_with("tokens-"))
            .unwrap_or(false)
        {
            continue;
        }

        let content = match std::fs::read_to_string(&path) {
            Ok(c) => c,
            Err(_) => continue,
        };

        for line in content.lines() {
            let parts: Vec<&str> = line.split('|').collect();
            if parts.len() < 7 {
                continue;
            }

            let timestamp = match parse_timestamp(parts[0]) {
                Some(ts) => ts,
                None => continue,
            };

            let model = parts[1].to_string();
            let input_tokens: u64 = parts[2].parse().unwrap_or(0);
            let output_tokens: u64 = parts[3].parse().unwrap_or(0);
            let cache_tokens: u64 = parts[4].parse().unwrap_or(0);
            let total_tokens: u64 = parts[5].parse().unwrap_or(0);
            let cost: f64 = parts[6].parse().unwrap_or(0.0);

            aggregated.total_tokens += total_tokens;
            aggregated.total_cost += cost;

            let hour_key = timestamp.timestamp() / 3600;
            *hourly_tokens.entry(hour_key).or_insert(0) += total_tokens;

            if timestamp.date_naive() == today {
                aggregated.today_tokens += total_tokens;
                aggregated.today_cost += cost;
                aggregated.entries_today.push(TokenUsageEntry {
                    timestamp,
                    model,
                    input_tokens,
                    output_tokens,
                    cache_tokens,
                    total_tokens,
                    cost,
                });
            }
        }
    }

    let now_hour = Utc::now().timestamp() / 3600;
    for i in 0..24 {
        let hour = now_hour - i;
        aggregated
            .hourly_rates
            .push(hourly_tokens.get(&hour).copied().unwrap_or(0));
    }
    aggregated.hourly_rates.reverse();
    aggregated.entries_today.sort_by(|a, b| b.timestamp.cmp(&a.timestamp));
    // entries_today is used for model breakdown — keep all entries for today, limit to last 50 for display
    aggregated.entries_today.truncate(50);

    aggregated
}

// ============================================================================
// Tmux Parsing
// ============================================================================

fn parse_tmux_workspace(socket: &Option<String>) -> Option<TmuxWorkspace> {
    let socket_args: Vec<&str> = match socket {
        Some(s) => ["-L", s.as_str()].to_vec(),
        None => vec![],
    };

    // Get session names and their groups
    let mut cmd = std::process::Command::new("tmux");
    for arg in &socket_args {
        cmd.arg(arg);
    }
    cmd.args(["list-sessions", "-F", "#{session_name}|#{session_group}"]);
    let output = match cmd.output() {
        Ok(o) if o.status.success() => o,
        _ => return None,
    };

    let session_infos: Vec<(String, Option<String>)> = String::from_utf8_lossy(&output.stdout)
        .lines()
        .filter(|l| !l.is_empty())
        .map(|line| {
            let parts: Vec<&str> = line.splitn(2, '|').collect();
            let name = parts[0].to_string();
            // group is the session group - if session belongs to a group different from its name,
            // it's a secondary session and should be filtered out
            let group = parts.get(1).filter(|g| !g.is_empty()).map(|g| g.to_string());
            (name, group)
        })
        .collect();

    if session_infos.is_empty() {
        return Some(TmuxWorkspace {
            sessions: vec![],
            total_panes: 0,
        });
    }

    // Filter to only primary sessions: either standalone sessions (no group) or the first session in each group
    // Secondary sessions (group != name) share windows with the primary and should be hidden
    let primary_sessions: Vec<(String, Option<String>)> = session_infos
        .into_iter()
        .filter(|(name, group): &(String, Option<String>)| {
            // Keep if: group is None (standalone) OR group == name (primary session)
            // Skip if: group is Some and group != name (secondary session)
            match group {
                Some(g) if g.as_str() != *name => false,
                _ => true,
            }
        })
        .collect();

    let mut total_panes = 0;
    let sessions: Vec<TmuxSession> = primary_sessions
        .iter()
        .map(|(session_name, group)| {
            // NOTE: tmux list-panes -t session only returns panes from the CURRENT window,
            // not all windows in the session. We must iterate over each window.
            let mut all_panes: Vec<TmuxPane> = Vec::new();

            // First get all windows in this session
            let mut list_windows_cmd = std::process::Command::new("tmux");
            for arg in &socket_args {
                list_windows_cmd.arg(arg);
            }
            list_windows_cmd.args(["list-windows", "-t", session_name, "-F", "#{window_index}"]);

            let windows_output = match list_windows_cmd.output() {
                Ok(o) if o.status.success() => o,
                _ => return TmuxSession {
                    name: session_name.clone(),
                    group: group.clone(),
                    panes: vec![],
                },
            };

            let window_indices: Vec<String> = String::from_utf8_lossy(&windows_output.stdout)
                .lines()
                .filter(|l| !l.is_empty())
                .map(|s| s.to_string())
                .collect();

            // For each window, get all panes
            for window_idx in window_indices {
                let target = format!("{}:{}", session_name, window_idx);

                let mut list_panes_cmd = std::process::Command::new("tmux");
                for arg in &socket_args {
                    list_panes_cmd.arg(arg);
                }
                list_panes_cmd.args([
                    "list-panes", "-t", &target, "-F",
                    "#{pane_id}|#{window_name}|#{window_index}|#{session_name}|#{pane_current_path}|#{pane_current_command}|#{pane_title}|#{pane_dead}",
                ]);

                let output = match list_panes_cmd.output() {
                    Ok(o) if o.status.success() => o,
                    _ => continue,
                };

                let panes: Vec<TmuxPane> = String::from_utf8_lossy(&output.stdout)
                    .lines()
                    .filter_map(|line| {
                        let parts: Vec<&str> = line.splitn(8, '|').collect();
                        if parts.len() < 7 {
                            return None;
                        }
                        let pane_id = parts[0].to_string();
                        let window_name = parts[1].to_string();
                        let window_index = parts[2].to_string();
                        let session_name_str = parts[3].to_string();
                        let cwd = parts[4].to_string();
                        let running_cmd = if parts[5].is_empty() || parts[5] == "0" {
                            None
                        } else {
                            Some(parts[5].to_string())
                        };
                        let pane_title = if !parts[6].is_empty() {
                            Some(parts[6].to_string())
                        } else {
                            None
                        };
                        let pane_dead = parts.get(7).map(|s| *s == "1").unwrap_or(false);
                        Some(TmuxPane {
                            pane_id,
                            window_name,
                            window_index,
                            session_name: session_name_str,
                            cwd,
                            running_cmd,
                            pane_title,
                            pane_dead,
                        })
                    })
                    .collect();

                all_panes.extend(panes);
            }

            total_panes += all_panes.len();
            TmuxSession {
                name: session_name.clone(),
                group: group.clone(),
                panes: all_panes,
            }
        })
        .collect();

    Some(TmuxWorkspace {
        sessions,
        total_panes,
    })
}

// ============================================================================
// UI Helpers
// ============================================================================

fn session_status_icon(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::InProgress => "●",
        SessionStatus::Pending => "○",
        SessionStatus::Idle => "○",
        SessionStatus::Done => "✓",
        SessionStatus::Error => "✗",
    }
}

fn session_status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::InProgress => colors::GREEN,
        SessionStatus::Pending => colors::YELLOW,
        SessionStatus::Idle => colors::SECONDARY,
        SessionStatus::Done => colors::CYAN,
        SessionStatus::Error => colors::RED,
    }
}

fn queue_op_icon(op: &str) -> &'static str {
    match op {
        "running" => "●",
        "enqueue" => "○",
        "complete" => "✓",
        "failed" => "✗",
        "dequeue" => "·",
        _ => "?",
    }
}

fn queue_op_color(op: &str) -> Color {
    match op {
        "running" => colors::GREEN,
        "enqueue" => colors::YELLOW,
        "complete" => colors::CYAN,
        "failed" => colors::RED,
        "dequeue" => colors::SECONDARY,
        _ => colors::PRIMARY,
    }
}

fn format_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{:.1}K", n as f64 / 1_000.0)
    } else {
        format!("{}", n)
    }
}

// ============================================================================
// Rendering
// ============================================================================

fn render_header(f: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::new()
        .borders(Borders::NONE)
        .style(Style::default().bg(colors::SURFACE));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let now = Utc::now();
    let clock = now.format("%H:%M:%S").to_string();
    let countdown = format!("↻ in {}s", state.refresh_countdown);

    // Count sessions by status
    let in_progress = state.sessions.iter().filter(|s| s.status == SessionStatus::InProgress).count();
    let pending = state.sessions.iter().filter(|s| s.status == SessionStatus::Pending).count();
    let idle = state.sessions.iter().filter(|s| s.status == SessionStatus::Idle).count();
    let done = state.sessions.iter().filter(|s| s.status == SessionStatus::Done).count();
    let error = state.sessions.iter().filter(|s| s.status == SessionStatus::Error).count();

    // Build status breakdown
    let status_line = {
        let mut parts = Vec::new();
        if in_progress > 0 {
            parts.push(Span::raw(format!("⚡{} ", in_progress)).set_style(Style::default().fg(colors::GREEN)));
        }
        if pending > 0 {
            parts.push(Span::raw(format!("○{} ", pending)).set_style(Style::default().fg(colors::YELLOW)));
        }
        if idle > 0 {
            parts.push(Span::raw(format!("·{} ", idle)).set_style(Style::default().fg(colors::SECONDARY)));
        }
        if done > 0 {
            parts.push(Span::raw(format!("✓{} ", done)).set_style(Style::default().fg(colors::CYAN)));
        }
        if error > 0 {
            parts.push(Span::raw(format!("✗{} ", error)).set_style(Style::default().fg(colors::RED)));
        }
        if parts.is_empty() {
            parts.push(Span::raw("— ").set_style(Style::default().fg(colors::SECONDARY)));
        }
        parts
    };

    let token_str = format_tokens(state.aggregated_tokens.today_tokens);

    let line = Line::from(vec![
        Span::raw("  claude-watch  ")
            .set_style(Style::default().fg(colors::ACCENT).add_modifier(Modifier::BOLD)),
        Span::raw(&clock).set_style(Style::default().fg(colors::PRIMARY)),
        Span::raw("  ").set_style(Style::default()),
        Span::raw(&countdown).set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw("  ").set_style(Style::default()),
        Span::raw(&token_str).set_style(Style::default().fg(colors::YELLOW)),
        Span::raw(" today").set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw("     ").set_style(Style::default().fg(colors::SURFACE)),
    ]
    .into_iter()
    .chain(status_line.into_iter())
    .collect::<Vec<_>>());

    f.render_widget(
        Paragraph::new(line).set_style(Style::default().bg(colors::SURFACE)),
        inner,
    );
}

// Tree item for the tmux panel — each entry knows its depth and type
enum TmuxTreeEntry {
    Session { name: String },
    Window { session: String, name: String, index: String },
    Pane {
        pane_key: PaneKey,
        pane_label: String,    // tmux window name
        repo: Option<String>,  // project name if session matched
        branch: Option<String>, // git branch if session matched
    },
}

fn render_tmux_panel(f: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::new()
        .title(" ⎔ tmux ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colors::BORDER))
        .title_style(Style::default().fg(colors::CYAN).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(colors::BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(ref ws) = state.tmux_workspace else {
        let msg = Paragraph::new("  tmux: not running")
            .set_style(Style::default().fg(colors::SECONDARY));
        f.render_widget(msg, inner);
        return;
    };

    if ws.sessions.is_empty() {
        let msg = Paragraph::new("  no tmux sessions")
            .set_style(Style::default().fg(colors::SECONDARY));
        f.render_widget(msg, inner);
        return;
    }

    // ── Build flat list of agent pane entries (for linear navigation index) ──
    // Only include panes that are running coding agents
    let all_panes: Vec<PaneKey> = ws
        .sessions
        .iter()
        .flat_map(|s| {
            s.panes
                .iter()
                .filter(|p| is_coding_agent(p))
                .map(|p| PaneKey {
                    session: s.name.clone(),
                    window: p.window_name.clone(),
                    pane: p.pane_id.clone(),
                })
                .collect::<Vec<_>>()
        })
        .collect();

    let total_panes = all_panes.len();
    let selected_pane_idx = state.selected_pane_idx.min(total_panes.saturating_sub(1));
    let selected_pane_key = all_panes.get(selected_pane_idx).cloned();

    // ── Build tree entries (only sessions/windows/panes with agents) ─────────
    let mut tree: Vec<TmuxTreeEntry> = Vec::new();
    let mut pane_flat_index: Vec<PaneKey> = Vec::new();

    for session in &ws.sessions {
        // Collect agent panes for this session
        let agent_panes: Vec<&TmuxPane> = session.panes.iter().filter(|p| is_coding_agent(p)).collect();

        // Skip session if no agent panes
        if agent_panes.is_empty() {
            continue;
        }

        tree.push(TmuxTreeEntry::Session {
            name: session.name.clone(),
        });

        // Group panes by window (preserving order)
        let mut panes_by_window: Vec<(String, String, Vec<&TmuxPane>)> = Vec::new();
        let mut seen_windows: Vec<String> = Vec::new();
        for pane in &agent_panes {
            if !seen_windows.contains(&pane.window_name) {
                seen_windows.push(pane.window_name.clone());
                panes_by_window.push((pane.window_name.clone(), pane.window_index.clone(), Vec::new()));
            }
            let group = panes_by_window
                .iter_mut()
                .find(|(name, _, _)| name == &pane.window_name);
            if let Some((_, _, panes)) = group {
                panes.push(pane);
            }
        }

        for (window_name, window_index, window_panes) in panes_by_window.into_iter() {
            // Only add window header if multiple windows exist
            if seen_windows.len() > 1 {
                tree.push(TmuxTreeEntry::Window {
                    session: session.name.clone(),
                    name: window_name.clone(),
                    index: window_index.clone(),
                });
            }

            // Panes under this window
            for pane in window_panes.iter() {
                let pane_key = PaneKey {
                    session: session.name.clone(),
                    window: window_name.clone(),
                    pane: pane.pane_id.clone(),
                };
                pane_flat_index.push(pane_key.clone());

                // Look up Claude session via session_by_pane
                let (repo, branch) = state
                    .session_by_pane
                    .get(&pane_key)
                    .and_then(|&idx| state.sessions.get(idx))
                    .map(|s| {
                        // Derive real project name from cwd: /Users/tuannvm/project/cli/claudeboard → claudeboard
                        let repo = std::path::Path::new(&s.cwd)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .map(|n| n.to_string())
                            .unwrap_or_else(|| s.project.clone());
                        (Some(repo), s.git_branch.clone())
                    })
                    .unwrap_or((None, None));

                // Build pane label: use running_cmd (version like "2.1.89" or "claude-watch") as primary,
                // title as secondary info in parentheses if different
                let pane_label = if let Some(ref cmd) = pane.running_cmd {
                    if let Some(ref title) = pane.pane_title {
                        if title.as_str() != pane.window_name && !title.contains(cmd) {
                            format!("{} ({})", cmd, title)
                        } else {
                            cmd.clone()
                        }
                    } else {
                        cmd.clone()
                    }
                } else {
                    pane.pane_title.clone().unwrap_or_else(|| pane.window_name.clone())
                };

                tree.push(TmuxTreeEntry::Pane {
                    pane_key,
                    pane_label,
                    repo,
                    branch,
                });
            }
        }
    }

    let total_lines = tree.len();

    // ── Determine visible window ─────────────────────────────────────────────
    // Map selected_pane_idx (in all_panes) to an index in pane_flat_index
    let pane_line_idx: Option<usize> = selected_pane_key.as_ref().and_then(|pk| {
        pane_flat_index.iter().position(|k| k == pk)
    });

    // Find which tree line corresponds to the selected pane
    let selected_tree_idx = pane_line_idx.map(|pfi| {
        tree.iter()
            .enumerate()
            .filter_map(|(idx, entry)| {
                if let TmuxTreeEntry::Pane { pane_key, .. } = entry {
                    if pane_flat_index.iter().position(|k| k == pane_key) == Some(pfi) {
                        return Some(idx);
                    }
                }
                None
            })
            .next()
    }).flatten();

    // ── Render with viewport ─────────────────────────────────────────────────
    let header_lines = 1; // title bar counts as rendered
    let max_lines = (inner.height as usize).saturating_sub(header_lines);

    let start_idx = if let Some(idx) = selected_tree_idx {
        if idx >= max_lines {
            idx.saturating_sub(max_lines - 1)
        } else {
            0
        }
    } else {
        0
    };
    let end_idx = (start_idx + max_lines).min(total_lines);

    let mut lines: Vec<Line> = Vec::new();

    for idx in start_idx..end_idx {
        let entry = &tree[idx];
        match entry {
            TmuxTreeEntry::Session { name } => {
                let is_ancestor_of_selected = selected_pane_key
                    .as_ref()
                    .map(|pk| pk.session == *name)
                    .unwrap_or(false);
                let style = if is_ancestor_of_selected {
                    Style::default()
                        .fg(colors::ACCENT)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default()
                        .fg(colors::SECONDARY)
                        .add_modifier(Modifier::BOLD)
                };
                lines.push(Line::from(vec![
                    Span::raw("▸ ").set_style(style),
                    Span::raw(name).set_style(style),
                ]));
            }

            TmuxTreeEntry::Window { session, name, index } => {
                let is_ancestor_of_selected = selected_pane_key
                    .as_ref()
                    .map(|pk| pk.session == *session && pk.window == *name)
                    .unwrap_or(false);
                let style = if is_ancestor_of_selected {
                    Style::default().fg(colors::CYAN)
                } else {
                    Style::default().fg(colors::SECONDARY)
                };
                // Improved tree branch: show proper nesting with window index
                let line = format!("  ├──[{}] {}", index, name);
                lines.push(Line::from(vec![Span::raw(line).set_style(style)]));
            }

            TmuxTreeEntry::Pane {
                pane_key,
                pane_label,
                repo,
                branch,
            } => {
                let is_selected = selected_pane_key.as_ref() == Some(pane_key);
                let _pane_idx_in_tree = idx;

                // Is this the last pane in its window?
                // Look ahead to find the next entry that is a Window or Session
                let is_last_in_window = !tree[idx + 1..].iter().any(|e| {
                    matches!(e, TmuxTreeEntry::Pane { .. })
                });

                // Use proper tree branch characters
                let tree_char = if is_last_in_window { "  │   └──" } else { "  │   ├──" };
                // Show ● green if pane has a matched Claude session, ○ dimmed otherwise
                let has_match = repo.is_some();
                let marker = if has_match { "●" } else { "○" };
                let marker_color = if has_match { colors::GREEN } else { colors::SECONDARY };

                let bg = if is_selected { colors::SURFACE } else { colors::BG };
                let text_color = if is_selected {
                    colors::PRIMARY
                } else if has_match {
                    colors::GREEN
                } else {
                    colors::SECONDARY
                };

                // Build the label: show repo/branch whenever session_by_pane matched (regardless of is_claude)
                // is_claude only affects bullet color (● vs ○)
                let label = if let Some(r) = &repo {
                    if let Some(b) = &branch {
                        format!("[{}] {} - {}", pane_label, r, b)
                    } else {
                        format!("[{}] {} - worktree", pane_label, r)
                    }
                } else {
                    format!("[{}]", pane_label)
                };

                lines.push(Line::from(vec![
                    Span::raw(tree_char).set_style(Style::default().fg(colors::BORDER).bg(bg)),
                    Span::raw(marker).set_style(Style::default().fg(marker_color).bg(bg)),
                    Span::raw(" ").set_style(Style::default().bg(bg)),
                    Span::raw(label).set_style(Style::default().fg(text_color).bg(bg)),
                ]));
            }
        }
    }

    // Scrollbar
    if total_lines > max_lines && inner.height >= 3 {
        let scroll_pct = selected_tree_idx.map(|i| i as f32 / (total_lines - 1) as f32).unwrap_or(0.0);
        let scroll_y = (scroll_pct * (inner.height - 2) as f32) as u16;
        f.render_widget(
            Paragraph::new("▐").set_style(Style::default().fg(colors::ACCENT)),
            Rect::new(
                inner.x + inner.width - 1,
                inner.y + 1 + scroll_y.min(inner.height.saturating_sub(3)),
                1,
                1,
            ),
        );
    }

    let para = Paragraph::new(lines)
        .set_style(Style::default().bg(colors::BG));
    f.render_widget(para, inner);
}

// ============================================================================
// Tmux Pane Capture
// ============================================================================

/// Capture the visible content of a tmux pane using capture-pane
fn capture_pane_content(socket: &Option<String>, session: &str, window_idx: &str, pane: &str) -> Vec<String> {
    let socket_args: Vec<&str> = match socket {
        Some(s) => ["-L", s.as_str()].to_vec(),
        None => vec![],
    };

    // tmux capture-pane requires session:window.pane format
    let target = format!("{}:{}.{}", session, window_idx, pane);

    let output = {
        let mut cmd = std::process::Command::new("tmux");
        for arg in &socket_args {
            cmd.arg(arg);
        }
        cmd.args(["capture-pane", "-t", &target, "-p", "-S", "-50"]);
        cmd.output()
    };

    match output {
        Ok(o) if o.status.success() => {
            String::from_utf8_lossy(&o.stdout)
                .lines()
                .map(|s| s.to_string())
                .collect()
        }
        _ => vec![],
    }
}

// ============================================================================
// Rendering - Live Pane Panel (Right Top)
// ============================================================================

fn render_live_pane(f: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::new()
        .title(" live ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colors::BORDER))
        .title_style(Style::default().fg(colors::GREEN).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(colors::BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Get selected pane info
    let (pane_key, pane_info): (PaneKey, Option<TmuxPane>) = if let Some(ref ws) = state.tmux_workspace {
        let all_agent_panes: Vec<(PaneKey, TmuxPane)> = ws
            .sessions
            .iter()
            .flat_map(|s| {
                s.panes
                    .iter()
                    .filter(|p| is_coding_agent(p))
                    .map(|p| {
                        let key = PaneKey {
                            session: s.name.clone(),
                            window: p.window_name.clone(),
                            pane: p.pane_id.clone(),
                        };
                        (key, p.clone())
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        let selected = state.selected_pane_idx.min(all_agent_panes.len().saturating_sub(1));
        all_agent_panes.into_iter().nth(selected)
            .map(|(k, p)| (k, Some(p)))
            .unwrap_or_else(|| (PaneKey { session: String::new(), window: String::new(), pane: String::new() }, None))
    } else {
        (PaneKey { session: String::new(), window: String::new(), pane: String::new() }, None)
    };

    // Live poll indicator
    let poll_str = if pane_info.is_some() {
        let cd = state.refresh_countdown;
        if cd > 3 { "⚡ polling".to_string() } else { format!("↻ {}s", cd) }
    } else {
        String::new()
    };

    match pane_info {
        Some(pane) => {
            // Capture live pane content via tmux capture-pane
            let lines = capture_pane_content(&state.tmux_socket, &pane_key.session, &pane.window_index, &pane_key.pane);

            // Header with pane info
            let pane_label = pane.running_cmd.as_deref().unwrap_or("—");
            let status_color = if pane.pane_dead { colors::RED } else { colors::GREEN };
            let status_str = if pane.pane_dead { "dead ✗" } else { "active ⚡" };

            let header = vec![
                Span::raw(status_str).set_style(Style::default().fg(status_color)),
                Span::raw(" ").set_style(Style::default()),
                Span::raw(pane_label).set_style(Style::default().fg(colors::ACCENT).add_modifier(Modifier::BOLD)),
                Span::raw("  ").set_style(Style::default()),
                Span::raw(&pane_key.session).set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw("/").set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw(&pane_key.window).set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw("/").set_style(Style::default().fg(colors::CYAN)),
                Span::raw(&pane_key.pane).set_style(Style::default().fg(colors::CYAN)),
                Span::raw("  ").set_style(Style::default()),
                Span::raw(&poll_str).set_style(Style::default().fg(if pane.pane_dead { colors::SECONDARY } else { colors::GREEN })),
            ];

            let mut all_lines: Vec<Line> = vec![Line::from(header)];

            if pane.pane_dead {
                all_lines.push(Line::from(vec![Span::raw("  [pane is dead]").set_style(Style::default().fg(colors::RED))]));
            } else if lines.is_empty() {
                all_lines.push(Line::from(vec![Span::raw("  [pane content unavailable]").set_style(Style::default().fg(colors::SECONDARY))]));
            } else {
                // Show last N lines of pane content, truncated to fit width
                let max_lines = (inner.height as usize).saturating_sub(3).max(1);
                let max_chars = (inner.width as usize).saturating_sub(4).max(10);
                for line_text in lines.iter().rev().take(max_lines).rev() {
                    let display: String = if line_text.chars().count() > max_chars {
                        let mut chars = 0;
                        let end = line_text.char_indices()
                            .take_while(|(_, _c)| {
                                chars += 1;
                                chars <= max_chars.saturating_sub(3)
                            })
                            .last()
                            .map(|(i, _)| i + 1)
                            .unwrap_or(0);
                        format!("{}...", &line_text[..end])
                    } else {
                        line_text.clone()
                    };
                    all_lines.push(Line::from(vec![
                        Span::raw("  ").set_style(Style::default().fg(colors::SECONDARY)),
                        Span::raw(display).set_style(Style::default().fg(colors::PRIMARY)),
                    ]));
                }
            }

            let para = Paragraph::new(all_lines).set_style(Style::default().bg(colors::BG));
            f.render_widget(para, inner);
        }
        None => {
            let lines = vec![
                Line::from(vec![Span::raw("  no pane selected").set_style(Style::default().fg(colors::SECONDARY))]),
                Line::from(vec![Span::raw("  use j/k to navigate").set_style(Style::default().fg(colors::SECONDARY))]),
            ];
            let para = Paragraph::new(lines).set_style(Style::default().bg(colors::BG));
            f.render_widget(para, inner);
        }
    }
}

// ============================================================================
// Rendering - Session Metadata Panel (Right Bottom)
// ============================================================================

fn render_session_metadata(f: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::new()
        .title(" session ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colors::BORDER))
        .title_style(Style::default().fg(colors::YELLOW).add_modifier(Modifier::BOLD))
        .style(Style::default().bg(colors::BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Get selected pane info and corresponding session
    let (pane_key, pane_info): (PaneKey, Option<TmuxPane>) = if let Some(ref ws) = state.tmux_workspace {
        let all_agent_panes: Vec<(PaneKey, TmuxPane)> = ws
            .sessions
            .iter()
            .flat_map(|s| {
                s.panes
                    .iter()
                    .filter(|p| is_coding_agent(p))
                    .map(|p| {
                        let key = PaneKey {
                            session: s.name.clone(),
                            window: p.window_name.clone(),
                            pane: p.pane_id.clone(),
                        };
                        (key, p.clone())
                    })
                    .collect::<Vec<_>>()
            })
            .collect();

        let selected = state.selected_pane_idx.min(all_agent_panes.len().saturating_sub(1));
        all_agent_panes.into_iter().nth(selected)
            .map(|(k, p)| (k, Some(p)))
            .unwrap_or_else(|| (PaneKey { session: String::new(), window: String::new(), pane: String::new() }, None))
    } else {
        (PaneKey { session: String::new(), window: String::new(), pane: String::new() }, None)
    };

    let session = state.session_by_pane.get(&pane_key).and_then(|&i| state.sessions.get(i));

    match session {
        Some(session) => {
            let status_icon = session_status_icon(session.status);
            let status_color = session_status_color(session.status);
            let branch_str = session.git_branch.as_ref().map(|b| format!("@{}", b)).unwrap_or_default();
            let idle_min = (Utc::now() - session.last_active).num_minutes();
            let idle_str = if idle_min < 1 { "just now".to_string() } else { format!("{}m ago", idle_min) };

            let mut lines: Vec<Line> = vec![
                Line::from(vec![
                    Span::raw(status_icon).set_style(Style::default().fg(status_color)),
                    Span::raw(" ").set_style(Style::default()),
                    Span::raw(&session.project).set_style(Style::default().fg(colors::ACCENT).add_modifier(Modifier::BOLD)),
                    Span::raw(" ").set_style(Style::default()),
                    Span::raw(&branch_str).set_style(Style::default().fg(colors::PURPLE)),
                ]),
                Line::from(vec![
                    Span::raw("  id: ").set_style(Style::default().fg(colors::SECONDARY)),
                    Span::raw(&session.id).set_style(Style::default().fg(colors::CYAN)),
                    Span::raw(" · last: ").set_style(Style::default().fg(colors::SECONDARY)),
                    Span::raw(&idle_str).set_style(Style::default().fg(colors::YELLOW)),
                ]),
            ];

            // Truncate cwd
            let cwd_display = if session.cwd.chars().count() > 35 {
                format!("...{}", truncate_from_end(&session.cwd, 32))
            } else {
                session.cwd.clone()
            };
            lines.push(Line::from(vec![
                Span::raw("  cwd: ").set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw(&cwd_display).set_style(Style::default().fg(colors::PRIMARY)),
            ]));

            lines.push(Line::from(vec![]));
            lines.push(Line::from(vec![Span::raw(" msgs ").set_style(Style::default().fg(colors::SECONDARY).add_modifier(Modifier::BOLD))]));
            lines.push(Line::from(vec![
                Span::raw("  ● asst: ").set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw(format!("{}", session.message_counts.assistant)).set_style(Style::default().fg(colors::GREEN)),
                Span::raw("  ● user: ").set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw(format!("{}", session.message_counts.user)).set_style(Style::default().fg(colors::CYAN)),
                Span::raw("  ● sys: ").set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw(format!("{}", session.message_counts.system)).set_style(Style::default().fg(colors::PURPLE)),
            ]));

            lines.push(Line::from(vec![]));
            lines.push(Line::from(vec![Span::raw(" tokens ").set_style(Style::default().fg(colors::SECONDARY).add_modifier(Modifier::BOLD))]));

            let total = session.token_counts.total();
            if total > 0 {
                // Draw token breakdown bar
                let bar_width = (inner.width.saturating_sub(4) as u64).max(1);
                let draw_bar = |tokens: u64, color: Color, label: &str| {
                    let bar_len = ((tokens as f64 / total as f64) * bar_width as f64) as u16;
                    let bar_str = "█".repeat(bar_len as usize);
                    vec![
                        Span::raw(format!("{:>6} ", format_tokens(tokens))).set_style(Style::default().fg(colors::SECONDARY)),
                        Span::raw(bar_str).set_style(Style::default().fg(color)),
                        Span::raw(format!(" {:>4.0}% {}", (tokens as f64 / total as f64 * 100.0), label)).set_style(Style::default().fg(colors::SECONDARY)),
                    ]
                };
                lines.push(Line::from(draw_bar(session.token_counts.input_tokens, colors::CYAN, "in")));
                lines.push(Line::from(draw_bar(session.token_counts.output_tokens, colors::YELLOW, "out")));
                let cache_total = session.token_counts.cache_read_input_tokens + session.token_counts.cache_creation_input_tokens;
                lines.push(Line::from(draw_bar(cache_total, colors::PURPLE, "cache")));
            } else {
                lines.push(Line::from(vec![Span::raw("  — no token data").set_style(Style::default().fg(colors::SECONDARY))]));
            }

            lines.push(Line::from(vec![]));
            lines.push(Line::from(vec![Span::raw(" queue ").set_style(Style::default().fg(colors::SECONDARY).add_modifier(Modifier::BOLD))]));

            if session.queue_ops.is_empty() {
                let idle_m = (Utc::now() - session.last_active).num_minutes();
                if idle_m > 60 {
                    lines.push(Line::from(vec![Span::raw("  — idle >1h, no queue ops").set_style(Style::default().fg(colors::SECONDARY))]));
                } else if session.message_counts.assistant == 0 && session.message_counts.user == 0 {
                    lines.push(Line::from(vec![Span::raw("  — new session, no ops yet").set_style(Style::default().fg(colors::SECONDARY))]));
                } else {
                    lines.push(Line::from(vec![Span::raw("  — no operations").set_style(Style::default().fg(colors::SECONDARY))]));
                }
            } else {
                for op in session.queue_ops.iter().rev().take(6) {
                    let icon = queue_op_icon(&op.operation);
                    let icon_color = queue_op_color(&op.operation);
                    let time_str = op.timestamp.format("%H:%M:%S").to_string();
                    lines.push(Line::from(vec![
                        Span::raw("  ").set_style(Style::default()),
                        Span::raw(icon).set_style(Style::default().fg(icon_color)),
                        Span::raw(" [").set_style(Style::default().fg(colors::SECONDARY)),
                        Span::raw(time_str).set_style(Style::default().fg(colors::CYAN)),
                        Span::raw("] ").set_style(Style::default().fg(colors::SECONDARY)),
                        Span::raw(&op.operation).set_style(Style::default().fg(colors::PRIMARY)),
                    ]));
                }
            }

            let remaining = inner.height.saturating_sub(lines.len() as u16);
            if remaining > 1 {
                for _ in 0..(remaining - 1) as usize {
                    lines.push(Line::from(vec![]));
                }
            }

            let para = Paragraph::new(lines).set_style(Style::default().bg(colors::BG));
            f.render_widget(para, inner);
        }
        None => {
            // No JSONL session matched — show pane metadata
            let mut lines = vec![
                Line::from(vec![Span::raw("  no JSONL session matched").set_style(Style::default().fg(colors::YELLOW))]),
            ];
            if let Some(ref pane) = pane_info {
                lines.push(Line::from(vec![
                    Span::raw("  cwd: ").set_style(Style::default().fg(colors::SECONDARY)),
                    {
                        let cwd = if pane.cwd.len() > 35 {
                            format!("...{}", truncate_from_end(&pane.cwd, 32))
                        } else {
                            pane.cwd.clone()
                        };
                        Span::raw(cwd).set_style(Style::default().fg(colors::PRIMARY))
                    },
                ]));
                lines.push(Line::from(vec![
                    Span::raw("  cmd: ").set_style(Style::default().fg(colors::SECONDARY)),
                    Span::raw(pane.running_cmd.as_deref().unwrap_or("—")).set_style(Style::default().fg(colors::GREEN)),
                ]));
            }
            lines.push(Line::from(vec![]));
            lines.push(Line::from(vec![Span::raw("  token/queue data requires JSONL").set_style(Style::default().fg(colors::SECONDARY))]));
            lines.push(Line::from(vec![Span::raw("  match (cwd or project name)").set_style(Style::default().fg(colors::SECONDARY))]));

            let para = Paragraph::new(lines).set_style(Style::default().bg(colors::BG));
            f.render_widget(para, inner);
        }
    }
}

fn render_status_bar(f: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::new()
        .borders(Borders::NONE)
        .style(Style::default().bg(colors::SURFACE));
    let inner = block.inner(area);
    f.render_widget(block, area);

    let pane_count = state.agent_pane_count;
    let token_str = format_tokens(state.aggregated_tokens.total_tokens);

    // Count sessions by status
    let in_progress = state.sessions.iter().filter(|s| s.status == SessionStatus::InProgress).count();
    let pending = state.sessions.iter().filter(|s| s.status == SessionStatus::Pending).count();
    let idle = state.sessions.iter().filter(|s| s.status == SessionStatus::Idle).count();
    let done = state.sessions.iter().filter(|s| s.status == SessionStatus::Done).count();
    let error = state.sessions.iter().filter(|s| s.status == SessionStatus::Error).count();

    let left = vec![
        Span::raw("q:quit").set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw("  ")
            .set_style(Style::default().fg(colors::SURFACE)),
        Span::raw("↑↓:navigate")
            .set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw("  ")
            .set_style(Style::default().fg(colors::SURFACE)),
        Span::raw("r:refresh")
            .set_style(Style::default().fg(colors::SECONDARY)),
    ];

    // Build status breakdown string
    let status_parts = {
        let mut parts = Vec::new();
        if in_progress > 0 { parts.push(format!("⚡{}", in_progress)); }
        if pending > 0 { parts.push(format!("○{}", pending)); }
        if idle > 0 { parts.push(format!("·{}", idle)); }
        if done > 0 { parts.push(format!("✓{}", done)); }
        if error > 0 { parts.push(format!("✗{}", error)); }
        if parts.is_empty() { parts.push("—".to_string()); }
        parts.join(" ")
    };

    let right = vec![
        Span::raw(format!("{} agent panes", pane_count))
            .set_style(Style::default().fg(colors::CYAN)),
        Span::raw(" · ").set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw(&status_parts).set_style(Style::default().fg(colors::PRIMARY)),
        Span::raw(" · ").set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw(&token_str)
            .set_style(Style::default().fg(colors::YELLOW)),
        Span::raw(" total")
            .set_style(Style::default().fg(colors::SECONDARY)),
    ];

    let line = Line::from(vec![
        left[0].clone(),
        left[1].clone(),
        left[2].clone(),
        left[3].clone(),
        left[4].clone(),
        Span::raw("                    ")
            .set_style(Style::default().fg(colors::SURFACE)),
        right[0].clone(),
        right[1].clone(),
        right[2].clone(),
        right[3].clone(),
        right[4].clone(),
        right[5].clone(),
    ]);

    f.render_widget(
        Paragraph::new(line).set_style(Style::default().bg(colors::SURFACE)),
        inner,
    );
}

fn render(f: &mut Frame, area: Rect, state: &AppState) {
    f.render_widget(
        Paragraph::new("").set_style(Style::default().bg(colors::BG)),
        area,
    );

    // Header (1 line)
    render_header(f, Rect::new(area.x, area.y, area.width, 1), state);

    // Body: left 40% (TMUX) + right 60% (session queue top, tokens bottom)
    let body_height = area.height.saturating_sub(2);

    let chunks = Layout::default()
        .direction(Direction::Horizontal)
        .constraints([
            Constraint::Percentage(40),
            Constraint::Percentage(60),
        ])
        .split(Rect::new(area.x, area.y + 1, area.width, body_height));

    // Left: TMUX panel (full height)
    render_tmux_panel(f, chunks[0], state);

    // Right: live pane (top 60%) + session metadata (bottom 40%)
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([
            Constraint::Percentage(60),
            Constraint::Percentage(40),
        ])
        .split(chunks[1]);

    render_live_pane(f, right_chunks[0], state);
    render_session_metadata(f, right_chunks[1], state);

    // Status bar (1 line)
    render_status_bar(
        f,
        Rect::new(area.x, area.y + area.height - 1, area.width, 1),
        state,
    );
}

// ============================================================================
// Main
// ============================================================================

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    crossterm::execute!(std::io::stderr(), EnterAlternateScreen)?;
    enable_raw_mode()?;
    let backend = ratatui::backend::CrosstermBackend::new(std::io::stderr());
    let mut terminal = ratatui::Terminal::new(backend)?;
    terminal.clear()?;

    let state: Arc<RwLock<AppState>> = Arc::new(RwLock::new(AppState {
        tmux_workspace: None,
        selected_pane_idx: 0,
        agent_pane_count: 0,
        sessions: Vec::new(),
        session_by_pane: HashMap::new(),
        aggregated_tokens: AggregatedTokens::default(),
        refresh_countdown: args.refresh_interval,
        tmux_socket: args.tmux_socket.clone(),
    }));

    let args_clone = args.clone();
    let state_clone = state.clone();

    // Background refresh loop
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(1));
        let refresh_interval = args_clone.refresh_interval;
        let mut countdown = refresh_interval;
        let tmux_socket = args_clone.tmux_socket.clone();

        loop {
            ticker.tick().await;

            // Sync countdown from state (manual refresh via 'r')
            {
                let sc = state_clone.read().refresh_countdown;
                if sc == 0 {
                    state_clone.write().refresh_countdown = refresh_interval;
                    countdown = 0;
                } else {
                    countdown = countdown.saturating_sub(1);
                }
            }

            if countdown == 0 {
                countdown = refresh_interval;

                let sessions = tokio::task::spawn_blocking(move || {
                    scan_all_sessions(7) // only last 7 days
                })
                .await
                .unwrap_or_default();

                let tokens = tokio::task::spawn_blocking(parse_token_logs).await.unwrap_or_default();

                let tmux_socket_clone = tmux_socket.clone();
                let tmux_ws = tokio::task::spawn_blocking(move || {
                    parse_tmux_workspace(&tmux_socket_clone)
                })
                .await
                .unwrap_or(None);

                // Build session_by_pane map: match Claude Code session to tmux pane
                // Strategy 1: Match by session.cwd (the real cwd from JSONL) if it has depth >= 4
                // Strategy 2: Match by project name if pane.cwd ends with the session's project dir name
                // The project name is more reliable than cwd because JSONL cwd is often ~/.claude
                let mut session_by_pane = HashMap::new();
                let mut agent_pane_count = 0;
                if let Some(ref ws) = tmux_ws {
                    // First pass: count agent panes (across all tmux sessions once)
                    for tmux_session in &ws.sessions {
                        for pane in &tmux_session.panes {
                            if is_coding_agent(pane) {
                                agent_pane_count += 1;
                            }
                        }
                    }
                    // Second pass: match panes to sessions
                    for (sess_idx, session) in sessions.iter().enumerate() {
                        for tmux_session in &ws.sessions {
                            for pane in &tmux_session.panes {
                                let pane_cwd = &pane.cwd;
                                let session_cwd = &session.cwd;
                                let session_project = &session.project;

                                let matched = {
                                    // Strategy 1: cwd-based matching (requires depth >= 4)
                                    let session_path = std::path::PathBuf::from(session_cwd);
                                    let session_depth = session_path.components().count();
                                    let cwd_match = session_depth >= 4
                                        && pane_cwd.starts_with(session_cwd);

                                    // Strategy 2: project-name matching (pane cwd ends with project dir name)
                                    // This is more reliable when session.cwd is ~/.claude or empty
                                    let project_name = std::path::Path::new(session_project)
                                        .file_name()
                                        .and_then(|n| n.to_str())
                                        .unwrap_or(session_project);
                                    let project_match = pane_cwd.ends_with(project_name)
                                        || pane_cwd.ends_with(session_project);

                                    cwd_match || project_match
                                };

                                if matched {
                                    let key = PaneKey {
                                        session: tmux_session.name.clone(),
                                        window: pane.window_name.clone(),
                                        pane: pane.pane_id.clone(),
                                    };
                                    // First match wins — don't overwrite if already matched
                                    session_by_pane.entry(key).or_insert(sess_idx);
                                }
                            }
                        }
                    }
                }

                let selected = state_clone.read().selected_pane_idx;

                let mut s = state_clone.write();
                s.sessions = sessions;
                s.aggregated_tokens = tokens;
                s.tmux_workspace = tmux_ws;
                s.session_by_pane = session_by_pane;
                s.agent_pane_count = agent_pane_count;
                // Clamp selected pane index to available agent panes
                s.selected_pane_idx = selected.min(agent_pane_count.saturating_sub(1));
            }

            state_clone.write().refresh_countdown = countdown;
        }
    });

    let state_clone2 = state.clone();

    // Input loop
    loop {
        terminal.draw(|f| {
            let state_guard = state.read();
            render(f, f.size(), &state_guard);
        })?;

        if crossterm::event::poll(Duration::from_millis(100))? {
            if let crossterm::event::Event::Key(key) = crossterm::event::read()? {
                if key.kind == KeyEventKind::Press {
                    match key.code {
                        KeyCode::Char('q') | KeyCode::Esc => {
                            disable_raw_mode()?;
                            crossterm::execute!(std::io::stderr(), LeaveAlternateScreen)?;
                            return Ok(());
                        }
                        KeyCode::Char('r') => {
                            state_clone2.write().refresh_countdown = 0;
                        }
                        KeyCode::Up | KeyCode::Char('k') => {
                            let mut s = state_clone2.write();
                            if s.selected_pane_idx > 0 {
                                s.selected_pane_idx -= 1;
                            }
                        }
                        KeyCode::Down | KeyCode::Char('j') => {
                            let mut s = state_clone2.write();
                            if s.selected_pane_idx < s.agent_pane_count.saturating_sub(1) {
                                s.selected_pane_idx += 1;
                            }
                        }
                        KeyCode::Char('g') => {
                            state_clone2.write().selected_pane_idx = 0;
                        }
                        KeyCode::Char('G') => {
                            let mut s = state_clone2.write();
                            s.selected_pane_idx = s.agent_pane_count.saturating_sub(1);
                        }
                        _ => {}
                    }
                }
            }
        }
    }
}
