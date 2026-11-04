# clop

I run a lot of Claude Code sessions. Some are on this machine, some are on another machine over ssh, and I usually keep btop open in another pane so I can see what's going on. But btop shows me *everything*, and what I want to know is narrower. Which sessions are busy? What are they running? And what did they leave behind in `/tmp` and `~/worktrees` that I'll be cleaning up by hand on Friday?

clop is a btop-style terminal dashboard that only cares about Claude Code. It watches your sessions, everything they spawn, and the leftovers they create, and when you're ready to tidy up, it hands you the cleanup script.

clop is Linux-only for now. More on that [below](#macos).

## What you see

The main screen has a header, a sessions table with a CPU chart, and a bottom row that switches between four panels.

**Header.** A bar for each CPU thread, a two-row CPU history with Claude's share stacked in orange under the machine total, load average, memory, swap and disk. The third line of the memory and disk columns shows how much RAM Claude's processes are using and how big `~/.claude` plus `/tmp/claude-<uid>` have grown.

**Sessions.** One row per running session: status, model, context size, and CPU, memory and process count summed over the session's whole process tree. The disk column is the session's transcript, scratchpad and file history. Active subagents get their own rows underneath. Two flags show up when they apply:

- `spinning?` means 30 or more tool calls and 15 minutes with no edits since your last prompt.
- `idle child` means something the session started has used no CPU for 5 minutes, like a dev server nobody is looking at.

**Activity** (`f`). Every tool call across all sessions, newest first. Bash calls show the programs in the command (`git|tail`) next to Claude's own one-line description of what it's doing. Risky commands like `rm -rf`, `git push --force` and `git reset --hard` show up in red. Next to the feed is the process tree for the selected session. MCP servers are labeled by name, and finished processes fade out over a few seconds instead of vanishing, so you can actually see that `grep` ran.

**Programs** (`p`). Two leaderboards with a 14-day trend for each program:

- **Asked** is what Claude typed, counted from the transcripts. The first run backfills from every transcript on disk.
- **Ran** is what actually started, from the kernel's exec events.

They don't agree, and that's the interesting part. Claude Code's shell snapshot replaces `grep` and `find` with its own embedded ugrep and bfs, and your aliases apply too (my `ls` is `eza`). clop shows those as `grep (ugrep)`, `find (bfs)` and `eza`.

**Skills** (`s`). Skills Claude invoked, slash commands you typed, and installed skills nobody has used in the selected time window.

**Cleanup** (`c`). Everything ended sessions left behind. More on this next.

## Cleanup

The cleanup panel is report-only. clop never deletes anything. Each item comes with the shell command that would clean it up, and you decide.

| kind | how clop finds it | the cleanup command |
|---|---|---|
| process | carries the `CLAUDE_PID` of a session that's gone | `kill` |
| worktree | `git worktree add` in a transcript, or under `.claude/worktrees` | `git worktree remove` |
| file | inside a temp folder, created within 30 minutes after Claude named it | `rm` |
| scratch | scratchpads of ended sessions, one row per project | `rm -rf` |
| git refs | custom ref namespaces Claude fetched into, like `refs/remotes/pr/*` | `update-ref --stdin` |
| image, container | `podman` or `docker` `build -t` and `run --name` | `rmi`, `rm -f` |

Every rule has two halves. The evidence comes from the transcripts, and then clop checks that the thing still exists. The time check on files matters. Claude *looking* at `/tmp/dumps` doesn't make it Claude's, and neither does some other program recreating it a week later.

Worktrees with uncommitted changes get a commented-out `--force` line and a note, so you have to look before you remove them. Long-lived helpers that sessions happen to start, like rootless podman's pause process and gpg or ssh agents, are never reported.

Press `y` to copy the selected item's command, or `Y` to copy all of them as a script. clop copies through the terminal itself (OSC 52), so it should work over ssh as long as your terminal allows programs to set the clipboard. You can also skip the TUI:

```
clop --cleanup-script > cleanup.sh
```

The script runs each step independently and prints a note instead of stopping when something is already gone.

## Keys

| key | does |
|---|---|
| `q` | quit |
| `tab` | next panel (`f` feed, `p` programs, `s` skills, `c` cleanup) |
| `j` / `k` | select a session, or an item in the cleanup panel |
| `a` | feed: all sessions or just the selected one |
| `w` | programs and skills window: today, 7 days, all time |
| `y` / `Y` | copy one cleanup command / the whole script |
| `l` | show or hide cleanup items from sessions that are still running |
| `r` | rescan for leftovers now |

## Install

You need Rust 1.89 or newer.

```
cargo install --git https://github.com/carloseberhardt/clop
```

If you want to copy one binary to another machine, build it static against musl:

```
rustup target add x86_64-unknown-linux-musl
cargo build --release --target x86_64-unknown-linux-musl
```

The binary lands in `target/x86_64-unknown-linux-musl/release/clop`. It has no dependencies, so `scp` it anywhere and it runs. That's how I get it onto a second machine with an older glibc.

## Settings

Everything works without a config file. To start one, run:

```
clop --default-config > ~/.config/clop/config.toml
```

Here's what you can change:

```toml
[processes]
fade_seconds = 10                  # how long finished processes linger in the tree

[janitor]
temp_dirs = ["/tmp", "~/.cache"]   # where throwaway files land (replaces the default)
ignore_paths = []                  # never report these paths or anything under them
ignore_processes = []              # never report processes with these names
ignore_refs = []                   # never report refs under these prefixes
created_within_minutes = 30
```

The three `ignore_` lists add to the built-in ones. `ignore_paths` is the one you'll want if a tool of yours keeps a cache in `~/.cache` on purpose.

If the file has a typo, clop starts anyway with the defaults and tells you what's wrong, both in the terminal and in the cleanup panel.

## Where the data comes from

clop reads, and never writes, these:

- `~/.claude/sessions/<pid>.json` for the running sessions
- `~/.claude/projects/**/*.jsonl` for transcripts, including subagents
- `/proc` for processes, CPU, memory and environments. Every process a session spawns carries `CLAUDE_PID`, which is how clop recognizes orphans even after they've been reparented to init.
- the kernel's proc connector (netlink) for exec events. It works without root on the kernels I've tried (7.2 on Arch and Fedora).

It writes two things. The program and skill counts go in `~/.local/state/clop/counts.json`. It also keeps a lock file there, so if you run two clops on one machine, only one counts and the other displays its numbers.

## macOS

Most of my coworkers are on Macs, so this comes up. The parts that read `~/.claude` (sessions, activity, Asked counts, skills, the cleanup evidence) should port as-is. The process tree, CPU and memory need a macOS version, since there's no `/proc`. The Ran counter is probably Linux-only: as far as I know, macOS has no unprivileged way to watch every program start.

The Linux-specific code lives in `procs.rs`, `sys.rs` and `execmon.rs`. If you're on a Mac and want this, pull requests are very welcome.

## License

MIT
