use chrono::{DateTime, TimeDelta, Utc};
use std::cmp::Reverse;
use clap::Parser;
use crossterm::event::{KeyCode, KeyEventKind};
use crossterm::terminal::{
    EnterAlternateScreen, LeaveAlternateScreen, disable_raw_mode, enable_raw_mode,
};
use parking_lot::RwLock;
use ratatui::Frame;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Styled;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use serde::Deserialize;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
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
    s.chars()
        .rev()
        .take(max_chars)
        .collect::<Vec<_>>()
        .into_iter()
        .rev()
        .collect()
}

// ============================================================================
// CLI
// ============================================================================

#[derive(Parser, Debug, Clone)]
#[command(name = "claudeboard")]
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

    pub const BG: Color = Color::Rgb(0x1a, 0x1b, 0x26); // dark navy
    pub const SURFACE: Color = Color::Rgb(0x24, 0x28, 0x3b); // panel bg
    pub const PRIMARY: Color = Color::Rgb(0xc0, 0xca, 0xf5); // main text
    pub const SECONDARY: Color = Color::Rgb(0x56, 0x5f, 0x89); // dim text
    pub const ACCENT: Color = Color::Rgb(0x7a, 0xa2, 0xf7); // blue highlight
    pub const GREEN: Color = Color::Rgb(0x9e, 0xce, 0x6a); // running/active
    pub const YELLOW: Color = Color::Rgb(0xe0, 0xaf, 0x68); // pending/idle
    pub const RED: Color = Color::Rgb(0xf7, 0x76, 0x8e); // failed/error
    pub const CYAN: Color = Color::Rgb(0x73, 0xda, 0xca); // info
    pub const PURPLE: Color = Color::Rgb(0xbb, 0x9a, 0xf7); // special
    pub const BORDER: Color = Color::Rgb(0x41, 0x48, 0x68); // borders
}

// ============================================================================
// Data Models
// ============================================================================

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
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
    pub model: Option<String>,
    pub last_user_msg: Option<DateTime<Utc>>,
    pub last_asst_msg: Option<DateTime<Utc>>,
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
    // Total (all-time) breakdown
    pub total_tokens: u64,
    pub total_cost: f64,
    pub total_input: u64,
    pub total_output: u64,
    pub total_cache_read: u64,
    pub total_cache_write: u64,
    // Today's breakdown
    pub today_tokens: u64,
    pub today_cost: f64,
    pub today_input: u64,
    pub today_output: u64,
    pub today_cache_read: u64,
    pub today_cache_write: u64,
    // Entries for model breakdown display
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

/// Check if a pane is running a coding agent (Claude Code, Codex, or Gemini).
/// Primary signal is running_cmd; for wrapped launches (bash/node/npx/etc) fall back to pane_title.
fn is_coding_agent(pane: &TmuxPane) -> bool {
    // Dead panes are not running anything
    if pane.pane_dead {
        return false;
    }

    let title = pane.pane_title.as_deref().unwrap_or_default().to_lowercase();
    let title_has_agent = has_word_token(&title, "claude")
        || has_word_token(&title, "codex")
        || has_word_token(&title, "gemini")
        || has_word_token(&title, "anthropic");

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

        if is_agent_cmd {
            return true;
        }

        if is_version {
            return true;
        }

        // Wrapped launches often report shell/runtime in running_cmd.
        let is_wrapper_cmd = matches!(
            cmd_basename.as_str(),
            "bash"
                | "zsh"
                | "sh"
                | "fish"
                | "node"
                | "python"
                | "python3"
                | "env"
                | "bun"
                | "npm"
                | "npx"
                | "pnpm"
                | "yarn"
                | "uvx"
                | "pipx"
        );
        if is_wrapper_cmd && title_has_agent {
            return true;
        }
    }

    false
}

fn has_word_token(text: &str, token: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .any(|t| t.eq_ignore_ascii_case(token))
}

fn is_likely_claude_pane(pane: &TmuxPane) -> bool {
    let cmd_opt = pane.running_cmd.as_deref();
    let cmd_lower = cmd_opt.unwrap_or_default().to_lowercase();

    let cmd_basename = std::path::Path::new(&cmd_lower)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&cmd_lower);

    let title = pane.pane_title.as_deref().unwrap_or_default().to_lowercase();
    let title_has_claude = has_word_token(&title, "claude")
        && !has_word_token(&title, "codex")
        && !has_word_token(&title, "gemini");

    if cmd_basename.contains("codex") || cmd_basename.contains("gemini") {
        return false;
    }

    if cmd_basename.contains("claude") || cmd_basename.contains("anthropic") {
        return true;
    }

    // Claude sometimes reports version-only running_cmd (e.g. 2.1.89)
    let is_version_only = {
        let parts: Vec<&str> = cmd_basename.split('.').collect();
        parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    };
    if is_version_only {
        return title_has_claude;
    }

    // Wrapped launches often show shell/runtime as running_cmd; use title fallback there.
    let is_wrapper_cmd = matches!(
        cmd_basename,
        "bash"
            | "zsh"
            | "sh"
            | "fish"
            | "node"
            | "python"
            | "python3"
            | "bun"
            | "env"
            | "npm"
            | "npx"
            | "pnpm"
            | "yarn"
            | "uvx"
            | "pipx"
    );
    if cmd_opt.is_none() || is_wrapper_cmd {
        return title_has_claude;
    }

    false
}

fn pane_title_has_token(title: &str, token: &str) -> bool {
    has_word_token(title, token)
}

fn coding_agent_signal_strength(pane: &TmuxPane) -> u8 {
    let title = pane.pane_title.as_deref().unwrap_or_default().to_lowercase();
    let title_has_agent = pane_title_has_token(&title, "claude")
        || pane_title_has_token(&title, "codex")
        || pane_title_has_token(&title, "gemini")
        || pane_title_has_token(&title, "anthropic");

    let cmd = pane.running_cmd.as_deref().unwrap_or_default().to_lowercase();
    let cmd_basename = std::path::Path::new(&cmd)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(&cmd);

    if cmd_basename.contains("claude")
        || cmd_basename.contains("codex")
        || cmd_basename.contains("gemini")
        || cmd_basename.contains("anthropic")
    {
        return 3;
    }

    let is_version_only = {
        let parts: Vec<&str> = cmd_basename.split('.').collect();
        parts.len() >= 2 && parts.iter().all(|p| !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()))
    };
    if is_version_only {
        return if title_has_agent { 2 } else { 1 };
    }

    let is_wrapper_cmd = matches!(
        cmd_basename,
        "bash"
            | "zsh"
            | "sh"
            | "fish"
            | "node"
            | "python"
            | "python3"
            | "env"
            | "bun"
            | "npm"
            | "npx"
            | "pnpm"
            | "yarn"
            | "uvx"
            | "pipx"
    );

    if is_wrapper_cmd && title_has_agent {
        return 1;
    }

    0
}

fn is_selectable_agent_pane(pane: &TmuxPane, matched: bool) -> Option<u8> {
    if !is_coding_agent(pane) {
        return None;
    }

    let strength = coding_agent_signal_strength(pane);
    if strength == 0 {
        return None;
    }

    if matched {
        return Some(strength);
    }

    // Keep unmatched wrapped-agent fallback enabled so active wrapper-launched panes
    // remain selectable when JSONL matching is unavailable.
    Some(strength)
}

fn session_status_rank(status: SessionStatus) -> u8 {
    match status {
        SessionStatus::InProgress => 3,
        SessionStatus::Pending => 2,
        SessionStatus::Idle => 1,
        SessionStatus::Done | SessionStatus::Error => 0,
    }
}

fn pane_numeric_id(pane_id: &str) -> Option<u32> {
    pane_id.trim_start_matches('%').parse::<u32>().ok()
}

fn active_agent_candidates(state: &AppState) -> Vec<(PaneKey, TmuxPane, bool, u8, u8)> {
    let Some(ws) = state.tmux_workspace.as_ref() else {
        return Vec::new();
    };

    let mut candidates: Vec<(PaneKey, TmuxPane, bool, u8, u8)> = ws
        .sessions
        .iter()
        .flat_map(|s| {
            s.panes
                .iter()
                .filter_map(|p| {
                    let key = PaneKey {
                        session: s.name.clone(),
                        window: p.window_name.clone(),
                        pane: p.pane_id.clone(),
                    };

                    if let Some(&idx) = state.session_by_pane.get(&key)
                        && let Some(session) = state.sessions.get(idx)
                    {
                        let strength = is_selectable_agent_pane(p, true)?;
                        return Some((
                            key,
                            p.clone(),
                            true,
                            session_status_rank(session.status),
                            strength,
                        ));
                    }

                    let strength = is_selectable_agent_pane(p, false)?;
                    Some((key, p.clone(), false, 0, strength))
                })
                .collect::<Vec<_>>()
        })
        .collect();

    candidates.sort_by_key(|(k, _, matched, status_rank, strength)| {
        (
            Reverse(*matched),
            Reverse(*status_rank),
            Reverse(*strength),
            k.session.clone(),
            k.window.clone(),
            pane_numeric_id(&k.pane).unwrap_or(u32::MAX),
            k.pane.clone(),
        )
    });

    candidates
}

fn resolve_selected_index(previous_keys: &[PaneKey], previous_idx: usize, new_keys: &[PaneKey]) -> usize {
    if new_keys.is_empty() {
        return 0;
    }

    if let Some(prev_key) = previous_keys.get(previous_idx)
        && let Some(new_idx) = new_keys.iter().position(|k| k == prev_key)
    {
        return new_idx;
    }

    previous_idx.min(new_keys.len().saturating_sub(1))
}

fn select_active_agent_pane(state: &AppState) -> Option<(PaneKey, TmuxPane)> {
    let candidates = active_agent_candidates(state);
    if candidates.is_empty() {
        return None;
    }

    let idx = state.selected_pane_idx.min(candidates.len().saturating_sub(1));

    if idx == 0
        && let Some((k, p, _, _, _)) = candidates
            .iter()
            .find(|(_, _, matched, status_rank, _)| *matched && *status_rank == 3)
            .cloned()
    {
        return Some((k, p));
    }

    candidates
        .into_iter()
        .nth(idx)
        .map(|(k, p, _, _, _)| (k, p))
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
    pub agent_pane_count: usize,  // total count of agent panes (for navigation bounds)
    // Session data (filtered to ≤7 days)
    pub sessions: Vec<Session>,
    pub session_by_pane: HashMap<PaneKey, usize>, // pane key -> session index
    // Token data (from JSONL, computed via LiteLLM pricing)
    pub aggregated_tokens: AggregatedTokens,
    // Refresh
    pub refresh_countdown: u64,
    // tmux socket (needed for live pane capture)
    pub tmux_socket: Option<String>,
}

// ============================================================================
// LiteLLM Pricing
// ============================================================================

/// Pricing data for a single model from LiteLLM
#[derive(Debug, Clone, Default)]
pub struct ModelPricing {
    pub input_cost_per_token: f64,
    pub output_cost_per_token: f64,
    pub cache_read_input_token_cost: f64,
    pub cache_creation_input_token_cost: f64,
}

/// Fetched and cached LiteLLM pricing data
#[derive(Debug, Clone, Default)]
pub struct LiteLLMPricing {
    pub models: HashMap<String, ModelPricing>,
}

