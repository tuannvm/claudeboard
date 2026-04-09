use crate::models::{AppState, PaneKey, SessionStatus, TmuxPane};
use std::cmp::Reverse;

/// Check if a pane is running a coding agent (Claude Code, Codex, or Gemini).
/// Primary signal is running_cmd; for wrapped launches (bash/node/npx/etc) fall back to pane_title.
pub fn is_coding_agent(pane: &TmuxPane) -> bool {
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

pub fn has_word_token(text: &str, token: &str) -> bool {
    text.split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|t| !t.is_empty())
        .any(|t| t.eq_ignore_ascii_case(token))
}

pub fn is_likely_claude_pane(pane: &TmuxPane) -> bool {
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

pub fn pane_title_has_token(title: &str, token: &str) -> bool {
    has_word_token(title, token)
}

pub fn coding_agent_signal_strength(pane: &TmuxPane) -> u8 {
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

pub fn is_selectable_agent_pane(pane: &TmuxPane, matched: bool) -> Option<u8> {
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

pub fn session_status_rank(status: SessionStatus) -> u8 {
    match status {
        SessionStatus::InProgress => 3,
        SessionStatus::Pending => 2,
        SessionStatus::Idle => 1,
        SessionStatus::Done | SessionStatus::Error => 0,
    }
}

pub fn pane_numeric_id(pane_id: &str) -> Option<u32> {
    pane_id.trim_start_matches('%').parse::<u32>().ok()
}

pub fn active_agent_candidates(state: &AppState) -> Vec<(PaneKey, TmuxPane, bool, u8, u8)> {
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

pub fn resolve_selected_index(previous_keys: &[PaneKey], previous_idx: usize, new_keys: &[PaneKey]) -> usize {
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

pub fn select_active_agent_pane(state: &AppState) -> Option<(PaneKey, TmuxPane)> {
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
