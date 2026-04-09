use crate::models::{PaneKey, Session, TmuxWorkspace};
use std::collections::HashMap;
use std::path::PathBuf;

pub fn build_session_by_pane(sessions: &[Session], tmux_ws: &Option<TmuxWorkspace>) -> HashMap<PaneKey, usize> {
    let mut session_by_pane = HashMap::new();

    if let Some(ws) = tmux_ws {
        for (sess_idx, session) in sessions.iter().enumerate() {
            for tmux_session in &ws.sessions {
                for pane in &tmux_session.panes {
                    let pane_cwd = &pane.cwd;
                    let session_cwd = &session.cwd;
                    let session_project = &session.project;

                    let matched = {
                        // Strategy 1: cwd-based matching (requires depth >= 4)
                        let session_path = PathBuf::from(session_cwd);
                        let session_depth = session_path.components().count();
                        let cwd_match = session_depth >= 4 && pane_cwd.starts_with(session_cwd);

                        // Strategy 2: project-name matching (pane cwd ends with project dir name)
                        // This is more reliable when session.cwd is ~/.claude or empty
                        let project_name = std::path::Path::new(session_project)
                            .file_name()
                            .and_then(|n| n.to_str())
                            .unwrap_or(session_project);
                        let project_match =
                            pane_cwd.ends_with(project_name) || pane_cwd.ends_with(session_project);

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

    session_by_pane
}
