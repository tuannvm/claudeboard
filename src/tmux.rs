use crate::models::{TmuxPane, TmuxSession, TmuxWorkspace};

// ============================================================================
// Tmux Parsing
// ============================================================================

pub fn parse_tmux_workspace(socket: &Option<String>) -> Option<TmuxWorkspace> {
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
