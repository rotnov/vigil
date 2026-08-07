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
this goal too. `alerts::IncidentTracker` exists because repeated notifications,
diagnoses, and journal entries for one ongoing condition was real CPU/token/attention
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
- `cd agent && uv run pytest` — prompt building (`prompts.py`, pure/no network), the
  tool-access safety rails (`test_diagnose_config.py` asserts Write/Edit are never
  allowed and the destructive Bash patterns stay denylisted), and a `pytest-cov` gate
  (`--cov-fail-under=99.9`, configured in `agent/pyproject.toml`). Run this whenever
  `agent/` changes.
- New Rust logic gets a pure, unit-testable function kept separate from any
  `Command`/IO call — the same split `parse_battery_line`/`parse_netstat_output`/
  `build_args` already have from `read_battery`/`collect_connections`/`ask`'s actual
  process spawn. Follow it for new OS-shelling code instead of testing through the
  side effect. Where the OS-shelling function itself is cheap and side-effect-free to
  call for real (`take_snapshot`, `collect_disks`, `read_battery`,
  `collect_connections`), call it directly in a test rather than mocking `Command` —
  this suite only runs on the maintainer's own Mac (see below), so the real command is
  available and a mock would just be a second, driftable description of output the
  pure parser already covers on its own.
- New alert rules follow `alerts.rs`'s existing shape: a pure `evaluate()`-style
  function taking a `&Snapshot` plus `&mut AlertState` for cooldown/streak tracking, a
  threshold constant near the top of the file, and tests built on the file's
  `healthy_snapshot()` fixture — not a new ad hoc pattern per rule.
- **Hard rule: `cargo llvm-cov --workspace --ignore-filename-regex
  'src/(main|watch|ui_loop|menubar_loop|agent_process|notify)\.rs' --fail-under-lines
  99.5 --fail-under-regions 98`.** This is a merge invariant, not an aspiration — treat
  a change that drops below it the same as a failing test. As of 2026-08-07 this is
  met (99.5%+ lines, stable across repeated runs — see
  [docs/decisions/0003-coverage-gate-glue-isolation.md](docs/decisions/0003-coverage-gate-glue-isolation.md)
  for the full story of closing the gap from an earlier 73.44%, including why the
  original 99.9% target was revised: the remaining shortfall is `assert!` panic-message
  arguments, a known source-coverage artifact on test code, not untested behavior).
  - The six `--ignore-filename-regex` files hold *only* irreducible OS-boundary glue
    (a real terminal event loop, a real macOS tray event loop, a real process spawn) —
    each has a doc comment explaining why. Adding new logic to one of them is a smell;
    extract a pure function into the file it's gluing together instead, the same way
    `agent.rs`/`ui.rs`/`menubar.rs`/`alerts.rs` already keep their tested logic
    separate from `agent_process.rs`/`ui_loop.rs`/`menubar_loop.rs`/`notify.rs`.
  - `#[coverage(off)]` is NOT an option here — confirmed experimental/nightly-only on
    this project's stable toolchain (rustc 1.88.0). Don't rediscover that; use the
    file-isolation pattern above, or `coverage.py`'s `exclude_also`/`omit` on the
    Python side (which *is* stable, see `agent/pyproject.toml`).
  - The only permitted way to fall short of the gate going forward is a documented,
    deliberate exemption: an inline comment on the excluded region (for something that
    can't reasonably move to its own file — e.g. a defensive `$HOME`-unset fallback)
    explaining why it can't be exercised in a unit test, or a new file added to the
    `--ignore-filename-regex` list with the same doc-comment convention. An
    undocumented gap is a defect, not a shrug. Before assuming something can't be
    tested, check whether it's actually *unreachable* (like `collect_disks`'s old
    `total > 0 else 0.0` branch, dead once the upstream `.filter()` is accounted for)
    — simplify those away instead of exempting them.
  - Because `cargo test` cannot exercise the six excluded files, a change touching any
    of them needs a manual smoke run before merging: `vigil snapshot | jq .`, `vigil
    watch --count 2 --out /tmp/x.jsonl` (check the JSONL line and that the status file
    got written), `vigil incidents` + `vigil incidents --show <name>` (check `echo $?`
    on both a match and a miss — `Commands::Incidents` goes through
    `std::process::exit`), and `vigil menubar` launched briefly and killed.

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
  `alerts::RecentAlerts`, `alerts::IncidentTracker` shipped on commit-message
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
  it: an initial narrow fix (`agent::DiagnosisCoalescer`, a 120s near-simultaneous
  window keyed by target process) was itself later found to never actually engage —
  real repeats arrived 5-13 minutes apart, gated by each rule's own re-fire cooldown,
  not by anything a 120s window could catch. It was replaced by
  `alerts::IncidentTracker`, a longer open/close window (2x cooldown) keyed the same
  way (by target process, not by time alone — the earlier design's own reasoning for
  that part held up: the same live incident batch that motivated coalescing also
  contained a genuinely independent finding a *time-only* cooldown would have silently
  dropped) and covering notification + diagnosis + journal together, not diagnosis
  alone. Re-verify a fix like this against fresh field data after shipping it, not just
  once at design time — this project's own history is the example.
- Never let this loop, or any alert/diagnosis path, auto-execute anything the incident
  data suggests — it stays investigate → journal → notify → (human decides). This is
  the same rule as the tool-access one above, restated because it's the thing this
  whole feature exists to not violate.

## Git workflow

- Every change lands via a PR, not a direct push to `master` — create a branch, commit,
  push, `gh pr create`. This applies even though there's currently one maintainer and
  no required-review gate; it's for the audit trail (one PR per change, a
  `gh pr list`/`gh pr view` history of what shipped and why), not for blocking on
  review.
- Auto-merge (`gh pr merge --squash`) once `cargo test --release` (and `uv run pytest`
  if `agent/` changed) are green locally — no CI is configured yet, so "green" means
  the local run right before merging. Don't leave a PR open waiting on a review that
  isn't coming; that would just stall the live incident-monitoring loop for no benefit.
- If a change needs `vigil watch`/`vigil menubar` restarted to take effect (most do),
  do that after the merge, from the merged `master`, the same as before this workflow
  existed — the PR step doesn't change when/how the background processes get restarted.

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
