# Repository Instructions

vigil is a lightweight macOS resource monitor: a Rust metrics collector/TUI plus a
separate Python layer (Claude Agent SDK) that diagnoses what the Rust side observes.
Scale process to the project's own size — this is a small, single-maintainer tool, not
a governance-heavy one. Keep instructions here proportionate to that; don't import
heavier process from other projects wholesale just because it exists elsewhere.

## Governing design goal

Everything vigil does should serve one goal: keep the machine's own performance at its
max while on AC power, and conserve battery while on battery power. This governs
judgment calls, not just user-facing features — vigil's *own* overhead counts against
this goal too. `agent::DiagnosisCoalescer` exists because three redundant background
`uv run` + Claude Agent SDK investigations for one correlated event was real CPU/token
cost working against exactly this goal. When evaluating a proposed change, ask whether
it moves the machine toward or away from this, including the cost vigil itself adds.

## Core architectural rule: Rust never decides or fixes anything

- `src/*.rs` only collects metrics (no network calls, no LLM) and, in `alerts.rs`,
  evaluates fixed cheap thresholds — it never reasons about *why* something is wrong.
- Diagnosis and recommendations live only in `agent/` (Python, Claude Agent SDK),
  reached via `src/agent.rs` shelling out to `uv run vigil-agent ask`.
- The agent may *inspect* the live system (Bash/Read/Grep/Glob — logs, `sample`,
  `vm_stat`, `du`, `pmset -g therm`, ...) but is blocked at the tool level from
  modifying it, identically whether it's the interactive `a`-key ask in `vigil ui` or
  an auto-triggered background diagnosis — same contract, same
  `agent/src/vigil_agent/diagnose.py` config either way:
  - `ALLOWED_TOOLS`: `Bash`, `Read`, `Grep`, `Glob` only.
  - `DISALLOWED_TOOLS` always includes `Write`, `Edit`, `NotebookEdit`, plus a Bash
    denylist covering destructive/privilege-escalating patterns: `sudo *`, `su *`,
    `rm *`, `rmdir *`, `mv *`, `dd *`, `kill *`, `killall *`, `pkill *`, `diskutil
    erase*`/`partition*`/`eraseVolume*`, `launchctl unload*`/`bootout*`/`remove*`,
    `chmod *`, `chown *`, `shutdown *`, `reboot *`, `halt *`, `defaults
    write*`/`delete*`.
  - It only ever produces text — nothing on the machine changes without the user
    doing it themselves. Any risky suggestion is framed as needing explicit user
    confirmation, never phrased or executed as already-done.
  - Never relax this list to let the agent execute a fix directly, in either flow.
- Battery: percentage-trend ETA only (`src/battery.rs`), no `powermetrics`/sudo — a
  deliberate, explicit choice, not an oversight. If accurate per-process power
  attribution is ever wanted, that's a new decision (record it under
  `docs/decisions/`), not a default to slide into.

## Language

- Converse with the user in whatever language they write in.
- Everything written into the repository is English regardless: code, identifiers,
  comments, UI strings, alert/notification text, commit messages, docs, tests, ADRs.
  This was an explicit early project rule after the TUI briefly shipped Russian text —
  don't let it regress.

## Python tooling

- Always `uv` — `uv sync`, `uv run vigil-agent ...`, `uv run --with <pkg> python3
  script.py` for throwaway scripts. Never a manual `python3 -m venv` + `pip install`,
  including for scratch/investigation scripts outside `agent/`.
