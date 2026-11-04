# CLAUDE.md

clop is a btop-style terminal dashboard for Claude Code sessions (Rust, ratatui). See README.md for what it shows.

## Build and test

```
cargo build
cargo test
cargo run -- --once            # print what clop sees, no TUI
cargo run -- --cleanup-script  # print the janitor's cleanup script
cargo build --release --target x86_64-unknown-linux-musl   # static binary
```

To look at the TUI without a terminal of your own, run it in a detached tmux session and use `tmux capture-pane -p`.

## Layout

- `procs.rs`: /proc scanner, CPU% between scans, session attribution (ancestry first, then the `CLAUDE_PID` env var)
- `sys.rs`: machine CPU (total and per thread), memory, swap, disks
- `execmon.rs`: kernel exec events over the netlink proc connector
- `sessions.rs`: `~/.claude/sessions/<pid>.json`, transcripts, subagents, MCP server names, disk footprint
- `transcript.rs`: incremental JSONL reader, tool-call events, program-name parsing, risky-command check
- `counts.rs`: persisted asked/ran/skill/slash counts in `~/.local/state/clop/counts.json`
- `janitor.rs`: leftovers from ended sessions, and their cleanup commands
- `config.rs`: `~/.config/clop/config.toml`
- `app.rs`: state and the per-second tick; `ui.rs`: drawing; `main.rs`: keys and CLI flags

## Things to keep true

- The janitor is report-only. clop never deletes, kills or changes anything; it only produces commands for the user to run.
- Only one clop per machine counts. The instance holding `counter.lock` counts and saves; others re-read the file. Every instance still listens to exec events to draw finished processes in its own tree.
- A temp entry is a leftover only if it was created within `created_within_minutes` after Claude named it. A later mention, or something else recreating the path, doesn't count.
- Claude Code's shell snapshot runs `grep` and `find` through its own binary with argv0 `ugrep` and `bfs`. clop reports them as `grep (ugrep)` and `find (bfs)`, so "asked" and "ran" differ by design.
- `procs.rs`, `sys.rs` and `execmon.rs` are Linux-only. Keep platform code in those files so a macOS port has clear places to plug in.
- Test fixtures and examples use generic names. Never put real hostnames, employer details or paths from a user's machine in the repo.
