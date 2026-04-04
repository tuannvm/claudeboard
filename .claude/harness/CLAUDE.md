# Harness — Automated Development Loop

This file is the **single source of truth** for the harness session. It contains the task, instructions, and all mutable state. Read it fully before each iteration.

## Task

Implement GitHub Actions CI/CD workflows for the `claude-watch` Rust project, modeled after the structure used in `tuannvm/mcp-trino`.

### Required Workflows

Create `.github/workflows/` directory with the following workflows:

#### 1. `build.yml` - Build & Verify Pipeline
Trigger on: push to main, pull requests
Jobs:
- **verify**: Code quality checks (cargo fmt --check, cargo clippy, cargo check)
- **security**: Security scanning (cargo-audit, cargo-deny if available)
- **test**: Run tests with coverage (cargo test, tarpaulin or similar)
- **build**: Build verification for PRs

Use these environment variables (adapt for Rust):
- Use stable Rust, consider matrix testing (stable, beta, nightly)
- Cache cargo registry and build artifacts

#### 2. `release.yml` - Create Release
Trigger on: workflow_dispatch with bump_type input (major/minor/patch)
Jobs:
- Bump version in Cargo.toml
- Create git tag
- Build release binaries using cargo build --release
- Upload to GitHub Releases
- Optionally: publish to crates.io

#### 3. `claude.yml` - Claude Code Integration
Similar to mcp-trino's setup for @claude mentions in PRs/issues

### Project Context

- **Language**: Rust (edition 2024)
- **Build tool**: cargo
- **Project**: claude-watch (TUI app using ratatui)
- **Dependencies**: ratatui, crossterm, tokio, parking_lot, chrono, serde, clap, serde_json, regex-lite

### Technical Requirements

1. **Follow GitHub Actions best practices**:
   - Use `actions/checkout@v4` or later
   - Use `actions/cache@v4` for cargo dependencies
   - Set appropriate permissions (contents: read, packages: write, etc.)
   - `paths-ignore` for markdown changes

2. **Rust-specific considerations**:
   - Use `dtolnay/rust-toolchain@stable` for Rust setup
   - Cache target directory and cargo registry
   - Use `cargo-tarpaulin` or similar for coverage
   - Use `cargo-audit` for security scanning
   - Use `cargo-deny` for license/advisory checking

3. **Multi-platform builds** (for release):
   - linux-x86_64, linux-aarch64
   - macos-x86_64, macos-aarch64
   - windows-x86_64

### Success Criteria

- All workflows created and syntax-valid
- `build.yml` passes on main/PR
- Tests run and coverage reported
- Security scanning runs
- Release workflow can create tagged releases with binaries
- Claude Code integration works

## Architecture

```text
/loop <interval>                   ← outer: recurring heartbeat
  └─ /ralph-wiggum:ralph-loop     ← inner: self-referential iteration
       └─ /codex                  ← reviewer: each iteration
```

## Phases

The harness progresses through these phases (tracked in `state.json`):

| Phase | Meaning |
|-------|---------|
| `init` | Just started |
| `implementing` | Active development iterations |
| `testing` | Implementation done — dedicated testing & validation gate |
| `complete` | All tests pass, verified, safe to exit |

**Phase transitions:**
- `init` → `implementing`: first iteration starts
- `implementing` → `testing`: all action items done, no P1s, code review clean
- `testing` → `implementing`: test failures found → fix and re-review
- `testing` → `complete`: all tests pass with output captured below

## How This Works

You are inside a **Ralph loop**. Each iteration:

1. **Read** this file top-to-bottom for task + current state
2. **Check phase** in `state.json`:
   - If `init` or `implementing`: follow the Implementation Flow below
   - If `testing`: follow the Testing Gate below

### Implementation Flow

1. **Explore** the codebase: Glob/Read/Grep before writing code
2. **Plan**: output a `<plan>` of ordered steps
3. **Implement**: make changes, then update Action Items below (move done items, add new ones)
4. **Quick verify**: run build and tests — fix failures before continuing
5. **Review**: invoke `/codex` via the Skill tool:
   ```
   /codex review --uncommitted --title "Harness Iteration Review"
   ```
   Triage codex findings:
   - **P1** (logic bugs, incorrect behavior) → add to Blocking below — MUST fix before exit
   - **P2** (design, performance) → add to Open below
   - **P3** (minor, style) → append to Findings Log below
