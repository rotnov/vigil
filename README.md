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
  holding CPU for several consecutive samples, low disk space, low battery, an
  unusually high TCP connection count, or unusual incoming connections)
- `vigil ui` — a live terminal dashboard (CPU/MEM sparklines, top processes,
  battery % with a drain-rate ETA when discharging), `a` key — ask the agent a
  question right from the interface
- `vigil incidents` — list or show saved auto-diagnoses from the terminal, without
  needing an already-open `ui` session (a TUI can't pop itself open on a push
  notification): `vigil incidents` lists recent ones, `vigil incidents --show <name>`
  prints one in full (accepts a filename or any substring that matches exactly one)
- `vigil menubar` — a macOS menu bar status item: transparent/faint when nothing's
  open, yellow for one open incident, red for multiple. Click for a dropdown of
  recent incidents (opens the markdown file). Polls the status file `vigil watch`
  writes each tick rather than sampling on its own — see
  [docs/decisions/0002-menu-bar-health-indicator.md](docs/decisions/0002-menu-bar-health-indicator.md)

When `high_load`, `cpu_hog`, or `battery_low` fires, vigil also asks the agent to
investigate in a background thread — non-blocking, a follow-up notification with the
answer once it's done, and the diagnosis is saved as a markdown file in
`~/.vigil/incidents/<date>-<time>-<slug>.md` (override with `--incidents-dir`). Disk
and plain memory-pressure alerts don't auto-trigger the agent, and the interactive `a`
flow is UI-only — neither writes to the incident journal.

Repeat firings for the same process are treated as one ongoing incident, not a fresh
one each time: `alerts::IncidentTracker` skips the notification, the diagnosis, and
the journal entry for a target that already has one open (still firing within twice
the alert cooldown of its last firing), and only starts a new incident once that
target has been quiet for longer than that. A different process firing in the same
window still gets its own full treatment — an incident is scoped to what it's
actually about, not to a shared time window.

Battery ETA is a plain drain-rate extrapolation from the percentage vigil observes over
time — there's no per-process power attribution (that needs `powermetrics
--show-process-energy`, which requires sudo; deliberately not added). The low-battery
alert instead points at the heaviest CPU consumer in the snapshot as the best available
proxy, and the agent can dig further (e.g. `pmset -g therm`) if asked.

Connection counts come from `netstat -an` (both `inet`/`inet6`), classified by state.
"Incoming" is a heuristic, not a kernel-verified fact — an `ESTABLISHED` connection
counts as incoming when its local port matches one of this machine's own `LISTEN`
ports and the remote peer isn't loopback; see
[docs/decisions/0001-network-connection-monitoring.md](docs/decisions/0001-network-connection-monitoring.md)
for why, and for how the (first-guess, expect-to-tune) thresholds were picked.

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

# menu bar health indicator (run alongside `vigil watch`, not instead of it)
./target/release/vigil menubar
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
├── main.rs — snapshot collection,
│   incl. connection counts via
│   `netstat` (see docs/decisions/0001)
├── agent.rs — shell wrapper around
│   `uv run vigil-agent ask ...`,
│   plus the auto-diagnose trigger
└── menubar.rs — tray icon, polls the
    status file `watch` writes each
    tick (see docs/decisions/0002)
```

Project-wide design decisions with a real alternative (a parsing strategy, an alert
heuristic, a new subsystem) are recorded under
[`docs/decisions/`](docs/decisions/) as short ADRs. See `AGENTS.md` for the full
project rule set (language, testing conventions, the live incident-monitoring loop).
