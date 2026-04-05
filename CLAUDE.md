# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

## Build
- `cargo build --release` — build binary to `target/release/claudeboard`
- `cargo check` — type check without full build

## UTF-8 Safety
This codebase handles multi-byte UTF-8 characters. When truncating strings for display:
- **Never** use byte slicing like `&str[..32]` or `&str[len-32..]`
- **Always** use character-based truncation: iterate `chars()` or use `chars().rev().take()`
- When using `char_indices()`, track character count, not byte index

## TUI Framework
- ratatui 0.26 with crossterm 0.27
- Colors: Tokyo Night scheme (see `colors` module in main.rs)
- parking_lot::RwLock for shared state, tokio for async tmux operations
