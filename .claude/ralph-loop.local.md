---
active: true
iteration: 1
max_iterations: 15
completion_promise: "DONE"
started_at: "2026-04-04T04:49:44Z"
---

Thoroughly review and enhance the claude-watch TUI dashboard at /Users/tuannvm/project/cli/claudeboard/src/main.rs.

## Current Architecture
- Rust TUI: ratatui 0.26 + crossterm 0.27, parking_lot RwLock, tokio
- 3-panel layout: LEFT=tmux tree (40%), RIGHT TOP=session queue (60%), RIGHT BOTTOM=tokens (40%)
- Tokyo Night color scheme
- Sessions filtered to ≤7 days

## Current Issues to Address

1. **Confusing session hierarchy** — The tmux tree shows Liftoff, Liftoff_dev_ttys004_, Personal, ccc as separate sessions. But many of these appear to be grouped (Liftoff and Liftoff_dev_ttys004_ are in the same group). The tree should clearly show the actual hierarchy: sessions > windows > panes.

2. **Duplicate information** — Looking at the current output, Liftoff and Liftoff_dev_ttys004_ show almost identical content. This is likely because they're attached to the same tmux session group. The dashboard should either:
   - Deduplicate grouped sessions and show only one
   - Or clearly indicate which session is "primary" vs "secondary"

3. **Window names are confusing** — Windows like "Projects", "jaeger", "knowledge-base" appear under different sessions. These are actually different tmux windows, but the dashboard shows them with identical names making it hard to distinguish.

4. **Pane identification** — Panes are shown with pane_id like "%72" but this isn't very informative. The pane's running command (like "claude-watch", "2.1.89") would be more useful.

5. **Session status panel issues**:
   - When navigating with j/k, the right panel should update to show the selected pane's session
   - The queue operations (enqueue/dequeue) don't have clear timestamps
   - Some sessions show "no operations" which might mean they're genuinely idle or there's a data gap

6. **Token usage panel** — Shows all-time totals but not today's actual usage (today: 0 tokens seems wrong if there are active sessions)

7. **Empty/busy states** — Need to handle gracefully:
   - No tmux running
   - No sessions found
   - No agent panes
   - tmux running but no coding agents

## What the Dashboard Should Provide

### Overview Panel (Status Bar)
- Total active sessions count
- Sessions by status: ⚡in-progress, ○pending, ○idle, ✓done, ✗error
- Token usage summary (today vs all-time)

### Tmux Tree Panel (Left)
- Sessions grouped logically (tmux groups should be collapsed or marked)
- Windows under each session with window name and window index
- Panes under each window showing:
  - Pane ID (or more useful identifier)
  - Running command/version (e.g., "2.1.89", "claude-watch")
  - Title if set
  - Matched session info (repo/branch) if available
  - Selection indicator
- Clear tree branch characters (├──, └──, │)

### Session Queue Panel (Right Top)
- For the selected pane, show:
  - Session ID and project name
  - Git branch if available
  - Status with colored icon
  - Last active timestamp
  - Message counts (assistant/user/system)
  - Token breakdown (input/output/cache)
  - Recent queue operations with timestamps

### Token Usage Panel (Right Bottom)
- Daily gauge (0 to 1.5M or configurable limit)
- Today's tokens and cost
- All-time tokens and cost
- Last hour rate
- By-model breakdown

### Navigation
- j/k or arrows to navigate panes
- g to go to first pane
- G to go to last pane
- r to force refresh
- q to quit

## Review Checklist
- [ ] Read the full main.rs and understand all data flows
- [ ] Verify tmux parsing handles all edge cases (groups, detached, etc.)
- [ ] Fix session deduplication/grouping issue
- [ ] Improve pane labels to show running command instead of just pane_id
- [ ] Ensure selected pane selection is visually clear
- [ ] Handle all empty states gracefully
- [ ] Verify token parsing is correct
- [ ] Build: cargo build --release must be clean
- [ ] Test navigation works correctly

After review: cargo build --release to confirm zero errors.
