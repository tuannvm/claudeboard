// ============================================================================
// Tmux Pane Capture
// ============================================================================

/// Extract the last Claude response from raw pane lines.
/// Claude responses appear between ██████ markers and user `> ` prompts.
/// Returns only the content of the last Claude message, filtered.
pub fn pane_has_claude_boundary(lines: &[String]) -> bool {
    lines.iter().any(|l| l.contains("██████"))
}

pub fn filter_last_claude_response(lines: &[String]) -> Vec<String> {
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
pub fn capture_pane_content(
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
