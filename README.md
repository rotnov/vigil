# vigil

A lightweight macOS resource monitor: a Rust metrics collector (CPU/memory/swap/disks/battery)
with a terminal dashboard, plus a separate layer on the
[Claude Agent SDK](https://github.com/anthropics/claude-agent-sdk-python) that reads a
snapshot and answers questions like "why is it slow" or "why is disk space low".

The split is deliberate: **Rust never decides or fixes anything** — it only cheaply
collects metrics (no network, no LLM) and draws them. Diagnosis and recommendations are
a separate Python process, invoked only on explicit user request (`a` key in the UI).
The tool never takes any automatic action on the system.

## Features

- `vigil snapshot` — a single JSON snapshot to stdout (for scripts/the agent)
- `vigil watch` — continuously appends snapshots to JSONL + fires native macOS
  notifications on detected anomalies (high load average, active swap, a process
  holding CPU for several consecutive samples, low disk space)
- `vigil ui` — a live terminal dashboard (CPU/MEM sparklines, top processes),
  `a` key — ask the agent a question right from the interface

When a CPU-related alert fires (`high_load` or a process holding CPU for 3+
consecutive samples), vigil also asks the agent to explain it in a background
thread — non-blocking, read-only, and a follow-up notification with the
answer. Disk/memory alerts don't auto-trigger the agent; press `a` for those.

Notifications and the agent **only suggest** — they never kill anything or delete
anything on their own.

## Install

```bash
cargo build --release
cd agent && uv sync
```

Requires an installed and logged-in [Claude Code](https://claude.com/claude-code) —
`vigil-agent` reuses its session, no separate `ANTHROPIC_API_KEY` is needed.

## Usage

```bash
# one-off snapshot
./target/release/vigil snapshot | jq .

# background monitoring with notifications every 5s
./target/release/vigil watch --interval 5 --out vigil.jsonl

# live dashboard, ask the agent a question with 'a'
./target/release/vigil ui
```

## Tests

```bash
cargo test                      # UI rendering via TestBackend, alerts, battery parsing
cd agent && uv run pytest       # pure prompt-building logic
```

The TUI isn't tested by hand — `ratatui::backend::TestBackend` renders frames into a
buffer, and tests assert on the buffer's content without a real terminal.

## Architecture

```
vigil (Rust)                          agent/ (Python, Claude Agent SDK)
├── snapshot/watch/ui                 ├── prompts.py   — pure prompt building (tested without network)
├── alerts.rs — threshold rules       ├── diagnose.py  — query() to Claude, allowed_tools=[] (read-only v1)
│   (no LLM, no network)              └── cli.py       — vigil-agent ask --snapshot F --question Q
└── agent.rs — shell wrapper around
    `uv run vigil-agent ask ...`
```
