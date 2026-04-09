use crossterm::event::{KeyCode, KeyEventKind};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

mod agent;
mod cli;
mod colors;
mod jsonl;
mod models;
mod pane_capture;
mod pricing;
mod tmux;
mod ui;
mod usage;
mod utils;
mod session_match;
mod refresh;
mod terminal;

use clap::Parser;
use cli::Args;
use models::*;
use refresh::spawn_refresh_loop;
use terminal::{setup_terminal, teardown_terminal};
use ui::render;

#[cfg(test)]
use chrono::Utc;
#[cfg(test)]
use agent::{
    active_agent_candidates, coding_agent_signal_strength, has_word_token, is_coding_agent,
    is_likely_claude_pane, is_selectable_agent_pane, resolve_selected_index,
    select_active_agent_pane,
};
#[cfg(test)]
use pane_capture::{filter_last_claude_response, pane_has_claude_boundary};
#[cfg(test)]
use ui::{TmuxTreeEntry, build_tmux_tree_entries};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let args = Args::parse();

    let mut terminal = setup_terminal()?;

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

    // Background refresh loop
    let _refresh_task = spawn_refresh_loop(
        state.clone(),
        args.refresh_interval,
        args.tmux_socket.clone(),
    );

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
                    teardown_terminal()?;
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
