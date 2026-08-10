# vigil

A lightweight macOS resource monitor: a Rust metrics collector (CPU/memory/swap/disks/battery)
with a terminal dashboard, plus a separate layer on the
[Claude Agent SDK](https://github.com/anthropics/claude-agent-sdk-python) that investigates a
snapshot (and, if needed, the live system) and answers questions like "why is it slow" or
"why is disk space low".

The split is deliberate: **Rust never decides or fixes anything** — it only cheaply
collects metrics (no network, no LLM) and draws them. Diagnosis, fix proposals, and
fix execution are all a separate Python process. Investigation (`vigil investigate`)
is read-only, same as always — the agent can *inspect* the live system (logs,
`sample`, `vm_stat`, `du`, ...) but is blocked at the tool level from modifying it. If
it identifies a specific, low-risk fix, it may propose one; nothing runs until you
explicitly approve it step by step with `vigil fix`, which hands only what you
approved to a separate, narrowly-scoped agent session — see "Investigate, propose,
approve, execute" below.

## Features

- `vigil snapshot` — a single JSON snapshot to stdout (for scripts/the agent)
- `vigil watch` — continuously appends snapshots to JSONL + fires native macOS
  notifications on detected anomalies (high load average, active swap, a process
  holding CPU for several consecutive samples, low disk space, low battery, an
  unusually high TCP connection count, unusual incoming connections, or an
  unusually large number of near-idle instances of the same process — see below).
  Swap/memory alerts name whichever is bigger — the single top process, or a process
  *group* (every same-named instance combined, e.g. a dozen renderer helpers that
  individually never rank at the top) when the group is at least 1.5x larger. When a
  CPU-related alert's own top consumer turns out to be `vigil` itself, the message
  says so explicitly instead of reading like any other app to restart — vigil's own
  overhead counts against the same performance goal it exists to protect, so this is
  never hidden or excluded, just made legible when it happens
- **Leaked process detection**: a live incident found 224 `node` processes sitting at
  0% combined CPU — MCP server subprocesses spawned by closed Claude Code/Codex/Devin
  sessions, never cleaned up, some over a week old. `high_process_count` fires when a
  process group is both unusually large *and* collectively near-idle (a real busy
  multi-process workload, like a browser with many active tabs, won't match both at
  once) and names concrete `pid (ppid X, age)` samples of the group's oldest members
  in the message — not just a count — so there's something to actually check before
  deciding whether to kill anything. Investigate with `vigil investigate
  high_process_count:<name>`, same as `cpu_hog`. See
  [docs/decisions/0004-leaked-process-detection.md](docs/decisions/0004-leaked-process-detection.md)
- `vigil ui` — a live terminal dashboard (CPU/MEM sparklines, top processes with a
  ↑/↓/→ trend arrow on memory over the last 10 samples, battery % with a drain-rate
  ETA when discharging). `a` key — ask the agent a free-form question. `w` key —
  ask, pre-filled, why the current #1 CPU process is using what it's using
- `vigil incidents` — list or show saved investigations from the terminal, without
  needing an already-open `ui` session (a TUI can't pop itself open on a push
  notification): `vigil incidents` lists recent ones, `vigil incidents --show <name>`
  prints one in full (accepts a filename or any substring that matches exactly one)
- `vigil menubar` — a macOS menu bar status item: an eye icon (drawn procedurally,
  not a bundled asset), transparent/faint when nothing's open, yellow for one open
  incident, red for multiple. Click for a dropdown of recent incidents (opens the
  markdown file). Polls the status file `vigil watch` writes each tick rather than
  sampling on its own — see
  [docs/decisions/0002-menu-bar-health-indicator.md](docs/decisions/0002-menu-bar-health-indicator.md)

Every agent answer — interactive `a`, `vigil investigate`, or `vigil fix` — ends with
a token/cost footer (`_Tokens: N in / M out (+K cache read) — ~$X_`), since the
agent's own token spend is part of the overhead this project tries to keep visible,
not hide.

### Investigate, propose, approve, execute

Nothing runs automatically when an alert fires. For `high_load`, `cpu_hog:*`,
`battery_low`, and `high_process_count:*`, vigil writes a stub incident file
(title, alert key, rule message, and — for process-specific alerts that captured
one — a `**Command:**` line with the process's command line at fire time) to
`~/.vigil/incidents/<date>-<time>-<slug>.md` (override with `--incidents-dir`) and
notifies with the command to investigate it. Other alert keys (low disk, connection
counts, swap/memory pressure) just fire a plain notification, same as before this
feature — no stub file, no investigate hint, nothing added to the journal:

```bash
# after a notification like "cpu_hog:37489 — investigate? vigil investigate cpu_hog:37489"
./target/release/vigil investigate cpu_hog:37489
```

This runs the same read-only agent as the interactive `a` flow and appends its
answer to the incident file. If it's confident about a specific, narrow, low-risk
fix, it also appends a `## Proposed fix` JSON plan — most diagnoses won't have one.
When there is one:

```bash
./target/release/vigil fix ~/.vigil/incidents/2026-08-09-01-09-41-cpu-hog-37489.md
```

prompts per step (`[1/2] kill_process — Kill the stale claude session ... Approve?
[y/N]`), then hands *only* the steps you approved to a separate, narrowly-scoped
agent session (`agent/src/vigil_agent/execute.py`) — its tool config is built fresh
each time from exactly the fix categories your approved steps belong to
(`kill_process`, `delete_path`, `system_setting`), with a non-liftable hard floor
(`sudo`/`dd`/`diskutil erase`-family/`shutdown`/`chmod`/`chown`/etc.) that stays
blocked no matter what's approved. The execute-agent re-verifies each step's target
before acting — a pid/path captured when the plan was proposed can be stale by
execution time — and aborts all remaining steps the moment one fails or diverges,
rather than pressing on with stale assumptions. Results append to the same incident
file as a `## Fix execution` section. See
[docs/decisions/0006-opt-in-investigate-propose-approve-execute.md](docs/decisions/0006-opt-in-investigate-propose-approve-execute.md)
for the full design rationale.

Because you may not run `vigil investigate` until well after the alert fired —
seconds later, or much longer — the flagged process's pid can already have been
recycled by the OS to something unrelated by the time you check it: observed live
(see `2026-08-07-14-20-56-cpu-hog-27339.md`), an alert named "claude" whose pid had
already become an unrelated `bfs` scan. Process-targeted alerts (`high_load`/
`cpu_hog`/`battery_low`) capture the flagged process's full command line
synchronously, at the moment they fire, and hand it to the agent when you later run
`vigil investigate` — a guard against exactly that. Still worth double-checking
yourself (`ps -p <pid> -o command`) as a sanity check, especially the longer you wait
to investigate.

Repeat firings are treated as one ongoing incident, not a fresh one each time:
`alerts::IncidentTracker` skips the notification and the incident stub for a target
that already has one open (still firing within twice the alert
cooldown of its last firing), and only starts a new incident once that target has
been quiet for longer than that. For `cpu_hog`/`high_process_count`, the alert is
fundamentally about one specific process/group, so "target" means exactly that — a
different process firing in the same window gets its own full treatment. For
`high_load`/`swap_pressure`/`low_memory`/`battery_low`, which are about an aggregate
system condition, "target" is a fixed sentinel instead of whichever process the
message happens to cite as the top consumer — on a loaded machine that name rotates
every few seconds (contactsd, then codex, then pycharm, ...) while the underlying
condition is the same ongoing thing, and citing the rotating name as the dedup key
used to defeat `IncidentTracker` almost entirely for these rules. `high_load` also
requires the load average to stay above threshold continuously for 30s before firing
at all, not just a fixed sentinel for dedup — `load_avg.one` is already a 1-minute
OS-smoothed average, so this only filters a brief threshold-crossing right at the
boundary, not real sample-to-sample noise. See
[docs/decisions/0005-stable-incident-targets-and-sustained-high-load.md](docs/decisions/0005-stable-incident-targets-and-sustained-high-load.md).

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

Nothing on your machine changes unless you approve it: alerts and `vigil investigate`
only ever produce text, and the one path that can act, `vigil fix`, runs only the
steps you approved, one at a time.

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

# browse past investigations from a plain shell
./target/release/vigil incidents
./target/release/vigil incidents --show cpu-hog-64955

# investigate an alert the notification pointed at, then act on any proposed fix
./target/release/vigil investigate cpu_hog:37489
./target/release/vigil fix ~/.vigil/incidents/2026-08-09-01-09-41-cpu-hog-37489.md

# menu bar health indicator (run alongside `vigil watch`, not instead of it)
./target/release/vigil menubar
```

## Tests

```bash
cargo llvm-cov --workspace --ignore-filename-regex 'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs' \
  --fail-under-lines 99.5 --fail-under-regions 98
cd agent && uv run pytest       # prompt building, the tool-access safety rails, cov-fail-under=99.9
```

The TUI isn't tested by hand — `ratatui::backend::TestBackend` renders frames into a
buffer, and tests assert on the buffer's content without a real terminal. See
`AGENTS.md`'s testing section for why line coverage is measured with that
`--ignore-filename-regex` (eight files are genuine OS-boundary glue — a real terminal
event loop, a real macOS menu bar event loop, three real process spawns — with
everything else around them fully unit-tested) and
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
├── snapshot.rs — snapshot collection,  ├── diagnose.py   — investigation query(): Bash/Read/Grep/Glob allowed,
│   incl. connections via `netstat`     │                    Write/Edit + destructive Bash patterns denylisted
│   (see docs/decisions/0001)           ├── execute.py    — fix-execution query(): per-plan-scoped Bash unlocks
├── alerts.rs — threshold rules         │                    on top of a non-liftable hard floor
│   (no LLM, no network)                └── cli.py        — vigil-agent ask --snapshot F --question Q
├── battery.rs — drain-rate ETA                              vigil-agent execute --plan-json J
│   (no powermetrics/sudo)
├── incidents.rs — markdown journal:
│   write_stub / append_diagnosis /
│   append_fix_execution (~/.vigil/incidents/)
├── incidents_cmd.rs — `vigil incidents`
├── fixplan.rs — proposed-fix JSON plan
│   parsing + approval formatting
├── investigate.rs — resolve an alert
│   key to its incident file
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
├── investigate_process.rs — `vigil
│   investigate`'s spawn (OS-boundary)
├── fix_process.rs — `vigil fix`'s
│   approval prompt + spawn (OS-boundary)
├── notify.rs — the actual `osascript`
│   shell-out (OS-boundary)
└── main.rs — Cli::parse() + dispatch
    to the module owning each subcommand
```

Project-wide design decisions with a real alternative (a parsing strategy, an alert
heuristic, a new subsystem) are recorded under
[`docs/decisions/`](docs/decisions/) as short ADRs. See `AGENTS.md` for the full
project rule set (language, testing conventions, the live incident-monitoring loop).