/// Fallback hardcoded rates for models not in LiteLLM (matching token-usage skill)
fn get_fallback_rates(model: &str) -> ModelPricing {
    if model.contains("claude-opus-4") {
        ModelPricing {
            input_cost_per_token: 0.000015,
            output_cost_per_token: 0.000075,
            cache_read_input_token_cost: 0.0000015,
            cache_creation_input_token_cost: 0.000015,
        }
    } else if model.contains("claude-sonnet-4") {
        ModelPricing {
            input_cost_per_token: 0.000003,
            output_cost_per_token: 0.000015,
            cache_read_input_token_cost: 0.0000003,
            cache_creation_input_token_cost: 0.000003,
        }
    } else if model.contains("claude-haiku-4") {
        ModelPricing {
            input_cost_per_token: 0.0000008,
            output_cost_per_token: 0.000004,
            cache_read_input_token_cost: 0.0000001,
            cache_creation_input_token_cost: 0.0000008,
        }
    } else if model.contains("glm-4") {
        ModelPricing {
            input_cost_per_token: 0.0000001,
            output_cost_per_token: 0.0000005,
            cache_read_input_token_cost: 0.0,
            cache_creation_input_token_cost: 0.0,
        }
    } else if model.contains("gemini-2.5-") {
        ModelPricing {
            input_cost_per_token: 0.000000075,
            output_cost_per_token: 0.00015,
            cache_read_input_token_cost: 0.0,
            cache_creation_input_token_cost: 0.0,
        }
    } else if model.contains("minimax") {
        ModelPricing {
            input_cost_per_token: 0.0,
            output_cost_per_token: 0.0,
            cache_read_input_token_cost: 0.0,
            cache_creation_input_token_cost: 0.0,
        }
    } else {
        ModelPricing::default()
    }
}

impl LiteLLMPricing {
    /// Load pricing from cache file, fetching if stale
    pub fn load() -> Self {
        let cache_dir = std::env::var("HOME")
            .ok()
            .filter(|s| !s.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(std::env::temp_dir)
            .join(".cache")
            .join("claude-pricing");
        let cache_file = cache_dir.join("litellm-pricing.json");
        let cache_ttl = 86400; // 24 hours

        // Check if cache exists and is fresh
        let need_fetch = if !cache_file.exists() {
            true
        } else if let Ok(metadata) = std::fs::metadata(&cache_file) {
            if let Ok(modified) = metadata.modified() {
                let age = std::time::SystemTime::now()
                    .duration_since(modified)
                    .map(|d| d.as_secs())
                    .unwrap_or(cache_ttl + 1);
                age > cache_ttl
            } else {
                true
            }
        } else {
            true
        };

        // Fetch if needed
        if need_fetch {
            let _ = std::fs::create_dir_all(&cache_dir);
            let url = "https://raw.githubusercontent.com/BerriAI/litellm/main/model_prices_and_context_window.json";
            // Use reqwest for native HTTP (no curl dependency)
            if let Ok(client) = reqwest::blocking::Client::builder()
                .timeout(std::time::Duration::from_secs(30))
                .build()
            {
                if let Ok(response) = client.get(url).send() {
                    if response.status().is_success() {
                        let _ = std::fs::write(&cache_file, response.bytes().unwrap_or_default());
                    }
                }
            }
        }

        // Parse cache file
        let mut pricing = LiteLLMPricing::default();
        if let Ok(content) = std::fs::read_to_string(&cache_file) {
            if let Ok(json) = serde_json::from_str::<serde_json::Value>(&content) {
                if let Some(obj) = json.as_object() {
                    for (model_name, model_data) in obj {
                        if let Some(data) = model_data.as_object() {
                            let p = ModelPricing {
                                input_cost_per_token: data
                                    .get("input_cost_per_token")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0),
                                output_cost_per_token: data
                                    .get("output_cost_per_token")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0),
                                cache_read_input_token_cost: data
                                    .get("cache_read_input_token_cost")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0),
                                cache_creation_input_token_cost: data
                                    .get("cache_creation_input_token_cost")
                                    .and_then(|v| v.as_f64())
                                    .unwrap_or(0.0),
                            };
                            pricing.models.insert(model_name.clone(), p);
                        }
                    }
                }
            }
        }

        pricing
    }

    /// Get pricing for a model, with fallback to hardcoded rates
    pub fn get(&self, model: &str) -> ModelPricing {
        self.models.get(model).cloned().unwrap_or_else(|| {
            // Try suffix match first (case-insensitive, like token-usage skill)
            let model_lower = model.to_lowercase();
            for (key, p) in &self.models {
                let key_lower = key.to_lowercase();
                if key_lower.ends_with(&format!("/{}", model_lower)) || key_lower == model_lower {
                    return p.clone();
                }
            }
            get_fallback_rates(model)
        })
    }
}

/// Compute cost for given token counts and model
pub fn compute_cost(
    model: &str,
    input_tokens: u64,
    output_tokens: u64,
    cache_read: u64,
    cache_write: u64,
) -> f64 {
    static PRICING: std::sync::LazyLock<LiteLLMPricing> =
        std::sync::LazyLock::new(LiteLLMPricing::load);
    let p = PRICING.get(model);
    (input_tokens as f64 * p.input_cost_per_token)
        + (output_tokens as f64 * p.output_cost_per_token)
        + (cache_read as f64 * p.cache_read_input_token_cost)
        + (cache_write as f64 * p.cache_creation_input_token_cost)
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
    #[serde(rename = "uuid")]
    uuid: Option<String>,
}

#[derive(Debug, Deserialize)]
struct MessageContent {
    usage: Option<Usage>,
    #[serde(rename = "model")]
    model: Option<String>,
    #[serde(rename = "content")]
    content: Option<serde_json::Value>, // Can be string or array
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

fn parse_session_jsonl(path: &Path, project: &str, project_path: &Path) -> Option<Session> {
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

// ============================================================================
// JSONL Usage Parsing (replaces token log parsing for cost)
// ============================================================================

/// A single aggregated usage record per session+model
#[derive(Debug, Clone, Default)]
struct UsageRecord {
    pub session_id: String,
    pub date: String,
    pub provider: String,
    pub project: String,
    pub model: String,
    pub input_tokens: u64,
    pub output_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub total_tokens: u64,
    pub message_count: u64,
}

impl UsageRecord {
    pub fn cost(&self) -> f64 {
        compute_cost(
            &self.model,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
        )
    }
}

/// Find all JSONL files across all provider directories (matching token-usage skill)
fn find_jsonl_files() -> Vec<PathBuf> {
    let home = std::env::var("HOME").unwrap_or_default();
    let mut files = Vec::new();

    // Collect all provider directories: ~/.claude, ~/.claude-*, etc.
    let mut provider_dirs: Vec<PathBuf> = Vec::new();

    // Standard provider
    let standard = PathBuf::from(&home).join(".claude");
    if standard.exists() && standard.join("projects").exists() {
        provider_dirs.push(standard);
    }

    // Additional providers: ~/.claude-*/*/projects
    if let Ok(entries) = std::fs::read_dir(&home) {
        for entry in entries.flatten() {
            let name = entry.file_name();
            let name_str = name.to_string_lossy();
            if name_str.starts_with(".claude-") && entry.path().is_dir() {
                let projects = entry.path().join("projects");
                if projects.exists() {
                    provider_dirs.push(entry.path());
                }
            }
        }
    }

    for provider_dir in provider_dirs {
        let projects_dir = provider_dir.join("projects");
        if let Ok(entries) = std::fs::read_dir(&projects_dir) {
            for entry in entries.flatten() {
                let project_path = entry.path();
                if project_path.is_dir() {
                    if let Ok(jsonl_entries) = std::fs::read_dir(&project_path) {
                        for jsonl_entry in jsonl_entries.flatten() {
                            let path = jsonl_entry.path();
                            if path.extension().map(|e| e == "jsonl").unwrap_or(false) {
                                files.push(path);
                            }
                        }
                    }
                }
            }
        }
    }

    files
}

/// Process a single JSONL file and extract usage records
fn process_jsonl_file(path: &Path) -> Vec<UsageRecord> {
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(_) => return Vec::new(),
    };

    // Derive provider, project, session_id from path
    let home = std::env::var("HOME").unwrap_or_default();
    let rel_path_raw = path.to_string_lossy().replace(&home, "");
    let rel_path = rel_path_raw.trim_start_matches('/');
    let parts: Vec<&str> = rel_path.split('/').collect();

    let provider = if parts
        .first()
        .map(|s| s.starts_with(".claude-"))
        .unwrap_or(false)
    {
        parts.first().unwrap_or(&".claude").to_string()
    } else {
        ".claude".to_string()
    };

    // project is between "projects" and filename
    let project = if let Some(idx) = parts.iter().position(|s| *s == "projects") {
        parts.get(idx + 1).unwrap_or(&"").to_string()
    } else {
        String::new()
    };

    let session_id = path
        .file_stem()
        .map(|s| s.to_string_lossy().to_string())
        .unwrap_or_default();

    let mut records: Vec<UsageRecord> = Vec::new();
    let mut seen_uuids: std::collections::HashSet<String> = std::collections::HashSet::new();

    for line in content.lines() {
        if line.trim().is_empty() {
            continue;
        }

        // Parse as JSON
        if let Ok(msg) = serde_json::from_str::<JsonlMessage>(line) {
            if msg.msg_type == "assistant" {
                // Deduplicate by uuid (skip if already seen)
                if let Some(ref uuid) = msg.uuid {
                    if seen_uuids.contains(uuid) {
                        continue;
                    }
                    seen_uuids.insert(uuid.clone());
                }

                if let Some(ref message) = msg.message {
                    if let Some(ref usage) = message.usage {
                        let model = message.model.as_deref().unwrap_or("unknown").to_string();

                        let input = usage.input_tokens.unwrap_or(0);
                        let output = usage.output_tokens.unwrap_or(0);
                        let cache_read = usage.cache_read_input_tokens.unwrap_or(0);
                        let cache_write = usage.cache_creation_input_tokens.unwrap_or(0);

                        if input > 0 || output > 0 || cache_read > 0 || cache_write > 0 {
                            records.push(UsageRecord {
                                session_id: session_id.clone(),
                                date: msg.timestamp.clone().unwrap_or_default(),
                                provider: provider.clone(),
                                project: project.clone(),
                                model,
                                input_tokens: input,
                                output_tokens: output,
                                cache_read_tokens: cache_read,
                                cache_write_tokens: cache_write,
                                total_tokens: input + output + cache_read + cache_write,
                                message_count: 1,
                            });
                        }
                    }
                }
            }
        }
    }

    records
}

/// Aggregate usage records by session+model+date (matching token-usage skill)
fn aggregate_usage(records: Vec<UsageRecord>) -> Vec<UsageRecord> {
    use std::collections::HashMap;

    let mut aggregated: HashMap<String, UsageRecord> = HashMap::new();

    for record in records {
        let key = format!(
            "{}|{}|{}|{}|{}",
            record.session_id, record.date, record.provider, record.project, record.model
        );

        let entry = aggregated.entry(key).or_insert_with(|| UsageRecord {
            session_id: record.session_id.clone(),
            date: record.date.clone(),
            provider: record.provider.clone(),
            project: record.project.clone(),
            model: record.model.clone(),
            ..Default::default()
        });

        entry.input_tokens += record.input_tokens;
        entry.output_tokens += record.output_tokens;
        entry.cache_read_tokens += record.cache_read_tokens;
        entry.cache_write_tokens += record.cache_write_tokens;
        entry.total_tokens += record.total_tokens;
        entry.message_count += record.message_count;
    }

    aggregated.into_values().collect()
}

/// Scan all JSONL files and compute aggregated costs (matching token-usage skill cost mode)
fn scan_all_usage() -> AggregatedTokens {
    let files = find_jsonl_files();

    let mut all_records: Vec<UsageRecord> = Vec::new();
    for path in files {
        all_records.extend(process_jsonl_file(&path));
    }

    let aggregated = aggregate_usage(all_records);

    let today = Utc::now().date_naive();

    let mut result = AggregatedTokens::default();
    let mut hourly_tokens: std::collections::HashMap<i64, u64> = std::collections::HashMap::new();

    for record in aggregated {
        let cost = record.cost();

        // All-time totals
        result.total_tokens += record.total_tokens;
        result.total_cost += cost;
        result.total_input += record.input_tokens;
        result.total_output += record.output_tokens;
        result.total_cache_read += record.cache_read_tokens;
        result.total_cache_write += record.cache_write_tokens;

        // Parse timestamp once for daily and hourly grouping
        if let Ok(ts) = DateTime::parse_from_rfc3339(&record.date) {
            let utc_date = ts.with_timezone(&Utc).date_naive();
            if utc_date == today {
                result.today_tokens += record.total_tokens;
                result.today_cost += cost;
                result.today_input += record.input_tokens;
                result.today_output += record.output_tokens;
                result.today_cache_read += record.cache_read_tokens;
                result.today_cache_write += record.cache_write_tokens;
            }
            // Hourly rate (for charts if needed)
            let hour_key = ts.timestamp() / 3600;
            *hourly_tokens.entry(hour_key).or_insert(0) += record.total_tokens;
        }
    }

    // Build 24-hour rate array
    let now_hour = Utc::now().timestamp() / 3600;
    for i in 0..24 {
        let hour = now_hour - i;
        result
            .hourly_rates
            .push(hourly_tokens.get(&hour).copied().unwrap_or(0));
    }
    result.hourly_rates.reverse();

    result
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
            let group = parts
                .get(1)
                .filter(|g| !g.is_empty())
                .map(|g| g.to_string());
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
            !matches!(group, Some(g) if g.as_str() != *name)
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
    let in_progress = state
        .sessions
        .iter()
        .filter(|s| s.status == SessionStatus::InProgress)
        .count();
    let pending = state
        .sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Pending)
        .count();
    let idle = state
        .sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Idle)
        .count();
    let done = state
        .sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Done)
        .count();
    let error = state
        .sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Error)
        .count();

    // Build status breakdown
    let status_line = {
        let mut parts = Vec::new();
        if in_progress > 0 {
            parts.push(
                Span::raw(format!("⚡{} ", in_progress))
                    .set_style(Style::default().fg(colors::GREEN)),
            );
        }
        if pending > 0 {
            parts.push(
                Span::raw(format!("○{} ", pending)).set_style(Style::default().fg(colors::YELLOW)),
            );
        }
        if idle > 0 {
            parts.push(
                Span::raw(format!("·{} ", idle)).set_style(Style::default().fg(colors::SECONDARY)),
            );
        }
        if done > 0 {
            parts.push(
                Span::raw(format!("✓{} ", done)).set_style(Style::default().fg(colors::CYAN)),
            );
        }
        if error > 0 {
            parts.push(
                Span::raw(format!("✗{} ", error)).set_style(Style::default().fg(colors::RED)),
            );
        }
        if parts.is_empty() {
            parts.push(Span::raw("— ").set_style(Style::default().fg(colors::SECONDARY)));
        }
        parts
    };

    let token_str = format_tokens(state.aggregated_tokens.today_tokens);
    let cost_str = if state.aggregated_tokens.today_cost > 0.0 {
        format!("${:.2}", state.aggregated_tokens.today_cost)
    } else {
        String::new()
    };

    let mut header_spans: Vec<Span<'_>> = vec![
        Span::raw("  claudeboard  ").set_style(
            Style::default()
                .fg(colors::ACCENT)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw(&clock).set_style(Style::default().fg(colors::PRIMARY)),
        Span::raw("  ").set_style(Style::default()),
        Span::raw(&countdown).set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw("  ").set_style(Style::default()),
    ];
    // Show today's token breakdown: input in, output out, cache cache, cw cw, cost
    if state.aggregated_tokens.today_input > 0 || state.aggregated_tokens.today_output > 0 {
        header_spans.push(
            Span::raw(format_tokens(state.aggregated_tokens.today_input))
                .set_style(Style::default().fg(colors::YELLOW)),
        );
        header_spans.push(Span::raw(" in, ").set_style(Style::default().fg(colors::SECONDARY)));
        header_spans.push(
            Span::raw(format_tokens(state.aggregated_tokens.today_output))
                .set_style(Style::default().fg(colors::YELLOW)),
        );
        header_spans.push(Span::raw(" out, ").set_style(Style::default().fg(colors::SECONDARY)));
        if state.aggregated_tokens.today_cache_read > 0 {
            header_spans.push(
                Span::raw(format_tokens(state.aggregated_tokens.today_cache_read))
                    .set_style(Style::default().fg(colors::PURPLE)),
            );
            header_spans
                .push(Span::raw(" cache, ").set_style(Style::default().fg(colors::SECONDARY)));
        }
        if state.aggregated_tokens.today_cache_write > 0 {
            header_spans.push(
                Span::raw(format_tokens(state.aggregated_tokens.today_cache_write))
                    .set_style(Style::default().fg(colors::CYAN)),
            );
            header_spans.push(Span::raw(" cw, ").set_style(Style::default().fg(colors::SECONDARY)));
        }
        if !cost_str.is_empty() {
            header_spans
                .push(Span::raw(cost_str.clone()).set_style(Style::default().fg(colors::GREEN)));
        }
    } else {
        // Fallback to simple token count
        header_spans.push(Span::raw(&token_str).set_style(Style::default().fg(colors::YELLOW)));
        header_spans.push(Span::raw(" tokens").set_style(Style::default().fg(colors::SECONDARY)));
        if !cost_str.is_empty() {
            header_spans.push(Span::raw("  ").set_style(Style::default()));
            header_spans.push(Span::raw(&cost_str).set_style(Style::default().fg(colors::GREEN)));
        }
    }
    header_spans.push(Span::raw("     ").set_style(Style::default().fg(colors::SURFACE)));
    header_spans.extend(status_line);

    let line = Line::from(header_spans);

    f.render_widget(
        Paragraph::new(line).set_style(Style::default().bg(colors::SURFACE)),
        inner,
    );
}