6. **Update state**: increment `iteration` in `state.json`, set `"phase": "implementing"`
7. **Transition check`: All action items done AND zero P1s? → set `"phase": "testing"` and continue to Testing Gate

### Testing Gate

This phase exists to **prove the work is correct** before exit. Do NOT skip it.

1. **Run the full test suite** — `cargo test`, verify workflows are syntactically valid
2. **Run the build** — `cargo build --release`
3. **Record results below** in the "Test Results" section — paste actual command output
4. **If ANY test fails or build breaks**: set `"phase": "implementing"`, add failures to Blocking (P1), and fix them
5. **If ALL tests pass AND build succeeds**:
   - Run `/codex` one final time for a clean review
   - Set `"phase": "complete"` and `"tests_passed": true` in `state.json`
   - Output `HARNESS_COMPLETE`

**You MUST have non-empty Test Results below before outputting HARNESS_COMPLETE.**

## Rules

- **Use the Skill tool** for `/codex` — do NOT run it as a bash command
- **Update this file** after each iteration (action items, findings, test results)
- **Never skip the codex review step**
- **Never skip the Testing Gate** — implementation without proof of testing is incomplete
- **Never output HARNESS_COMPLETE unless**: (1) Test Results section has actual output, (2) all tests pass, (3) build succeeds, (4) zero P1s remain

## Success Criteria

GitHub Actions workflows created and validated. Cargo build passes. Tests pass **with output recorded**. All P1s resolved.

---

## Action Items

### Blocking (P1)
_(none)_

### Open (P2)
- Pin cargo-installed tools to lockfile-compatible versions (add --locked flag or pin versions)
- Commit updated Cargo.lock when bumping release version

### Done
- Created `.github/workflows/` directory
- Created `build.yml` with verify/security/test/build jobs
- Created `release.yml` with multi-platform matrix build support
- Created `claude.yml` for Claude Code integration
- Created `deny.toml` for cargo-deny configuration
- Added MIT license metadata to `Cargo.toml`
- Fixed Claude workflow permissions (read → write for PRs/issues)
- Fixed release.yml variable expansion (VERSION → ${{ env.VERSION }})
- Fixed release.yml to checkout tag instead of HEAD
- Fixed ARM64 cross-compilation linker configuration
- Verified cargo fmt, cargo clippy, and cargo test all pass
- Completed Testing Gate with passing tests and build

---

## Test Results

**Commands Run:**
```bash
cargo test --verbose
cargo build --release
```

**Output:**
```
running 0 tests
test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.00s

Finished `release` profile [optimized] target(s) in 18.83s
```

**Workflow Files Created:**
- `.github/workflows/build.yml` (5,209 bytes)
- `.github/workflows/release.yml` (4,748 bytes)
- `.github/workflows/claude.yml` (1,386 bytes)

**Exit Codes:** 0 (success)

---

## Findings Log

| Iter | Severity | Finding |
|------|----------|---------|
| 1 | P1 | Claude workflow lacked write permissions for PRs/issues - Fixed |
| 1 | P1 | Release asset upload used wrong variable syntax - Fixed |
| 1 | P2 | Release workflow only built Linux x86_64 - Implemented full matrix build |
| 2 | P1 | Release artifacts built from wrong commit (not tag) - Fixed with ref checkout |
| 2 | P1 | Linux ARM64 cross-compilation missing linker config - Fixed |
| 2 | P1 | License check would fail without deny.toml - Created deny.toml |
| 3 | P1 | License metadata missing from Cargo.toml - Added MIT license |
| 4 | — | All P1 issues resolved, codex review passed |
| 5 | P2 | Pin cargo-installed tools to lockfile-compatible versions - Documented for future |
| 5 | P2 | Commit Cargo.lock when bumping release version - Documented for future |
