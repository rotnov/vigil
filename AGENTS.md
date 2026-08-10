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
this goal too. `alerts::IncidentTracker` exists because repeated notifications and
journal entries for one ongoing condition was real CPU/token/attention cost working
against exactly this goal. When evaluating a proposed change, ask whether it moves
the machine toward or away from this, including the cost vigil itself adds.

## Core architectural rule: Rust never decides or fixes anything

- `src/*.rs` only collects metrics (no network calls, no LLM) and, in `alerts.rs`,
  evaluates fixed cheap thresholds — it never reasons about *why* something is
  wrong, and never decides *whether* a fix is safe. Diagnosis, fix proposals, and
  fix execution all live only in `agent/` (Python, Claude Agent SDK), reached via
  `src/agent_process.rs` shelling out to `uv run vigil-agent ask` / `uv run
  vigil-agent execute`.
- Investigation is opt-in, not automatic: an alert firing notifies with the command
  to run, `vigil investigate <alert-key>`, and — only for alert keys
  `agent::is_journal_worthy` returns true for (`high_load`, `cpu_hog:*`,
  `battery_low`, `high_process_count:*`) — also writes a stub incident file
  (`incidents::write_stub`: title, alert key, rule message, plus a `**Command:**`
  line when the alert was process-specific and captured one — nothing else); other
  keys get only the plain notification, same as before this whole plan
  (`low_disk:<mount>`/`high_connection_count`/`incoming_connections` are
  targetless and would fire unboundedly if journaled; `swap_pressure`/`low_memory`
  dedupe fine via `IncidentTracker` just like `high_load` does but are simply
  outside the journal-worthy set by design, left to the interactive `a` flow). No
  agent process spawns until the user explicitly runs that command.
- `vigil investigate` runs the same read-only investigation agent as the
  interactive `a`-key ask in `vigil ui` — identical contract either way, same
  `agent/src/vigil_agent/diagnose.py` config:
  - `ALLOWED_TOOLS`: `Bash`, `Read`, `Grep`, `Glob` only.
  - `DISALLOWED_TOOLS` always includes `Write`, `Edit`, `NotebookEdit`, plus the
    full destructive/privilege-escalating Bash denylist: `sudo *`, `su *`, `rm *`,
    `rmdir *`, `mv *`, `dd *`, `kill *`, `killall *`, `pkill *`, `diskutil
    erase*`/`partition*`/`eraseVolume*`, `launchctl unload*`/`bootout*`/`remove*`,
    `chmod *`, `chown *`, `shutdown *`, `reboot *`, `halt *`, `defaults
    write*`/`delete*`.
  - It only ever produces text — nothing on the machine changes from this path. If
    it identifies a specific, narrowly-scoped, low-risk fix, it may additionally
    append a `## Proposed fix` JSON block (schema in `prompts.py`'s
    `SYSTEM_PROMPT`) — a proposal, not an action.
- A `## Proposed fix` only ever executes through `vigil fix <incident-file>`, and
  only after per-step interactive approval in the terminal (`fix_process::run`).
  Approved steps — and *only* the approved ones; a rejected step is never even
  mentioned to the execute-agent — are handed to a **separate**, narrowly-scoped
  execute-agent session (`agent/src/vigil_agent/execute.py`), whose tool config is
  built fresh per invocation from exactly the fix categories present in what was
  approved:
  - `kill_process` unlocks `Bash(kill *)`/`Bash(killall *)`/`Bash(pkill *)`.
  - `delete_path` unlocks `Bash(rm *)`/`Bash(rmdir *)`/`Bash(mv *)`.
  - `system_setting` unlocks `Bash(defaults write*)`/`Bash(defaults delete*)`/
    `Bash(launchctl unload*)`/`Bash(launchctl bootout*)`/`Bash(launchctl remove*)`.
  - A **non-liftable hard floor** stays blocked regardless of what's approved —
    outside all three categories, no plan can ever unlock it:
    `execute.HARD_FLOOR_DISALLOWED_TOOLS` (`sudo`/`su`/`dd`/`diskutil
    erase`-family/`shutdown`/`reboot`/`halt`/`chmod`/`chown`, plus
    `Write`/`Edit`/`NotebookEdit` — the execute-agent acts only through unlocked
    Bash patterns, never by writing/editing files directly).
  - The execute-agent must re-verify a step's target before acting (a
    pid/path/setting captured at proposal time may be stale by execution time —
    see `agent::build_diagnosis_question`'s doc comment for a real pid-reuse race
    this project already hit) and must abort all remaining steps the moment one
    fails or its target has diverged, rather than continuing on a plan whose
    assumptions no longer hold.
  - Never widen `HARD_FLOOR_DISALLOWED_TOOLS`, never let a category unlock more
    than its own listed patterns, and never let `vigil fix` run *any* step the user
    didn't explicitly approve.
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
  'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs'
  --fail-under-lines 99.5 --fail-under-regions 98`.** This is a merge invariant, not an aspiration — treat
  a change that drops below it the same as a failing test. As of 2026-08-07 this is
  met (99.5%+ lines, stable across repeated runs — see
  [docs/decisions/0003-coverage-gate-glue-isolation.md](docs/decisions/0003-coverage-gate-glue-isolation.md)
  for the full story of closing the gap from an earlier 73.44%, including why the
  original 99.9% target was revised: the remaining shortfall is `assert!` panic-message
  arguments, a known source-coverage artifact on test code, not untested behavior).
  - The eight `--ignore-filename-regex` files hold *only* irreducible OS-boundary
    glue (a real terminal event loop, a real macOS tray event loop, three real
    process spawns) — each has a doc comment explaining why. Adding new logic to one
    of them is a smell; extract a pure function into the file it's gluing together
    instead, the same way `agent.rs`/`ui.rs`/`menubar.rs`/`alerts.rs` already keep
    their tested logic separate from
    `agent_process.rs`/`ui_loop.rs`/`menubar_loop.rs`/`notify.rs`/`investigate_process.rs`/`fix_process.rs`.
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
  - Because `cargo test` cannot exercise the eight excluded files, a change touching
    any of them needs a manual smoke run before merging: `vigil snapshot | jq .`,
    `vigil watch --count 2 --out /tmp/x.jsonl` (check the JSONL line and that the
    status file got written), `vigil incidents` + `vigil incidents --show <name>`
    (check `echo $?` on both a match and a miss — `Commands::Incidents` goes through
    `std::process::exit`), `vigil menubar` launched briefly and killed, `vigil
    investigate <key>` against a hand-written stub incident file (spends real agent
    tokens), and `vigil fix <file>` against an incident with a `## Proposed fix`
    block, approving then rejecting a step to confirm both paths.

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
  `alerts::IncidentTracker` shipped on commit-message rationale alone, no ADR).

