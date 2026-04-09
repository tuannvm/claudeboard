use chrono::{DateTime, Utc};
use std::collections::HashMap;

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
