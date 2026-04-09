use crate::agent::{active_agent_candidates, is_likely_claude_pane, select_active_agent_pane};
use crate::colors;
use crate::models::{AppState, PaneKey, QueueOp, SessionStatus, TmuxPane};
use crate::pane_capture::{capture_pane_content, filter_last_claude_response, pane_has_claude_boundary};
use crate::pricing::compute_cost;
use crate::utils::truncate_from_end;
use chrono::Utc;
use ratatui::layout::{Constraint, Direction, Layout, Rect};
use ratatui::style::Styled;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Paragraph};
use ratatui::Frame;
use std::collections::HashMap;

// ============================================================================
// UI Helpers
// ============================================================================

pub fn session_status_icon(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::InProgress => "●",
        SessionStatus::Pending => "○",
        SessionStatus::Idle => "○",
        SessionStatus::Done => "✓",
        SessionStatus::Error => "✗",
    }
}

pub fn session_status_color(status: SessionStatus) -> Color {
    match status {
        SessionStatus::InProgress => colors::GREEN,
        SessionStatus::Pending => colors::YELLOW,
        SessionStatus::Idle => colors::SECONDARY,
        SessionStatus::Done => colors::CYAN,
        SessionStatus::Error => colors::RED,
    }
}

pub fn queue_op_icon(op: &str) -> &'static str {
    match op {
        "running" => "●",
        "enqueue" => "○",
        "complete" => "✓",
        "failed" => "✗",
        "dequeue" => "·",
        _ => "?",
    }
}

pub fn queue_op_color(op: &str) -> Color {
    match op {
        "running" => colors::GREEN,
        "enqueue" => colors::YELLOW,
        "complete" => colors::CYAN,
        "failed" => colors::RED,
        "dequeue" => colors::SECONDARY,
        _ => colors::PRIMARY,
    }
}

pub fn format_tokens(n: u64) -> String {
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
pub enum TmuxTreeEntry {
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

pub fn format_tmux_pane_label(pane: &TmuxPane) -> String {
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

pub fn build_tmux_tree_entries(state: &AppState) -> Vec<TmuxTreeEntry> {
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

pub fn render_tmux_panel(f: &mut Frame, area: Rect, state: &AppState) {
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

pub fn render_live_pane(f: &mut Frame, area: Rect, state: &AppState) {
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

pub fn render_session_metadata(f: &mut Frame, area: Rect, state: &AppState) {
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

pub fn render_status_bar(f: &mut Frame, area: Rect, state: &AppState) {
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

pub fn render(f: &mut Frame, area: Rect, state: &AppState) {
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