- `vigil-agent` reuses the already-authenticated `claude` CLI session — no separate
  `ANTHROPIC_API_KEY` is needed or expected. Requires an installed, logged-in
  [Claude Code](https://claude.com/claude-code) on the machine running it.

## Testing

- `cargo test` (or `cargo test --release` for parity with what actually ships) —
  includes TUI rendering via `ratatui::backend::TestBackend` (renders into an
  in-memory `Buffer`; assert on `buf.content`), no real terminal needed. The TUI is
  never tested by hand for this reason.
- `cd agent && uv run pytest` — prompt building (`prompts.py`, pure/no network) and the
  tool-access safety rails (`test_diagnose_config.py` asserts Write/Edit are never
  allowed and the destructive Bash patterns stay denylisted). Run this whenever
  `agent/` changes.
- New Rust logic gets a pure, unit-testable function kept separate from any
  `Command`/IO call — the same split `parse_battery_line`/`parse_netstat_output`/
  `build_args` already have from `read_battery`/`collect_connections`/`ask`'s actual
  process spawn. Follow it for new OS-shelling code instead of testing through the
  side effect.
- New alert rules follow `alerts.rs`'s existing shape: a pure `evaluate()`-style
  function taking a `&Snapshot` plus `&mut AlertState` for cooldown/streak tracking, a
  threshold constant near the top of the file, and tests built on the file's
  `healthy_snapshot()` fixture — not a new ad hoc pattern per rule.

## Decisions (ADRs)

- Record an irreversible or project-wide design choice — a parsing strategy with a
  real alternative, a new alert heuristic, a new subsystem (e.g. a menu-bar UI) — as a
  new file under `docs/decisions/NNNN-slug.md`. Short frontmatter (`id`, `title`,
  `status`) plus Context/Decision/Alternatives/Consequences sections; see any existing
  file there for the exact shape.
- Don't renumber or silently rewrite an accepted decision — a changed mind is a new
  decision file that supersedes the old one (say so in its Context).
- No index-generation tooling for this, unlike heavier projects that do — the
  directory listing is the index. Keep the directory small enough that this stays
  true; if it ever doesn't, that's itself a decision to make deliberately, not to back
  into.
- Smaller calls (a threshold constant, a cooldown value, a struct field) don't need
  their own ADR — a clear commit message explaining the "why" is enough, consistent
  with how the live incident-driven improvement cycle has worked so far (e.g.
  `alerts::RecentAlerts`, `agent::DiagnosisCoalescer` shipped on commit-message
  rationale alone, no ADR).

## The live incident-monitoring loop

- `vigil watch` runs continuously in the background; `alerts.rs` auto-triggers a
  background agent diagnosis for `high_load`/`cpu_hog:*`/`battery_low` (disk and plain
  memory-pressure alerts don't — see `agent::is_auto_diagnose_worthy`). Every
  auto-triggered diagnosis is journaled to
  `~/.vigil/incidents/<date>-<time>-<slug>.md` — a fixed, home-relative path (vigil is
  meant to run from anywhere, not just its own repo). The interactive `a`-key ask in
  `vigil ui` is deliberately NOT journaled — on-screen only, by design.
- `vigil incidents` reads that journal from a plain shell (list recent, or `--show
  <name>` for one in full) — this exists specifically because a push notification
  can't spontaneously open an already-running `vigil ui` session.
- When real incidents land, read them, look for a genuine pattern across more than one
  (not a single anecdote), and treat a confirmed pattern as a legitimate case for a
  targeted vigil improvement — this is the actual mechanism this project uses to find
  bugs and gaps in itself, not a hypothetical. Verify a proposed fix against the
  *actual* field data before shipping it, not just against the pattern that motivated
  it: a naive time-only cooldown for redundant diagnoses was rejected in favor of
  `DiagnosisCoalescer`'s coalesce-by-target design specifically because the live
  incident batch that motivated it also contained a genuinely independent finding a
  time-only cooldown would have silently dropped.
- Never let this loop, or any alert/diagnosis path, auto-execute anything the incident
  data suggests — it stays investigate → journal → notify → (human decides). This is
  the same rule as the tool-access one above, restated because it's the thing this
  whole feature exists to not violate.

## Personal / machine-local config

- `.claude/settings.local.json` is gitignored personal tooling config (e.g.
  `autoCompactWindow`), not a project rule — don't commit it, don't treat its contents
  as something other contributors need.

## Completion check

Before finishing a change:

1. `cargo build --release` and `cargo test --release` (or the plain non-release forms
   during iteration).
2. `cd agent && uv run pytest` if `agent/` changed.
3. Update `README.md` (features/usage/architecture) in the same change when behavior,
   CLI flags, or the module layout changed — it's the primary user-facing doc this
   project maintains outside `docs/decisions/`.
4. Add a `docs/decisions/` entry if the change was an irreversible/project-wide choice
   with a real alternative (see "Decisions (ADRs)" above); otherwise a clear commit
   message carries the rationale.