## The live incident-monitoring loop

- `vigil watch` runs continuously in the background; an alert firing notifies with the
  exact command to investigate it — `vigil investigate <alert-key>` — but does not
  itself spawn an agent. Only alert keys `agent::is_journal_worthy` returns true for
  (`high_load`, `cpu_hog:*`, `battery_low`, `high_process_count:*`) also get a stub
  incident file (`incidents::write_stub` — title, alert key, rule message, plus a
  `**Command:**` line when one was captured); other keys fire a plain notification
  only, same as before this whole plan
  (`low_disk:<mount>`/`high_connection_count`/`incoming_connections` are
  targetless and would fire unboundedly if journaled; `swap_pressure`/`low_memory`
  dedupe fine via `IncidentTracker` but are simply outside the journal-worthy set
  by design, left to the interactive `a` flow). Every incident file that does get
  written lives at
  `~/.vigil/incidents/<date>-<time>-<slug>.md` — a fixed, home-relative path (vigil
  is meant to run from anywhere, not just its own repo). The interactive `a`-key ask
  in `vigil ui` is deliberately NOT journaled — on-screen only, by design.
- `vigil incidents` reads that journal from a plain shell (list recent, or `--show
  <name>` for one in full) — this exists specifically because a push notification
  can't spontaneously open an already-running `vigil ui` session.
- When real incidents land, read them, look for a genuine pattern across more than
  one (not a single anecdote), and treat a confirmed pattern as a legitimate case
  for a targeted vigil improvement — this is the actual mechanism this project uses
  to find bugs and gaps in itself, not a hypothetical. Verify a proposed fix
  against the *actual* field data before shipping it, not just against the pattern
  that motivated it: an initial narrow fix (`agent::DiagnosisCoalescer`, a 120s
  near-simultaneous window keyed by target process) was itself later found to
  never actually engage — real repeats arrived 5-13 minutes apart, gated by each
  rule's own re-fire cooldown, not by anything a 120s window could catch. It was
  replaced by `alerts::IncidentTracker`, a longer open/close window (2x cooldown)
  keyed the same way (by target process, not by time alone — the earlier design's
  own reasoning for that part held up: the same live incident batch that motivated
  coalescing also contained a genuinely independent finding a *time-only* cooldown
  would have silently dropped) and covering notification + the incident stub
  together — diagnosis timing itself is opt-in now and outside `IncidentTracker`'s
  concern entirely. Re-verify a fix like this against fresh field data after
  shipping it, not just once at design time — this project's own history is the
  example.
- Running a fix through `vigil fix` never marks an incident resolved by itself —
  `IncidentTracker`'s open/closed state stays driven purely by whether the alert
  keeps re-firing, unrelated to whether a fix was proposed or run. "The agent
  reports it took an action" and "the underlying condition actually cleared" are
  different claims; conflating them would let a failed or partially-effective fix
  read as resolved when the next snapshot might show the same alert firing again
  minutes later.
- Nothing on the machine changes except through the explicit `vigil investigate` →
  `vigil fix` path described in the "Core architectural rule" section above, and
  never without the human approving the specific plan first. This loop stays
  investigate (opt-in) → propose (opt-in, agent's judgment) → approve (human,
  per-step) → execute (narrowly-scoped agent) → journal — never a shortcut around
  any of those steps.

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
