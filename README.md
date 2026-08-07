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
  unusually high TCP connection count, or unusual incoming connections). Swap/
  memory alerts name whichever is bigger — the single top process, or a process
  *group* (every same-named instance combined, e.g. a dozen renderer helpers that
  individually never rank at the top) when the group is at least 1.5x larger. When a
  CPU-related alert's own top consumer turns out to be `vigil` itself, the message
  says so explicitly instead of reading like any other app to restart — vigil's own
  overhead counts against the same performance goal it exists to protect, so this is
  never hidden or excluded, just made legible when it happens
- `vigil ui` — a live terminal dashboard (CPU/MEM sparklines, top processes with a
  ↑/↓/→ trend arrow on memory over the last 10 samples, battery % with a drain-rate
  ETA when discharging). `a` key — ask the agent a free-form question. `w` key —
  ask, pre-filled, why the current #1 CPU process is using what it's using
- `vigil incidents` — list or show saved auto-diagnoses from the terminal, without
  needing an already-open `ui` session (a TUI can't pop itself open on a push
  notification): `vigil incidents` lists recent ones, `vigil incidents --show <name>`
  prints one in full (accepts a filename or any substring that matches exactly one)
- `vigil menubar` — a macOS menu bar status item: an eye icon (drawn procedurally,
  not a bundled asset), transparent/faint when nothing's open, yellow for one open
  incident, red for multiple. Click for a dropdown of recent incidents (opens the
  markdown file). Polls the status file `vigil watch` writes each tick rather than
  sampling on its own — see
  [docs/decisions/0002-menu-bar-health-indicator.md](docs/decisions/0002-menu-bar-health-indicator.md)

Every agent answer — interactive `a` or auto-triggered — ends with a token/cost
footer (`_Tokens: N in / M out (+K cache read) — ~$X_`), since the agent's own
token spend is part of the overhead this project tries to keep visible, not hide.

When `high_load`, `cpu_hog`, or `battery_low` fires, vigil also asks the agent to
investigate in a background thread — non-blocking, a follow-up notification with the
answer once it's done, and the diagnosis is saved as a markdown file in
`~/.vigil/incidents/<date>-<time>-<slug>.md` (override with `--incidents-dir`). Disk
and plain memory-pressure alerts don't auto-trigger the agent, and the interactive `a`
flow is UI-only — neither writes to the incident journal.

Because that investigation runs seconds to minutes after the alert fired, the flagged
process's pid can already have been recycled by the OS to something unrelated by the
time the agent checks it — observed live (see
`2026-08-07-14-20-56-cpu-hog-27339.md`): an alert named "claude" whose pid had already
become an unrelated `bfs` scan. Process-targeted alerts (`high_load`/`swap_pressure`/
`low_memory`/`cpu_hog`/`battery_low`) capture the process's full command line
synchronously, at the moment they fire, and hand it to the agent alongside the alert
so a stale-by-the-time-you-check pid doesn't get misattributed.

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
cargo llvm-cov --workspace --ignore-filename-regex 'src/(main|watch|ui_loop|menubar_loop|agent_process|notify)\.rs' \
  --fail-under-lines 99.5 --fail-under-regions 98
cd agent && uv run pytest       # prompt building, the tool-access safety rails, cov-fail-under=99.9
```

The TUI isn't tested by hand — `ratatui::backend::TestBackend` renders frames into a
buffer, and tests assert on the buffer's content without a real terminal. See
`AGENTS.md`'s testing section for why line coverage is measured with that
`--ignore-filename-regex` (six files are genuine OS-boundary glue — a real terminal
event loop, a real macOS menu bar event loop, a real process spawn — with everything
else around them fully unit-tested) and
[docs/decisions/0003-coverage-gate-glue-isolation.md](docs/decisions/0003-coverage-gate-glue-isolation.md)
for the design rationale.

## Profiling

For when vigil itself shows up as the top consumer in its own alerts (see the
`self_process_note` behavior above) or otherwise feels slower than it should:

```bash
brew install samply                          # no sudo/SIP dance, unlike cargo-flamegraph+dtrace on macOS
cargo build --profile profiling              # release-level optimization, real symbols (see Cargo.toml)
samply record -- ./target/profiling/vigil snapshot
# or, for the steady-state loop:
samply record -- ./target/profiling/vigil watch --count 20 --interval 1 --out /tmp/profile.jsonl
```

`samply record` opens the recorded profile in the Firefox Profiler UI in your browser
(add `--save-only -o profile.json.gz` to just write the file instead). No `criterion`
micro-benchmark suite — vigil's hot paths are I/O-bound shell-outs/`sysinfo` calls, not
CPU-bound pure functions a micro-benchmark would usefully isolate, and there's no CI to
run regression benchmarks against yet (see the Git workflow section in `AGENTS.md`);
ad-hoc profiling when something actually feels slow is proportionate to this project's
size. `[profile.profiling]` in `Cargo.toml` exists solely for this — it never affects
the real `--release` build anything ships as.

## Architecture

```
vigil (Rust)                            agent/ (Python, Claude Agent SDK)
├── cli.rs — clap arg definitions       ├── prompts.py    — pure prompt building (tested without network)
├── snapshot.rs — snapshot collection,  ├── diagnose.py   — query() to Claude: Bash/Read/Grep/Glob allowed,
│   incl. connections via `netstat`     │                    Write/Edit + destructive Bash patterns denylisted
│   (see docs/decisions/0001)           └── cli.py        — vigil-agent ask --snapshot F --question Q
├── alerts.rs — threshold rules
│   (no LLM, no network)
├── battery.rs — drain-rate ETA
│   (no powermetrics/sudo)
├── incidents.rs — markdown journal
│   for auto-diagnoses only
│   (~/.vigil/incidents/)
├── incidents_cmd.rs — `vigil incidents`
├── agent.rs — question/arg building,
│   output parsing (pure, unit-tested)
├── menubar.rs — health classification,
│   icon rendering (pure, unit-tested)
├── watch.rs — the `vigil watch` loop
│   (OS-boundary glue, see ADR-0003)
├── ui_loop.rs — the `vigil ui` terminal
│   event loop (OS-boundary glue)
├── menubar_loop.rs — the real macOS
│   tray/menu event loop (OS-boundary)
├── agent_process.rs — the actual
│   `uv run vigil-agent` spawn (OS-boundary)
├── notify.rs — the actual `osascript`
│   shell-out (OS-boundary)
└── main.rs — Cli::parse() + dispatch
    to the module owning each subcommand
```

Project-wide design decisions with a real alternative (a parsing strategy, an alert
heuristic, a new subsystem) are recorded under
[`docs/decisions/`](docs/decisions/) as short ADRs. See `AGENTS.md` for the full
project rule set (language, testing conventions, the live incident-monitoring loop).