// Tree item for the tmux panel — each entry knows its depth and type
enum TmuxTreeEntry {
    Session {
        name: String,
    },
    Window {
        session: String,
        name: String,
        index: String,
    },
    Pane {
        pane_key: PaneKey,
        pane_label: String,     // tmux window name
        repo: Option<String>,   // project name if session matched
        branch: Option<String>, // git branch if session matched
    },
}

fn format_tmux_pane_label(pane: &TmuxPane) -> String {
    if let Some(ref cmd) = pane.running_cmd {
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
        pane.pane_title
            .clone()
            .unwrap_or_else(|| pane.window_name.clone())
    }
}

fn build_tmux_tree_entries(state: &AppState) -> Vec<TmuxTreeEntry> {
    let candidates = active_agent_candidates(state);

    struct PaneEntry {
        pane_key: PaneKey,
        pane_label: String,
        repo: Option<String>,
        branch: Option<String>,
    }

    struct WindowEntry {
        session: String,
        name: String,
        index: String,
        panes: Vec<PaneEntry>,
    }

    struct SessionEntry {
        name: String,
        windows: Vec<WindowEntry>,
    }

    let mut sessions: Vec<SessionEntry> = Vec::new();
    let mut session_idx_by_name: HashMap<String, usize> = HashMap::new();
    let mut window_idx_by_session_and_name: HashMap<(String, String), usize> = HashMap::new();

    for (pane_key, pane, _, _, _) in candidates {
        let (repo, branch) = state
            .session_by_pane
            .get(&pane_key)
            .and_then(|&idx| state.sessions.get(idx))
            .map(|s| {
                let repo = std::path::Path::new(&s.cwd)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(|n| n.to_string())
                    .unwrap_or_else(|| s.project.clone());
                (Some(repo), s.git_branch.clone())
            })
            .unwrap_or((None, None));

        let session_idx = if let Some(&idx) = session_idx_by_name.get(&pane_key.session) {
            idx
        } else {
            let idx = sessions.len();
            sessions.push(SessionEntry {
                name: pane_key.session.clone(),
                windows: Vec::new(),
            });
            session_idx_by_name.insert(pane_key.session.clone(), idx);
            idx
        };

        let window_key = (pane_key.session.clone(), pane_key.window.clone());
        let window_idx = if let Some(&idx) = window_idx_by_session_and_name.get(&window_key) {
            idx
        } else {
            let idx = sessions[session_idx].windows.len();
            sessions[session_idx].windows.push(WindowEntry {
                session: pane_key.session.clone(),
                name: pane_key.window.clone(),
                index: pane.window_index.clone(),
                panes: Vec::new(),
            });
            window_idx_by_session_and_name.insert(window_key, idx);
            idx
        };

        sessions[session_idx].windows[window_idx].panes.push(PaneEntry {
            pane_key,
            pane_label: format_tmux_pane_label(&pane),
            repo,
            branch,
        });
    }

    let mut tree: Vec<TmuxTreeEntry> = Vec::new();
    for session in sessions {
        tree.push(TmuxTreeEntry::Session {
            name: session.name.clone(),
        });
        for window in session.windows {
            tree.push(TmuxTreeEntry::Window {
                session: window.session,
                name: window.name,
                index: window.index,
            });
            for pane in window.panes {
                tree.push(TmuxTreeEntry::Pane {
                    pane_key: pane.pane_key,
                    pane_label: pane.pane_label,
                    repo: pane.repo,
                    branch: pane.branch,
                });
            }
        }
    }

    tree
}

