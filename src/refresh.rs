use crate::agent::{active_agent_candidates, resolve_selected_index};
use crate::jsonl::scan_all_sessions;
use crate::models::AppState;
use crate::session_match::build_session_by_pane;
use crate::tmux::parse_tmux_workspace;
use crate::usage::scan_all_usage;
use parking_lot::RwLock;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;
use tokio::time::interval;

pub async fn refresh_state(state: &Arc<RwLock<AppState>>, tmux_socket: &Option<String>) {
    let sessions = tokio::task::spawn_blocking(move || {
        scan_all_sessions(7) // only last 7 days
    })
    .await
    .unwrap_or_default();

    let tokens = tokio::task::spawn_blocking(scan_all_usage)
        .await
        .unwrap_or_default();

    let tmux_socket_clone = tmux_socket.clone();
    let tmux_ws = tokio::task::spawn_blocking(move || parse_tmux_workspace(&tmux_socket_clone))
        .await
        .unwrap_or(None);

    let session_by_pane = build_session_by_pane(&sessions, &tmux_ws);

    let (selected, previous_keys) = {
        let s = state.read();
        let previous_keys = active_agent_candidates(&s)
            .into_iter()
            .map(|(k, _, _, _, _)| k)
            .collect::<Vec<_>>();
        (s.selected_pane_idx, previous_keys)
    };

    let mut s = state.write();
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

pub fn spawn_refresh_loop(
    state: Arc<RwLock<AppState>>,
    refresh_interval: u64,
    tmux_socket: Option<String>,
) -> JoinHandle<()> {
    tokio::spawn(async move {
        let mut ticker = interval(Duration::from_secs(1));
        let mut countdown = refresh_interval;

        loop {
            ticker.tick().await;

            // Sync countdown from state (manual refresh via 'r')
            {
                let sc = state.read().refresh_countdown;
                if sc == 0 {
                    state.write().refresh_countdown = refresh_interval;
                    countdown = 0;
                } else {
                    countdown = countdown.saturating_sub(1);
                }
            }

            if countdown == 0 {
                countdown = refresh_interval;
                refresh_state(&state, &tmux_socket).await;
            }

            state.write().refresh_countdown = countdown;
        }
    })
}
