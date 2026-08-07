# vigil

A lightweight macOS resource monitor: a Rust metrics collector (CPU/memory/swap/disks/battery)
with a terminal dashboard, plus a separate layer on the
[Claude Agent SDK](https://github.com/anthropics/claude-agent-sdk-python) that investigates a
snapshot (and, if needed, the live system) and answers questions like "why is it slow" or
"why is disk space low".

The split is deliberate: **Rust never decides or fixes anything** — it only cheaply
collects metrics (no network, no LLM) and draws them. Diagnosis and recommendations are
a separate Python process. The agent can *inspect* the live system (logs, `sample`,
`vm_stat`, `du`, ...) but is blocked at the tool level from modifying it — no Write/Edit,
no kill/rm/mv/sudo/shutdown. It only ever produces text; nothing on your machine changes
without you doing it yourself.

## Features

- `vigil snapshot` — a single JSON snapshot to stdout (for scripts/the agent)
- `vigil watch` — continuously appends snapshots to JSONL + fires native macOS
  notifications on detected anomalies (high load average, active swap, a process
  holding CPU for several consecutive samples, low disk space, low battery)
- `vigil ui` — a live terminal dashboard (CPU/MEM sparklines, top processes,
  battery % with a drain-rate ETA when discharging), `a` key — ask the agent a
  question right from the interface
- `vigil incidents` — list or show saved auto-diagnoses from the terminal, without
  needing an already-open `ui` session (a TUI can't pop itself open on a push
  notification): `vigil incidents` lists recent ones, `vigil incidents --show <name>`
  prints one in full (accepts a filename or any substring that matches exactly one)

When `high_load`, `cpu_hog`, or `battery_low` fires, vigil also asks the agent to
investigate in a background thread — non-blocking, a follow-up notification with the
answer once it's done, and the diagnosis is saved as a markdown file in
`~/.vigil/incidents/<date>-<time>-<slug>.md` (override with `--incidents-dir`). Disk
and plain memory-pressure alerts don't auto-trigger the agent, and the interactive `a`
flow is UI-only — neither writes to the incident journal. If multiple alerts name the
same process within a couple of minutes (e.g. a `cpu_hog:<pid>` alert immediately
followed by `high_load` for that same process), only the first spawns an
investigation — each one is a real `uv run` + Claude Agent SDK session, so investigating
the same root cause three times over wastes exactly the CPU/battery budget vigil
exists to protect. A different process firing in the same window still gets its own
investigation.

Battery ETA is a plain drain-rate extrapolation from the percentage vigil observes over
time — there's no per-process power attribution (that needs `powermetrics
--show-process-energy`, which requires sudo; deliberately not added). The low-battery
alert instead points at the heaviest CPU consumer in the snapshot as the best available
proxy, and the agent can dig further (e.g. `pmset -g therm`) if asked.

Notifications and the agent **only suggest** — they never kill, delete, or modify
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

# browse past auto-diagnoses from a plain shell
./target/release/vigil incidents
./target/release/vigil incidents --show cpu-hog-64955
```

## Tests

```bash
cargo test                      # UI rendering via TestBackend, alerts, battery trend/parsing
cd agent && uv run pytest       # prompt building + the tool-access safety rails
```

The TUI isn't tested by hand — `ratatui::backend::TestBackend` renders frames into a
buffer, and tests assert on the buffer's content without a real terminal.

## Architecture

```
vigil (Rust)                          agent/ (Python, Claude Agent SDK)
├── snapshot/watch/ui                 ├── prompts.py    — pure prompt building (tested without network)
├── alerts.rs — threshold rules       ├── diagnose.py   — query() to Claude: Bash/Read/Grep/Glob allowed,
│   (no LLM, no network)              │                    Write/Edit + destructive Bash patterns denylisted
├── battery.rs — drain-rate ETA       └── cli.py        — vigil-agent ask --snapshot F --question Q
│   (no powermetrics/sudo)
├── incidents.rs — markdown journal
│   for auto-diagnoses only
│   (~/.vigil/incidents/)
└── agent.rs — shell wrapper around
    `uv run vigil-agent ask ...`,
    plus the auto-diagnose trigger
```