fn render_tmux_panel(f: &mut Frame, area: Rect, state: &AppState) {
    let block = Block::new()
        .title(" ⎔ tmux ")
        .borders(Borders::ALL)
        .border_style(Style::default().fg(colors::BORDER))
        .title_style(
            Style::default()
                .fg(colors::CYAN)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(colors::BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    let Some(ref ws) = state.tmux_workspace else {
        let msg =
            Paragraph::new("  tmux: not running").set_style(Style::default().fg(colors::SECONDARY));
        f.render_widget(msg, inner);
        return;
    };

    if ws.sessions.is_empty() {
        let msg =
            Paragraph::new("  no tmux sessions").set_style(Style::default().fg(colors::SECONDARY));
        f.render_widget(msg, inner);
        return;
    }

    let selected_pane_key = select_active_agent_pane(state).map(|(k, _)| k);

    // ── Build tree entries for all active coding agent panes ─────────────────
    let tree = build_tmux_tree_entries(state);

    if tree.is_empty() {
        let msg = Paragraph::new("  no active coding agent pane")
            .set_style(Style::default().fg(colors::SECONDARY));
        f.render_widget(msg, inner);
        return;
    }

    let total_lines = tree.len();

    // ── Determine visible window ─────────────────────────────────────────────
    // Find which tree line corresponds to the selected pane
    let selected_tree_idx = selected_pane_key.as_ref().and_then(|selected_key| {
        tree.iter().position(|entry| {
            matches!(
                entry,
                TmuxTreeEntry::Pane { pane_key, .. } if pane_key == selected_key
            )
        })
    });

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

            TmuxTreeEntry::Window {
                session,
                name,
                index,
            } => {
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
                let is_last_in_window = !tree[idx + 1..].iter().any(|e| {
                    matches!(
                        e,
                        TmuxTreeEntry::Pane { pane_key: next_key, .. }
                            if next_key.session == pane_key.session && next_key.window == pane_key.window
                    )
                });

                // Use proper tree branch characters
                let tree_char = if is_last_in_window {
                    "  │   └──"
                } else {
                    "  │   ├──"
                };
                // Show ● green if pane has a matched Claude session, ○ dimmed otherwise
                let has_match = repo.is_some();
                let marker = if has_match { "●" } else { "○" };
                let marker_color = if has_match {
                    colors::GREEN
                } else {
                    colors::SECONDARY
                };

                let bg = if is_selected {
                    colors::SURFACE
                } else {
                    colors::BG
                };
                let text_color = if is_selected {
                    colors::ACCENT
                } else if has_match {
                    colors::GREEN
                } else {
                    colors::SECONDARY
                };

                // Build the label: show repo/branch whenever session_by_pane matched (regardless of is_claude)
                // is_claude only affects bullet color (● vs ○)
                let label = if let Some(r) = &repo {
                    if let Some(b) = &branch {
                        format!("[{}] {} @{}", pane_label, r, b)
                    } else {
                        format!("[{}] {} (worktree)", pane_label, r)
                    }
                } else {
                    format!("[{}] %{}", pane_label, pane_key.pane.replace("%", ""))
                };

                let label_style = if is_selected {
                    Style::default()
                        .fg(text_color)
                        .bg(bg)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(text_color).bg(bg)
                };

                lines.push(Line::from(vec![
                    Span::raw(tree_char).set_style(Style::default().fg(colors::BORDER).bg(bg)),
                    Span::raw(marker).set_style(Style::default().fg(marker_color).bg(bg)),
                    Span::raw(" ").set_style(Style::default().bg(bg)),
                    Span::raw(label).set_style(label_style),
                ]));
            }
        }
    }

    // Scrollbar
    if total_lines > max_lines && inner.height >= 3 {
        let scroll_pct = selected_tree_idx
            .map(|i| i as f32 / (total_lines - 1) as f32)
            .unwrap_or(0.0);
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

    let para = Paragraph::new(lines).set_style(Style::default().bg(colors::BG));
    f.render_widget(para, inner);
}

// ============================================================================
// Tmux Pane Capture
// ============================================================================

/// Extract the last Claude response from raw pane lines.
/// Claude responses appear between ██████ markers and user `> ` prompts.
/// Returns only the content of the last Claude message, filtered.
fn pane_has_claude_boundary(lines: &[String]) -> bool {
    lines.iter().any(|l| l.contains("██████"))
}

fn filter_last_claude_response(lines: &[String]) -> Vec<String> {
    if lines.is_empty() {
        return vec![];
    }

    // Normalize captured lines and trim leading/trailing empty lines to keep response glanceable.
    fn normalize_for_display(lines: Vec<String>) -> Vec<String> {
        if lines.is_empty() {
            return lines;
        }

        let normalized: Vec<String> = lines.into_iter().map(|l| l.replace('\r', "")).collect();

        let mut start = 0;
        let mut end = normalized.len();

        while start < end && normalized[start].trim().is_empty() {
            start += 1;
        }
        while end > start && normalized[end - 1].trim().is_empty() {
            end -= 1;
        }

        normalized[start..end].to_vec()
    }

    // Find the last user prompt line (`> ` at start of line)
    let last_user_prompt = lines.iter().enumerate().rev().find(|(_, l)| l.starts_with("> "));

    // Find the last ██████ boundary marker (claude response start)
    let last_boundary = lines
        .iter()
        .enumerate()
        .rev()
        .find(|(_, l)| l.contains("██████"));

    let extracted = match (last_boundary, last_user_prompt) {
        (Some((b_idx, _)), Some((u_idx, _))) if b_idx < u_idx => {
            // Claude response exists before user prompt — extract it.
            // Skip the ██████ line itself, capture content up to (but not including) the last `> `.
            // We take all lines after boundary, then truncate at the LAST `> ` to avoid
            // stopping at Markdown blockquotes that may appear inside the response.
            let after_boundary: Vec<&String> = lines.iter().skip(b_idx + 1).collect();
            let last_prompt = after_boundary
                .iter()
                .enumerate()
                .rev()
                .find(|(_, l)| l.starts_with("> "));
            let end_idx = last_prompt.map(|(i, _)| i).unwrap_or(after_boundary.len());
            after_boundary
                .iter()
                .take(end_idx)
                .map(|l| l.to_string())
                .collect()
        }
        (Some((b_idx, _)), None) => {
            // No user prompt after — capture from last boundary to end (streaming response)
            lines.iter().skip(b_idx + 1).map(|l| l.to_string()).collect()
        }
        (Some((b_idx, _)), Some((u_idx, _))) if b_idx > u_idx => {
            // Boundary is after the user prompt — response is streaming in.
            // Capture everything after the boundary (partial/in-progress response).
            lines.iter().skip(b_idx + 1).map(|l| l.to_string()).collect()
        }
        (None, Some(_)) => {
            // No ██████ boundary found — either session just started (no response yet)
            // or the boundary scrolled above the capture window. Without a boundary marker
            // we cannot reliably show just the last response, so return empty.
            vec![]
        }
        (None, None) => {
            // No boundary, no user prompt — return all
            lines.to_vec()
        }
        // Remaining: b_idx == u_idx (prompt at same line as boundary) or edge cases
        _ => vec![],
    };

    normalize_for_display(extracted)
}

/// Capture the visible content of a tmux pane using capture-pane
fn capture_pane_content(
    socket: &Option<String>,
    session: &str,
    window_idx: &str,
    pane: &str,
) -> Vec<String> {
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
        cmd.args(["capture-pane", "-t", &target, "-p", "-S", "-200"]);
        cmd.output()
    };

    match output {
        Ok(o) if o.status.success() => String::from_utf8_lossy(&o.stdout)
            .lines()
            .map(|s| s.to_string())
            .collect(),
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
        .title_style(
            Style::default()
                .fg(colors::GREEN)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(colors::BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Get active coding-agent pane info
    let (pane_key, pane_info): (PaneKey, Option<TmuxPane>) = select_active_agent_pane(state)
        .map(|(k, p)| (k, Some(p)))
        .unwrap_or_else(|| {
            (
                PaneKey {
                    session: String::new(),
                    window: String::new(),
                    pane: String::new(),
                },
                None,
            )
        });

    // Live poll indicator
    let poll_str = if pane_info.is_some() {
        let cd = state.refresh_countdown;
        if cd > 3 {
            "⚡ polling".to_string()
        } else {
            format!("↻ {}s", cd)
        }
    } else {
        String::new()
    };

    match pane_info {
        Some(pane) => {
            // Capture live pane content via tmux capture-pane
            let lines = capture_pane_content(
                &state.tmux_socket,
                &pane_key.session,
                &pane.window_index,
                &pane_key.pane,
            );

            // Header with pane info
            let pane_label = pane.running_cmd.as_deref().unwrap_or("—");
            let status_color = if pane.pane_dead {
                colors::RED
            } else {
                colors::GREEN
            };
            let status_str = if pane.pane_dead {
                "dead ✗"
            } else {
                "active ⚡"
            };

            let header = vec![
                Span::raw(status_str).set_style(Style::default().fg(status_color)),
                Span::raw(" ").set_style(Style::default()),
                Span::raw(pane_label).set_style(
                    Style::default()
                        .fg(colors::ACCENT)
                        .add_modifier(Modifier::BOLD),
                ),
                Span::raw("  ").set_style(Style::default()),
                Span::raw(&pane_key.session).set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw("/").set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw(&pane_key.window).set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw("/").set_style(Style::default().fg(colors::CYAN)),
                Span::raw(&pane_key.pane).set_style(Style::default().fg(colors::CYAN)),
                Span::raw("  ").set_style(Style::default()),
                Span::raw(&poll_str).set_style(Style::default().fg(if pane.pane_dead {
                    colors::SECONDARY
                } else {
                    colors::GREEN
                })),
            ];

            let mut all_lines: Vec<Line> = vec![Line::from(header)];

            if pane.pane_dead {
                all_lines.push(Line::from(vec![
                    Span::raw("  [pane is dead]").set_style(Style::default().fg(colors::RED)),
                ]));
            } else if lines.is_empty() {
                all_lines.push(Line::from(vec![
                    Span::raw("  [pane content unavailable]")
                        .set_style(Style::default().fg(colors::SECONDARY)),
                ]));
            } else {
                // Filter only when pane output itself contains Claude boundary markers.
                // This avoids stale-title false positives while still supporting wrapped launches.
                let is_claude_pane = pane_has_claude_boundary(&lines) && is_likely_claude_pane(&pane);
                let display_lines = if is_claude_pane {
                    filter_last_claude_response(&lines)
                } else {
                    lines.clone()
                };

                if display_lines.is_empty() && is_claude_pane {
                    all_lines.push(Line::from(vec![
                        Span::raw("  [no recent Claude response in captured pane content]")
                            .set_style(Style::default().fg(colors::SECONDARY)),
                    ]));
                } else {
                    let max_lines = (inner.height as usize).saturating_sub(3).max(1);
                    let max_chars = (inner.width as usize).saturating_sub(4).max(10);
                    // Show last N lines, truncated to fit width
                    for line_text in display_lines.iter().rev().take(max_lines).rev() {
                        let display = if line_text.chars().count() > max_chars {
                            let limit = max_chars.saturating_sub(3);
                            let truncated: String = line_text.chars().take(limit).collect();
                            format!("{}...", truncated)
                        } else {
                            line_text.clone()
                        };
                        all_lines.push(Line::from(vec![
                            Span::raw("  ").set_style(Style::default().fg(colors::SECONDARY)),
                            Span::raw(display)
                                .set_style(Style::default().fg(colors::PRIMARY)),
                        ]));
                    }
                }
            }

            let para = Paragraph::new(all_lines).set_style(Style::default().bg(colors::BG));
            f.render_widget(para, inner);
        }
        None => {
            let lines = vec![
                Line::from(vec![
                    Span::raw("  no pane selected")
                        .set_style(Style::default().fg(colors::SECONDARY)),
                ]),
                Line::from(vec![
                    Span::raw("  use j/k to navigate")
                        .set_style(Style::default().fg(colors::SECONDARY)),
                ]),
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
        .title_style(
            Style::default()
                .fg(colors::YELLOW)
                .add_modifier(Modifier::BOLD),
        )
        .style(Style::default().bg(colors::BG));

    let inner = block.inner(area);
    f.render_widget(block, area);

    // Get active coding-agent pane info and corresponding session
    let (pane_key, pane_info): (PaneKey, Option<TmuxPane>) = select_active_agent_pane(state)
        .map(|(k, p)| (k, Some(p)))
        .unwrap_or_else(|| {
            (
                PaneKey {
                    session: String::new(),
                    window: String::new(),
                    pane: String::new(),
                },
                None,
            )
        });

    let session = state
        .session_by_pane
        .get(&pane_key)
        .and_then(|&i| state.sessions.get(i));

    match session {
        Some(session) => {
            let status_icon = session_status_icon(session.status);
            let status_color = session_status_color(session.status);
            let branch_str = session
                .git_branch
                .as_ref()
                .map(|b| format!("@{}", b))
                .unwrap_or_default();
            let idle_min = (Utc::now() - session.last_active).num_minutes();
            let idle_str = if idle_min < 1 {
                "just now".to_string()
            } else {
                format!("{}m ago", idle_min)
            };

            // Compute user and assistant idle times
            let user_idle_str = session.last_user_msg.map(|ts| {
                let m = (Utc::now() - ts).num_minutes();
                if m < 1 { "now".to_string() } else { format!("{}m", m) }
            }).unwrap_or_else(|| "—".to_string());
            let asst_idle_str = session.last_asst_msg.map(|ts| {
                let m = (Utc::now() - ts).num_minutes();
                if m < 1 { "now".to_string() } else { format!("{}m", m) }
            }).unwrap_or_else(|| "—".to_string());

            let mut lines: Vec<Line> = vec![
                Line::from(vec![
                    Span::raw(status_icon).set_style(Style::default().fg(status_color)),
                    Span::raw(" ").set_style(Style::default()),
                    Span::raw(&session.project).set_style(
                        Style::default()
                            .fg(colors::ACCENT)
                            .add_modifier(Modifier::BOLD),
                    ),
                    Span::raw(" ").set_style(Style::default()),
                    Span::raw(&branch_str).set_style(Style::default().fg(colors::PURPLE)),
                ]),
                Line::from(vec![
                    Span::raw("  id: ").set_style(Style::default().fg(colors::SECONDARY)),
                    Span::raw(&session.id).set_style(Style::default().fg(colors::CYAN)),
                    Span::raw(" · last: ").set_style(Style::default().fg(colors::SECONDARY)),
                    Span::raw(&idle_str).set_style(Style::default().fg(colors::YELLOW)),
                    Span::raw(" · you: ").set_style(Style::default().fg(colors::SECONDARY)),
                    Span::raw(&user_idle_str).set_style(Style::default().fg(colors::CYAN)),
                    Span::raw(" · asst: ").set_style(Style::default().fg(colors::SECONDARY)),
                    Span::raw(&asst_idle_str).set_style(Style::default().fg(colors::GREEN)),
                ]),
            ];

            // Show model if available
            if let Some(ref model) = session.model {
                if let Some(model_display) = model.split('/').last() {
                    lines.push(Line::from(vec![
                        Span::raw("  model: ").set_style(Style::default().fg(colors::SECONDARY)),
                        Span::raw(model_display).set_style(Style::default().fg(colors::GREEN)),
                    ]));
                }
            }

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
            lines.push(Line::from(vec![
                Span::raw(" msgs ").set_style(
                    Style::default()
                        .fg(colors::SECONDARY)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  ● asst: ").set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw(format!("{}", session.message_counts.assistant))
                    .set_style(Style::default().fg(colors::GREEN)),
                Span::raw("  ● user: ").set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw(format!("{}", session.message_counts.user))
                    .set_style(Style::default().fg(colors::CYAN)),
                Span::raw("  ● sys: ").set_style(Style::default().fg(colors::SECONDARY)),
                Span::raw(format!("{}", session.message_counts.system))
                    .set_style(Style::default().fg(colors::PURPLE)),
            ]));

            lines.push(Line::from(vec![]));
            lines.push(Line::from(vec![
                Span::raw(" tokens ").set_style(
                    Style::default()
                        .fg(colors::SECONDARY)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));

            let total = session.token_counts.total();
            if total > 0 {
                // Draw token breakdown bar
                let bar_width = (inner.width.saturating_sub(4) as u64).max(1);
                let draw_bar = |tokens: u64, color: Color, label: &str| {
                    let bar_len = ((tokens as f64 / total as f64) * bar_width as f64) as u16;
                    let bar_str = "█".repeat(bar_len as usize);
                    vec![
                        Span::raw(format!("{:>6} ", format_tokens(tokens)))
                            .set_style(Style::default().fg(colors::SECONDARY)),
                        Span::raw(bar_str).set_style(Style::default().fg(color)),
                        Span::raw(format!(
                            " {:>4.0}% {}",
                            (tokens as f64 / total as f64 * 100.0),
                            label
                        ))
                        .set_style(Style::default().fg(colors::SECONDARY)),
                    ]
                };
                lines.push(Line::from(draw_bar(
                    session.token_counts.input_tokens,
                    colors::CYAN,
                    "in",
                )));
                lines.push(Line::from(draw_bar(
                    session.token_counts.output_tokens,
                    colors::YELLOW,
                    "out",
                )));
                let cache_total = session.token_counts.cache_read_input_tokens
                    + session.token_counts.cache_creation_input_tokens;
                lines.push(Line::from(draw_bar(cache_total, colors::PURPLE, "cache")));

                // Show cache hit ratio if we have cache reads
                if session.token_counts.cache_read_input_tokens > 0 {
                    // cache_hit = cache_read / (input + cache_read)
                    // cache_creation is excluded since it's a write, not a read
                    let total_in = session.token_counts.input_tokens
                        + session.token_counts.cache_read_input_tokens;
                    if total_in > 0 {
                        let cache_ratio = session.token_counts.cache_read_input_tokens as f64 / total_in as f64;
                        lines.push(Line::from(vec![
                            Span::raw("  cache hit: ").set_style(Style::default().fg(colors::SECONDARY)),
                            Span::raw(format!("{:.0}%", cache_ratio * 100.0)).set_style(Style::default().fg(colors::PURPLE)),
                        ]));
                    }
                }

                // Show output/input ratio
                if session.token_counts.input_tokens > 0 {
                    let ratio = session.token_counts.output_tokens as f64 / session.token_counts.input_tokens as f64;
                    lines.push(Line::from(vec![
                        Span::raw("  out/in: ").set_style(Style::default().fg(colors::SECONDARY)),
                        Span::raw(format!("{:.1}", ratio)).set_style(Style::default().fg(colors::YELLOW)),
                    ]));
                }

                // Show session cost if model is available
                if let Some(ref model) = session.model {
                    let cost = compute_cost(
                        model,
                        session.token_counts.input_tokens,
                        session.token_counts.output_tokens,
                        session.token_counts.cache_read_input_tokens,
                        session.token_counts.cache_creation_input_tokens,
                    );
                    if cost > 0.0 {
                        lines.push(Line::from(vec![
                            Span::raw("  cost: ").set_style(Style::default().fg(colors::SECONDARY)),
                            Span::raw(format!("${:.2}", cost)).set_style(Style::default().fg(colors::GREEN)),
                        ]));
                    }
                }
            } else {
                lines.push(Line::from(vec![
                    Span::raw("  — no token data")
                        .set_style(Style::default().fg(colors::SECONDARY)),
                ]));
            }

            lines.push(Line::from(vec![]));
            lines.push(Line::from(vec![
                Span::raw(" queue ").set_style(
                    Style::default()
                        .fg(colors::SECONDARY)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));

            if session.queue_ops.is_empty() {
                let idle_m = (Utc::now() - session.last_active).num_minutes();
                if idle_m > 60 {
                    lines.push(Line::from(vec![
                        Span::raw("  — idle >1h, no queue ops")
                            .set_style(Style::default().fg(colors::SECONDARY)),
                    ]));
                } else if session.message_counts.assistant == 0 && session.message_counts.user == 0
                {
                    lines.push(Line::from(vec![
                        Span::raw("  — new session, no ops yet")
                            .set_style(Style::default().fg(colors::SECONDARY)),
                    ]));
                } else {
                    lines.push(Line::from(vec![
                        Span::raw("  — no operations")
                            .set_style(Style::default().fg(colors::SECONDARY)),
                    ]));
                }
            } else {
                // Show current op status prominently
                if let Some(current_op) = session.queue_ops.last() {
                    let icon = queue_op_icon(&current_op.operation);
                    let icon_color = queue_op_color(&current_op.operation);
                    let time_str = current_op.timestamp.format("%H:%M:%S").to_string();
                    lines.push(Line::from(vec![
                        Span::raw("  now: ").set_style(Style::default().fg(colors::SECONDARY)),
                        Span::raw(icon).set_style(Style::default().fg(icon_color)),
                        Span::raw(" ").set_style(Style::default()),
                        Span::raw(&current_op.operation).set_style(Style::default().fg(icon_color)),
                        Span::raw(" [").set_style(Style::default().fg(colors::SECONDARY)),
                        Span::raw(time_str).set_style(Style::default().fg(colors::CYAN)),
                        Span::raw("]").set_style(Style::default().fg(colors::SECONDARY)),
                    ]));
                }

                // Show queue depth
                let total_ops = session.queue_ops.len();
                lines.push(Line::from(vec![
                    Span::raw("  ops: ").set_style(Style::default().fg(colors::SECONDARY)),
                    Span::raw(format!("{}", total_ops)).set_style(Style::default().fg(colors::YELLOW)),
                ]));

                // Show each queue op
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

                // Show ops velocity and session age based on visible ops spread
                // iter().rev() gives us newest-first, so first()=newest, last()=oldest
                if session.queue_ops.len() >= 2 {
                    let recent_ops: Vec<&QueueOp> = session.queue_ops.iter().rev().take(6).collect();
                    if let (Some(newest), Some(oldest)) = (recent_ops.first(), recent_ops.last()) {
                        let span_seconds = (newest.timestamp - oldest.timestamp).num_seconds();
                        if span_seconds > 0 {
                            let ops_count = recent_ops.len() as f64;
                            let velocity = ops_count / (span_seconds as f64 / 60.0);
                            lines.push(Line::from(vec![
                                Span::raw("  velocity: ").set_style(Style::default().fg(colors::SECONDARY)),
                                Span::raw(format!("{:.1}", velocity)).set_style(Style::default().fg(colors::GREEN)),
                                Span::raw(" ops/m").set_style(Style::default().fg(colors::SECONDARY)),
                            ]));

                            let age_minutes = span_seconds / 60;
                            if age_minutes > 0 {
                                lines.push(Line::from(vec![
                                    Span::raw("  age: ").set_style(Style::default().fg(colors::SECONDARY)),
                                    Span::raw(format!("{}m", age_minutes)).set_style(Style::default().fg(colors::CYAN)),
                                ]));
                            }
                        }
                    }
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
            let mut lines = vec![Line::from(vec![
                Span::raw("  no JSONL session matched")
                    .set_style(Style::default().fg(colors::YELLOW)),
            ])];
            if let Some(ref pane) = pane_info {
                lines.push(Line::from(vec![
                    Span::raw("  cwd: ").set_style(Style::default().fg(colors::SECONDARY)),
                    {
                        let cwd = if pane.cwd.chars().count() > 35 {
                            format!("...{}", truncate_from_end(&pane.cwd, 32))
                        } else {
                            pane.cwd.clone()
                        };
                        Span::raw(cwd).set_style(Style::default().fg(colors::PRIMARY))
                    },
                ]));
                lines.push(Line::from(vec![
                    Span::raw("  cmd: ").set_style(Style::default().fg(colors::SECONDARY)),
                    Span::raw(pane.running_cmd.as_deref().unwrap_or("—"))
                        .set_style(Style::default().fg(colors::GREEN)),
                ]));
                lines.push(Line::from(vec![
                    Span::raw("  pane: ").set_style(Style::default().fg(colors::SECONDARY)),
                    Span::raw(&pane.pane_id).set_style(Style::default().fg(colors::CYAN)),
                ]));
            }
            lines.push(Line::from(vec![]));
            lines.push(Line::from(vec![
                Span::raw("  sessions matched by cwd or")
                    .set_style(Style::default().fg(colors::SECONDARY)),
            ]));
            lines.push(Line::from(vec![
                Span::raw("  project name in JSONL logs")
                    .set_style(Style::default().fg(colors::SECONDARY)),
            ]));

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

    // Format cost with proper precision
    let today_cost_str = if state.aggregated_tokens.today_cost > 0.0 {
        format!("${:.4}", state.aggregated_tokens.today_cost)
    } else {
        String::new()
    };
    let total_cost_str = if state.aggregated_tokens.total_cost > 0.0 {
        format!("${:.2}", state.aggregated_tokens.total_cost)
    } else {
        String::new()
    };

    // Count sessions by status
    let in_progress = state
        .sessions
        .iter()
        .filter(|s| s.status == SessionStatus::InProgress)
        .count();
    let pending = state
        .sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Pending)
        .count();
    let idle = state
        .sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Idle)
        .count();
    let done = state
        .sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Done)
        .count();
    let error = state
        .sessions
        .iter()
        .filter(|s| s.status == SessionStatus::Error)
        .count();

    let left = [
        Span::raw("q:quit").set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw("  ").set_style(Style::default().fg(colors::SURFACE)),
        Span::raw("↑↓:navigate").set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw("  ").set_style(Style::default().fg(colors::SURFACE)),
        Span::raw("r:refresh").set_style(Style::default().fg(colors::SECONDARY)),
    ];

    // Build status breakdown string
    let status_parts = {
        let mut parts = Vec::new();
        if in_progress > 0 {
            parts.push(format!("⚡{}", in_progress));
        }
        if pending > 0 {
            parts.push(format!("○{}", pending));
        }
        if idle > 0 {
            parts.push(format!("·{}", idle));
        }
        if done > 0 {
            parts.push(format!("✓{}", done));
        }
        if error > 0 {
            parts.push(format!("✗{}", error));
        }
        if parts.is_empty() {
            parts.push("—".to_string());
        }
        parts.join(" ")
    };

    // Right side: matching /cost format
    // {n} panes · {status} · today: {input} in, {output} out, {cache} cache, {cw} cw (${cost})
    // total: {input} in, {output} out, {cache} cache, {cw} cw (${cost})
    let today_in = format_tokens(state.aggregated_tokens.today_input);
    let today_out = format_tokens(state.aggregated_tokens.today_output);
    let today_cr = format_tokens(state.aggregated_tokens.today_cache_read);
    let today_cw = format_tokens(state.aggregated_tokens.today_cache_write);

    let total_in = format_tokens(state.aggregated_tokens.total_input);
    let total_out = format_tokens(state.aggregated_tokens.total_output);
    let total_cr = format_tokens(state.aggregated_tokens.total_cache_read);
    let total_cw = format_tokens(state.aggregated_tokens.total_cache_write);

    let mut right: Vec<Span<'_>> = vec![
        Span::raw(format!("{} panes", pane_count)).set_style(Style::default().fg(colors::CYAN)),
        Span::raw(" · ").set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw(status_parts.clone()).set_style(Style::default().fg(colors::PRIMARY)),
        Span::raw(" · today: ").set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw(today_in).set_style(Style::default().fg(colors::YELLOW)),
        Span::raw(" in, ").set_style(Style::default().fg(colors::SECONDARY)),
        Span::raw(today_out).set_style(Style::default().fg(colors::YELLOW)),
        Span::raw(" out, ").set_style(Style::default().fg(colors::SECONDARY)),
    ];
    if state.aggregated_tokens.today_cache_read > 0 {
        right.push(Span::raw(today_cr).set_style(Style::default().fg(colors::PURPLE)));
        right.push(Span::raw(" cache, ").set_style(Style::default().fg(colors::SECONDARY)));
    }
    if state.aggregated_tokens.today_cache_write > 0 {
        right.push(Span::raw(today_cw).set_style(Style::default().fg(colors::CYAN)));
        right.push(Span::raw(" cw, ").set_style(Style::default().fg(colors::SECONDARY)));
    }
    if !today_cost_str.is_empty() {
        right.push(Span::raw(today_cost_str).set_style(Style::default().fg(colors::GREEN)));
    }
    right.push(Span::raw(" · total: ").set_style(Style::default().fg(colors::SECONDARY)));
    right.push(Span::raw(total_in).set_style(Style::default().fg(colors::SECONDARY)));
    right.push(Span::raw(" in, ").set_style(Style::default().fg(colors::SECONDARY)));
    right.push(Span::raw(total_out).set_style(Style::default().fg(colors::SECONDARY)));
    right.push(Span::raw(" out, ").set_style(Style::default().fg(colors::SECONDARY)));
    if state.aggregated_tokens.total_cache_read > 0 {
        right.push(Span::raw(total_cr).set_style(Style::default().fg(colors::PURPLE)));
        right.push(Span::raw(" cache, ").set_style(Style::default().fg(colors::SECONDARY)));
    }
    if state.aggregated_tokens.total_cache_write > 0 {
        right.push(Span::raw(total_cw).set_style(Style::default().fg(colors::CYAN)));
        right.push(Span::raw(" cw, ").set_style(Style::default().fg(colors::SECONDARY)));
    }
    if !total_cost_str.is_empty() {
        right.push(Span::raw(total_cost_str).set_style(Style::default().fg(colors::SECONDARY)));
    }

    let mut line_spans: Vec<Span<'_>> = Vec::new();
    line_spans.extend(left.iter().cloned());
    line_spans
        .push(Span::raw("                    ").set_style(Style::default().fg(colors::SURFACE)));
    line_spans.extend(right);

    let line = Line::from(line_spans);

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
        .constraints([Constraint::Percentage(40), Constraint::Percentage(60)])
        .split(Rect::new(area.x, area.y + 1, area.width, body_height));

    // Left: TMUX panel (full height)
    render_tmux_panel(f, chunks[0], state);

    // Right: live pane (top 60%) + session metadata (bottom 40%)
    let right_chunks = Layout::default()
        .direction(Direction::Vertical)
        .constraints([Constraint::Percentage(60), Constraint::Percentage(40)])
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

                let tokens = tokio::task::spawn_blocking(scan_all_usage)
                    .await
                    .unwrap_or_default();

                let tmux_socket_clone = tmux_socket.clone();
                let tmux_ws =
                    tokio::task::spawn_blocking(move || parse_tmux_workspace(&tmux_socket_clone))
                        .await
                        .unwrap_or(None);

                // Build session_by_pane map: match Claude Code session to tmux pane
                // Strategy 1: Match by session.cwd (the real cwd from JSONL) if it has depth >= 4
                // Strategy 2: Match by project name if pane.cwd ends with the session's project dir name
                // The project name is more reliable than cwd because JSONL cwd is often ~/.claude
                let mut session_by_pane = HashMap::new();
                if let Some(ref ws) = tmux_ws {
                    // Match panes to parsed Claude sessions
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
                                    let cwd_match =
                                        session_depth >= 4 && pane_cwd.starts_with(session_cwd);

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

                let (selected, previous_keys) = {
                    let s = state_clone.read();
                    let previous_keys = active_agent_candidates(&s)
                        .into_iter()
                        .map(|(k, _, _, _, _)| k)
                        .collect::<Vec<_>>();
                    (s.selected_pane_idx, previous_keys)
                };

                let mut s = state_clone.write();
                s.sessions = sessions;
                s.aggregated_tokens = tokens;
                s.tmux_workspace = tmux_ws;
                s.session_by_pane = session_by_pane;

                let candidates = active_agent_candidates(&s);
                let new_keys = candidates
                    .iter()
                    .map(|(k, _, _, _, _)| k.clone())
                    .collect::<Vec<_>>();

                s.agent_pane_count = candidates.len();
                s.selected_pane_idx = resolve_selected_index(&previous_keys, selected, &new_keys);
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

        if crossterm::event::poll(Duration::from_millis(100))?
            && let crossterm::event::Event::Key(key) = crossterm::event::read()?
            && key.kind == KeyEventKind::Press
        {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_filter_last_claude_response_basic() {
        // Simple case: boundary, response, then prompt
        let lines = vec![
            "previous stuff".to_string(),
            "██████".to_string(),
            "Claude's response line 1".to_string(),
            "Claude's response line 2".to_string(),
            "> ".to_string(),
            "next prompt".to_string(),
        ];
        let result = filter_last_claude_response(&lines);
        assert_eq!(result, vec!["Claude's response line 1", "Claude's response line 2"]);
    }

    #[test]
    fn test_filter_last_claude_response_no_boundary() {
        // No boundary - should return empty (can't extract reliably)
        let lines = vec![
            "some content".to_string(),
            "> ".to_string(),
        ];
        let result = filter_last_claude_response(&lines);
        assert!(result.is_empty());
    }

    #[test]
    fn test_filter_last_claude_response_streaming() {
        // Boundary AFTER prompt = streaming response in progress
        let lines = vec![
            "old stuff".to_string(),
            "> ".to_string(),
            "██████".to_string(),
            "partial response".to_string(),
        ];
        let result = filter_last_claude_response(&lines);
        assert_eq!(result, vec!["partial response"]);
    }

    #[test]
    fn test_filter_last_claude_response_with_blockquotes() {
        // Claude response containing Markdown blockquotes - should NOT truncate at blockquotes
        let lines = vec![
            "██████".to_string(),
            "Here's the plan:".to_string(),
            "> quote from previous".to_string(),
            "More of Claude's response".to_string(),
            "> ".to_string(),
        ];
        let result = filter_last_claude_response(&lines);
        // Should capture all content before the LAST > prompt
        assert_eq!(result, vec!["Here's the plan:", "> quote from previous", "More of Claude's response"]);
    }

    #[test]
    fn test_filter_last_claude_response_only_boundary_no_prompt() {
        // Boundary present but no user prompt after - streaming/no-prompt-yet case
        let lines = vec![
            "██████".to_string(),
            "Claude's response".to_string(),
        ];
        let result = filter_last_claude_response(&lines);
        assert_eq!(result, vec!["Claude's response"]);
    }

    #[test]
    fn test_filter_last_claude_response_boundary_at_same_line() {
        // Boundary and prompt at same position - waiting for response
        let lines = vec![
            "stuff".to_string(),
            "██████ > ".to_string(),
        ];
        let result = filter_last_claude_response(&lines);
        assert!(result.is_empty());
    }

    #[test]
    fn test_filter_last_claude_response_trims_blank_edges() {
        let lines = vec![
            "██████".to_string(),
            "".to_string(),
            "  ".to_string(),
            "Claude response".to_string(),
            "".to_string(),
            "> ".to_string(),
        ];
        let result = filter_last_claude_response(&lines);
        assert_eq!(result, vec!["Claude response"]);
    }

    #[test]
    fn test_is_likely_claude_pane_claude_cmd() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("irrelevant".to_string()),
            pane_dead: false,
        };
        assert!(is_likely_claude_pane(&pane));
    }

    #[test]
    fn test_is_likely_claude_pane_non_claude_cmd() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("codex".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        assert!(!is_likely_claude_pane(&pane));
    }

    #[test]
    fn test_is_likely_claude_pane_version_with_title_fallback() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("2.1.89".to_string()),
            pane_title: Some("Claude Code".to_string()),
            pane_dead: false,
        };
        assert!(is_likely_claude_pane(&pane));
    }

    #[test]
    fn test_is_likely_claude_pane_version_without_claude_title_is_false() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("2.1.89".to_string()),
            pane_title: Some("shell".to_string()),
            pane_dead: false,
        };
        assert!(!is_likely_claude_pane(&pane));
    }

    #[test]
    fn test_is_likely_claude_pane_does_not_use_title_when_cmd_exists() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("top".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        assert!(!is_likely_claude_pane(&pane));
    }

    #[test]
    fn test_is_likely_claude_pane_uses_title_for_wrapper_cmd() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("bash".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        assert!(is_likely_claude_pane(&pane));
    }

    #[test]
    fn test_is_likely_claude_pane_uses_title_for_npx_wrapper_cmd() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("npx".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        assert!(is_likely_claude_pane(&pane));
    }

    #[test]
    fn test_is_coding_agent_wrapper_cmd_with_claude_title() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("npx".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        assert!(is_coding_agent(&pane));
    }

    #[test]
    fn test_is_coding_agent_shell_cmd_with_claude_title_is_true() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("bash".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        assert!(is_coding_agent(&pane));
    }

    #[test]
    fn test_is_coding_agent_shell_cmd_with_non_agent_title_is_false() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("bash".to_string()),
            pane_title: Some("shell".to_string()),
            pane_dead: false,
        };
        assert!(!is_coding_agent(&pane));
    }

    #[test]
    fn test_is_coding_agent_version_is_agent_without_title() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("2.1.89".to_string()),
            pane_title: Some("shell".to_string()),
            pane_dead: false,
        };
        assert!(is_coding_agent(&pane));
    }

    #[test]
    fn test_is_coding_agent_version_with_agent_title() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("2.1.89".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        assert!(is_coding_agent(&pane));
    }

    #[test]
    fn test_filter_last_claude_response_normalizes_carriage_returns() {
        let lines = vec![
            "██████".to_string(),
            "line one\r".to_string(),
            "line two\r".to_string(),
            "> ".to_string(),
        ];
        let result = filter_last_claude_response(&lines);
        assert_eq!(result, vec!["line one", "line two"]);
    }

    #[test]
    fn test_has_word_token_requires_whole_token() {
        assert!(has_word_token("claude", "claude"));
        assert!(has_word_token("run claude now", "claude"));
        assert!(!has_word_token("myclaudepane", "claude"));
    }

    #[test]
    fn test_pane_has_claude_boundary() {
        let with_boundary = vec!["foo".to_string(), "██████".to_string()];
        let without_boundary = vec!["foo".to_string(), "bar".to_string()];
        assert!(pane_has_claude_boundary(&with_boundary));
        assert!(!pane_has_claude_boundary(&without_boundary));
    }

    #[test]
    fn test_coding_agent_signal_strength_prefers_direct_agent_cmd() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("shell".to_string()),
            pane_dead: false,
        };
        assert_eq!(coding_agent_signal_strength(&pane), 3);
    }

    #[test]
    fn test_coding_agent_signal_strength_wrapper_title_fallback_is_weaker() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("bash".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        assert_eq!(coding_agent_signal_strength(&pane), 1);
    }

    #[test]
    fn test_select_active_agent_pane_keeps_unmatched_wrapper_claude_title_fallback() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s1".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("bash".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![TmuxSession {
                name: "s1".to_string(),
                group: None,
                panes: vec![pane],
            }],
            total_panes: 1,
        };

        let key = PaneKey {
            session: "s1".to_string(),
            window: "w".to_string(),
            pane: "%1".to_string(),
        };

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 0,
            sessions: vec![],
            session_by_pane: HashMap::new(),
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        assert_eq!(select_active_agent_pane(&state).map(|(k, _)| k), Some(key));
    }

    #[test]
    fn test_is_selectable_agent_pane_unmatched_wrapper_claude_is_selectable() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("bash".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        assert!(is_selectable_agent_pane(&pane, false).is_some());
    }

    #[test]
    fn test_is_selectable_agent_pane_unmatched_wrapper_codex_is_selectable() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp".to_string(),
            running_cmd: Some("bash".to_string()),
            pane_title: Some("codex".to_string()),
            pane_dead: false,
        };
        assert!(is_selectable_agent_pane(&pane, false).is_some());
    }

    #[test]
    fn test_select_active_agent_pane_keeps_unmatched_wrapper_codex_title_fallback() {
        let pane = TmuxPane {
            pane_id: "%9".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s9".to_string(),
            cwd: "/tmp/fallback".to_string(),
            running_cmd: Some("bash".to_string()),
            pane_title: Some("codex".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![TmuxSession {
                name: "s9".to_string(),
                group: None,
                panes: vec![pane],
            }],
            total_panes: 1,
        };

        let key = PaneKey {
            session: "s9".to_string(),
            window: "w".to_string(),
            pane: "%9".to_string(),
        };

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 0,
            sessions: vec![],
            session_by_pane: HashMap::new(),
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let selected = select_active_agent_pane(&state);
        assert_eq!(selected.map(|(k, _)| k), Some(key));
    }

    #[test]
    fn test_select_active_agent_pane_prefers_in_progress_session() {
        let pane_active = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s1".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let pane_idle = TmuxPane {
            pane_id: "%2".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s2".to_string(),
            cwd: "/tmp/b".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![
                TmuxSession {
                    name: "s1".to_string(),
                    group: None,
                    panes: vec![pane_active.clone()],
                },
                TmuxSession {
                    name: "s2".to_string(),
                    group: None,
                    panes: vec![pane_idle.clone()],
                },
            ],
            total_panes: 2,
        };

        let key_active = PaneKey {
            session: "s1".to_string(),
            window: "w".to_string(),
            pane: "%1".to_string(),
        };
        let key_idle = PaneKey {
            session: "s2".to_string(),
            window: "w".to_string(),
            pane: "%2".to_string(),
        };

        let now = Utc::now();
        let sessions = vec![
            Session {
                id: "active".to_string(),
                project: "proj-a".to_string(),
                project_path: "/tmp/a".to_string(),
                cwd: "/tmp/a".to_string(),
                git_branch: None,
                status: SessionStatus::InProgress,
                last_active: now,
                message_counts: MessageCounts::default(),
                token_counts: TokenCounts::default(),
                queue_ops: vec![],
                model: None,
                last_user_msg: None,
                last_asst_msg: None,
            },
            Session {
                id: "idle".to_string(),
                project: "proj-b".to_string(),
                project_path: "/tmp/b".to_string(),
                cwd: "/tmp/b".to_string(),
                git_branch: None,
                status: SessionStatus::Idle,
                last_active: now,
                message_counts: MessageCounts::default(),
                token_counts: TokenCounts::default(),
                queue_ops: vec![],
                model: None,
                last_user_msg: None,
                last_asst_msg: None,
            },
        ];

        let mut session_by_pane = HashMap::new();
        session_by_pane.insert(key_active.clone(), 0);
        session_by_pane.insert(key_idle, 1);

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 0,
            sessions,
            session_by_pane,
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let selected = select_active_agent_pane(&state);
        assert_eq!(selected.map(|(k, _)| k), Some(key_active));
    }

    #[test]
    fn test_select_active_agent_pane_prefers_jsonl_match_over_unmatched() {
        let pane_matched = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s1".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("shell".to_string()),
            pane_dead: false,
        };

        let pane_unmatched = TmuxPane {
            pane_id: "%2".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s2".to_string(),
            cwd: "/tmp/b".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![
                TmuxSession {
                    name: "s1".to_string(),
                    group: None,
                    panes: vec![pane_matched.clone()],
                },
                TmuxSession {
                    name: "s2".to_string(),
                    group: None,
                    panes: vec![pane_unmatched],
                },
            ],
            total_panes: 2,
        };

        let key_matched = PaneKey {
            session: "s1".to_string(),
            window: "w".to_string(),
            pane: "%1".to_string(),
        };

        let sessions = vec![Session {
            id: "matched".to_string(),
            project: "proj-a".to_string(),
            project_path: "/tmp/a".to_string(),
            cwd: "/tmp/a".to_string(),
            git_branch: None,
            status: SessionStatus::InProgress,
            last_active: Utc::now(),
            message_counts: MessageCounts::default(),
            token_counts: TokenCounts::default(),
            queue_ops: vec![],
            model: None,
            last_user_msg: None,
            last_asst_msg: None,
        }];

        let mut session_by_pane = HashMap::new();
        session_by_pane.insert(key_matched.clone(), 0);

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 0,
            sessions,
            session_by_pane,
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let selected = select_active_agent_pane(&state);
        assert_eq!(selected.map(|(k, _)| k), Some(key_matched));
    }

    #[test]
    fn test_select_active_agent_pane_uses_unmatched_agent_fallback() {
        let pane = TmuxPane {
            pane_id: "%9".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s9".to_string(),
            cwd: "/tmp/fallback".to_string(),
            running_cmd: Some("codex".to_string()),
            pane_title: Some("codex".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![TmuxSession {
                name: "s9".to_string(),
                group: None,
                panes: vec![pane],
            }],
            total_panes: 1,
        };

        let key = PaneKey {
            session: "s9".to_string(),
            window: "w".to_string(),
            pane: "%9".to_string(),
        };

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 0,
            sessions: vec![],
            session_by_pane: HashMap::new(),
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let selected = select_active_agent_pane(&state);
        assert_eq!(selected.map(|(k, _)| k), Some(key));
    }

    #[test]
    fn test_select_active_agent_pane_prefers_pending_over_idle_when_no_in_progress() {
        let pane_pending = TmuxPane {
            pane_id: "%7".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "sp".to_string(),
            cwd: "/tmp/p".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let pane_idle = TmuxPane {
            pane_id: "%8".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "si".to_string(),
            cwd: "/tmp/i".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![
                TmuxSession {
                    name: "sp".to_string(),
                    group: None,
                    panes: vec![pane_pending],
                },
                TmuxSession {
                    name: "si".to_string(),
                    group: None,
                    panes: vec![pane_idle],
                },
            ],
            total_panes: 2,
        };

        let key_pending = PaneKey {
            session: "sp".to_string(),
            window: "w".to_string(),
            pane: "%7".to_string(),
        };
        let key_idle = PaneKey {
            session: "si".to_string(),
            window: "w".to_string(),
            pane: "%8".to_string(),
        };

        let now = Utc::now();
        let sessions = vec![
            Session {
                id: "pending".to_string(),
                project: "proj-p".to_string(),
                project_path: "/tmp/p".to_string(),
                cwd: "/tmp/p".to_string(),
                git_branch: None,
                status: SessionStatus::Pending,
                last_active: now,
                message_counts: MessageCounts::default(),
                token_counts: TokenCounts::default(),
                queue_ops: vec![],
                model: None,
                last_user_msg: None,
                last_asst_msg: None,
            },
            Session {
                id: "idle".to_string(),
                project: "proj-i".to_string(),
                project_path: "/tmp/i".to_string(),
                cwd: "/tmp/i".to_string(),
                git_branch: None,
                status: SessionStatus::Idle,
                last_active: now,
                message_counts: MessageCounts::default(),
                token_counts: TokenCounts::default(),
                queue_ops: vec![],
                model: None,
                last_user_msg: None,
                last_asst_msg: None,
            },
        ];

        let mut session_by_pane = HashMap::new();
        session_by_pane.insert(key_pending.clone(), 0);
        session_by_pane.insert(key_idle, 1);

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 0,
            sessions,
            session_by_pane,
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let selected = select_active_agent_pane(&state);
        assert_eq!(selected.map(|(k, _)| k), Some(key_pending));
    }

    #[test]
    fn test_select_active_agent_pane_uses_idle_match_when_no_in_progress() {
        let pane = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![TmuxSession {
                name: "s".to_string(),
                group: None,
                panes: vec![pane],
            }],
            total_panes: 1,
        };

        let key = PaneKey {
            session: "s".to_string(),
            window: "w".to_string(),
            pane: "%1".to_string(),
        };

        let sessions = vec![Session {
            id: "idle".to_string(),
            project: "proj-a".to_string(),
            project_path: "/tmp/a".to_string(),
            cwd: "/tmp/a".to_string(),
            git_branch: None,
            status: SessionStatus::Idle,
            last_active: Utc::now(),
            message_counts: MessageCounts::default(),
            token_counts: TokenCounts::default(),
            queue_ops: vec![],
            model: None,
            last_user_msg: None,
            last_asst_msg: None,
        }];

        let mut session_by_pane = HashMap::new();
        session_by_pane.insert(key.clone(), 0);

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 0,
            sessions,
            session_by_pane,
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let selected = select_active_agent_pane(&state);
        assert_eq!(selected.map(|(k, _)| k), Some(key));
    }

    #[test]
    fn test_active_agent_candidates_respects_in_progress_priority() {
        let pane_in_progress = TmuxPane {
            pane_id: "%2".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s1".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let pane_pending = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s2".to_string(),
            cwd: "/tmp/b".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![
                TmuxSession {
                    name: "s1".to_string(),
                    group: None,
                    panes: vec![pane_in_progress],
                },
                TmuxSession {
                    name: "s2".to_string(),
                    group: None,
                    panes: vec![pane_pending],
                },
            ],
            total_panes: 2,
        };

        let key_in_progress = PaneKey {
            session: "s1".to_string(),
            window: "w".to_string(),
            pane: "%2".to_string(),
        };

        let key_pending = PaneKey {
            session: "s2".to_string(),
            window: "w".to_string(),
            pane: "%1".to_string(),
        };

        let now = Utc::now();
        let sessions = vec![
            Session {
                id: "in_progress".to_string(),
                project: "proj-a".to_string(),
                project_path: "/tmp/a".to_string(),
                cwd: "/tmp/a".to_string(),
                git_branch: None,
                status: SessionStatus::InProgress,
                last_active: now,
                message_counts: MessageCounts::default(),
                token_counts: TokenCounts::default(),
                queue_ops: vec![],
                model: None,
                last_user_msg: None,
                last_asst_msg: None,
            },
            Session {
                id: "pending".to_string(),
                project: "proj-b".to_string(),
                project_path: "/tmp/b".to_string(),
                cwd: "/tmp/b".to_string(),
                git_branch: None,
                status: SessionStatus::Pending,
                last_active: now,
                message_counts: MessageCounts::default(),
                token_counts: TokenCounts::default(),
                queue_ops: vec![],
                model: None,
                last_user_msg: None,
                last_asst_msg: None,
            },
        ];

        let mut session_by_pane = HashMap::new();
        session_by_pane.insert(key_in_progress.clone(), 0);
        session_by_pane.insert(key_pending, 1);

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 0,
            sessions,
            session_by_pane,
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let candidates = active_agent_candidates(&state);
        assert_eq!(candidates.first().map(|(k, _, _, _, _)| k.clone()), Some(key_in_progress));
    }

    #[test]
    fn test_resolve_selected_index_keeps_same_pane_when_order_changes() {
        let previous = vec![
            PaneKey {
                session: "s".to_string(),
                window: "w".to_string(),
                pane: "%1".to_string(),
            },
            PaneKey {
                session: "s".to_string(),
                window: "w".to_string(),
                pane: "%2".to_string(),
            },
        ];

        let reordered = vec![
            PaneKey {
                session: "s".to_string(),
                window: "w".to_string(),
                pane: "%2".to_string(),
            },
            PaneKey {
                session: "s".to_string(),
                window: "w".to_string(),
                pane: "%1".to_string(),
            },
        ];

        assert_eq!(resolve_selected_index(&previous, 0, &reordered), 1);
        assert_eq!(resolve_selected_index(&previous, 1, &reordered), 0);
    }

    #[test]
    fn test_resolve_selected_index_clamps_when_previous_missing() {
        let previous = vec![PaneKey {
            session: "s".to_string(),
            window: "w".to_string(),
            pane: "%3".to_string(),
        }];

        let current = vec![
            PaneKey {
                session: "s".to_string(),
                window: "w".to_string(),
                pane: "%1".to_string(),
            },
            PaneKey {
                session: "s".to_string(),
                window: "w".to_string(),
                pane: "%2".to_string(),
            },
        ];

        assert_eq!(resolve_selected_index(&previous, 5, &current), 1);
    }

    #[test]
    fn test_select_active_agent_pane_sorts_numeric_pane_ids() {
        let pane_two = TmuxPane {
            pane_id: "%2".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let pane_ten = TmuxPane {
            pane_id: "%10".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![TmuxSession {
                name: "s".to_string(),
                group: None,
                panes: vec![pane_ten, pane_two],
            }],
            total_panes: 2,
        };

        let key_two = PaneKey {
            session: "s".to_string(),
            window: "w".to_string(),
            pane: "%2".to_string(),
        };

        let key_ten = PaneKey {
            session: "s".to_string(),
            window: "w".to_string(),
            pane: "%10".to_string(),
        };

        let now = Utc::now();
        let sessions = vec![
            Session {
                id: "a".to_string(),
                project: "proj-a".to_string(),
                project_path: "/tmp/a".to_string(),
                cwd: "/tmp/a".to_string(),
                git_branch: None,
                status: SessionStatus::InProgress,
                last_active: now,
                message_counts: MessageCounts::default(),
                token_counts: TokenCounts::default(),
                queue_ops: vec![],
                model: None,
                last_user_msg: None,
                last_asst_msg: None,
            },
            Session {
                id: "b".to_string(),
                project: "proj-a".to_string(),
                project_path: "/tmp/a".to_string(),
                cwd: "/tmp/a".to_string(),
                git_branch: None,
                status: SessionStatus::InProgress,
                last_active: now,
                message_counts: MessageCounts::default(),
                token_counts: TokenCounts::default(),
                queue_ops: vec![],
                model: None,
                last_user_msg: None,
                last_asst_msg: None,
            },
        ];

        let mut session_by_pane = HashMap::new();
        session_by_pane.insert(key_ten.clone(), 0);
        session_by_pane.insert(key_two.clone(), 1);

        let first_state = AppState {
            tmux_workspace: Some(ws.clone()),
            selected_pane_idx: 0,
            agent_pane_count: 2,
            sessions: sessions.clone(),
            session_by_pane: session_by_pane.clone(),
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let second_state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 1,
            agent_pane_count: 2,
            sessions,
            session_by_pane,
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        assert_eq!(select_active_agent_pane(&first_state).map(|(k, _)| k), Some(key_two));
        assert_eq!(select_active_agent_pane(&second_state).map(|(k, _)| k), Some(key_ten));
    }

    #[test]
    fn test_select_active_agent_pane_respects_selected_index() {
        let pane_a = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s1".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let pane_b = TmuxPane {
            pane_id: "%2".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s2".to_string(),
            cwd: "/tmp/b".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![
                TmuxSession {
                    name: "s1".to_string(),
                    group: None,
                    panes: vec![pane_a],
                },
                TmuxSession {
                    name: "s2".to_string(),
                    group: None,
                    panes: vec![pane_b],
                },
            ],
            total_panes: 2,
        };

        let key_a = PaneKey {
            session: "s1".to_string(),
            window: "w".to_string(),
            pane: "%1".to_string(),
        };
        let key_b = PaneKey {
            session: "s2".to_string(),
            window: "w".to_string(),
            pane: "%2".to_string(),
        };

        let now = Utc::now();
        let sessions = vec![
            Session {
                id: "a".to_string(),
                project: "proj-a".to_string(),
                project_path: "/tmp/a".to_string(),
                cwd: "/tmp/a".to_string(),
                git_branch: None,
                status: SessionStatus::InProgress,
                last_active: now,
                message_counts: MessageCounts::default(),
                token_counts: TokenCounts::default(),
                queue_ops: vec![],
                model: None,
                last_user_msg: None,
                last_asst_msg: None,
            },
            Session {
                id: "b".to_string(),
                project: "proj-b".to_string(),
                project_path: "/tmp/b".to_string(),
                cwd: "/tmp/b".to_string(),
                git_branch: None,
                status: SessionStatus::InProgress,
                last_active: now,
                message_counts: MessageCounts::default(),
                token_counts: TokenCounts::default(),
                queue_ops: vec![],
                model: None,
                last_user_msg: None,
                last_asst_msg: None,
            },
        ];

        let mut session_by_pane = HashMap::new();
        session_by_pane.insert(key_a.clone(), 0);
        session_by_pane.insert(key_b.clone(), 1);

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 1,
            agent_pane_count: 2,
            sessions,
            session_by_pane,
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let selected = select_active_agent_pane(&state);
        assert_eq!(selected.map(|(k, _)| k), Some(key_b));
    }

    #[test]
    fn test_build_tmux_tree_entries_includes_all_active_panes() {
        let pane_one = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        let pane_two = TmuxPane {
            pane_id: "%2".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![TmuxSession {
                name: "s".to_string(),
                group: None,
                panes: vec![pane_one, pane_two],
            }],
            total_panes: 2,
        };

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 2,
            sessions: vec![],
            session_by_pane: HashMap::new(),
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let tree = build_tmux_tree_entries(&state);
        let pane_entries = tree
            .iter()
            .filter(|e| matches!(e, TmuxTreeEntry::Pane { .. }))
            .count();

        assert_eq!(pane_entries, 2);
        assert_eq!(tree.len(), 4); // Session + Window + 2 Panes
    }

    #[test]
    fn test_build_tmux_tree_entries_keeps_window_hierarchy() {
        let pane_one = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w1".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        let pane_two = TmuxPane {
            pane_id: "%2".to_string(),
            window_name: "w2".to_string(),
            window_index: "1".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![TmuxSession {
                name: "s".to_string(),
                group: None,
                panes: vec![pane_one, pane_two],
            }],
            total_panes: 2,
        };

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 2,
            sessions: vec![],
            session_by_pane: HashMap::new(),
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let tree = build_tmux_tree_entries(&state);
        let window_entries = tree
            .iter()
            .filter(|e| matches!(e, TmuxTreeEntry::Window { .. }))
            .count();

        assert_eq!(window_entries, 2);
        assert_eq!(tree.len(), 5); // Session + 2*(Window + Pane)
    }

    #[test]
    fn test_build_tmux_tree_entries_groups_same_session_window_when_priorities_differ() {
        let pane_matched = TmuxPane {
            pane_id: "%1".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp/a".to_string(),
            running_cmd: Some("claude".to_string()),
            pane_title: Some("claude".to_string()),
            pane_dead: false,
        };
        let pane_unmatched = TmuxPane {
            pane_id: "%2".to_string(),
            window_name: "w".to_string(),
            window_index: "0".to_string(),
            session_name: "s".to_string(),
            cwd: "/tmp/other".to_string(),
            running_cmd: Some("zsh".to_string()),
            pane_title: Some("✳ Claude Code".to_string()),
            pane_dead: false,
        };

        let ws = TmuxWorkspace {
            sessions: vec![TmuxSession {
                name: "s".to_string(),
                group: None,
                panes: vec![pane_unmatched, pane_matched],
            }],
            total_panes: 2,
        };

        let now = Utc::now();
        let sessions = vec![Session {
            id: "a".to_string(),
            project: "proj-a".to_string(),
            project_path: "/tmp/a".to_string(),
            cwd: "/tmp/a".to_string(),
            git_branch: Some("main".to_string()),
            status: SessionStatus::InProgress,
            last_active: now,
            message_counts: MessageCounts::default(),
            token_counts: TokenCounts::default(),
            queue_ops: vec![],
            model: None,
            last_user_msg: None,
            last_asst_msg: None,
        }];

        let key_matched = PaneKey {
            session: "s".to_string(),
            window: "w".to_string(),
            pane: "%1".to_string(),
        };
        let mut session_by_pane = HashMap::new();
        session_by_pane.insert(key_matched, 0);

        let state = AppState {
            tmux_workspace: Some(ws),
            selected_pane_idx: 0,
            agent_pane_count: 2,
            sessions,
            session_by_pane,
            aggregated_tokens: AggregatedTokens::default(),
            refresh_countdown: 0,
            tmux_socket: None,
        };

        let tree = build_tmux_tree_entries(&state);
        let window_entries = tree
            .iter()
            .filter(|e| matches!(e, TmuxTreeEntry::Window { .. }))
            .count();
        let pane_entries = tree
            .iter()
            .filter(|e| matches!(e, TmuxTreeEntry::Pane { .. }))
            .count();

        assert_eq!(window_entries, 1);
        assert_eq!(pane_entries, 2);
        assert_eq!(tree.len(), 4); // Session + Window + 2 Panes
    }
}
