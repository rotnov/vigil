---
id: 0006
title: "Opt-in investigate/propose/approve/execute fix workflow"
status: accepted
---

## 0006: Opt-in investigate/propose/approve/execute fix workflow

- Status: accepted
- Context: vigil was investigate-only by design since its first version — every
  alert-worthy incident auto-triggered a real Claude Agent SDK diagnosis
  (`agent::is_auto_diagnose_worthy`/`agent_process::maybe_diagnose_alert_async`), and
  every suggestion required the user to act themselves; `DISALLOWED_TOOLS` blocked
  `kill*`/`rm*`/`sudo*`/etc. unconditionally, in both the interactive and
  auto-triggered paths. Living with that in the field surfaced two real costs: (1)
  auto-diagnosis spent real tokens ($0.50-$2.50 per diagnosis observed) on every
  alert-worthy firing whether or not the user wanted to read it — on a chronically
  loaded machine, one ~8h overnight window produced 30+ `cpu_hog`/`high_load`
  incidents, mostly the same root cause (a PyCharm MCP notification storm)
  repeating; (2) the diagnosis dead-ended at "here's what to run yourself" even for
  low-risk, well-scoped fixes (kill a confirmed-stale process, delete an orphaned
  cache, flip one `defaults`/`launchctl` setting) the agent had already fully
  identified.
- Decision:
  1. **Investigation becomes opt-in.** An alert firing notifies with the exact
     command to investigate it, `vigil investigate <alert-key>`, but does not
     itself spawn an agent — in `vigil watch` and in `vigil ui`'s own background
     alert loop alike; the interactive `a`/`w`-key ask is unaffected. Only alert
     keys `agent::is_journal_worthy` returns true for (`high_load`, `cpu_hog:*`,
     `battery_low`, `high_process_count:*`) also get a stub incident file written
     (`incidents::write_stub` — title, alert key, rule message, nothing else);
     other keys get only the plain notification, same as before this whole
     feature. This gating was added during implementation, discovered via a Task 9
     review bug: writing a stub for *every* alert firing would cause unbounded
     incident-journal growth for targetless alerts — `low_disk:<mount>`,
     `high_connection_count`, `incoming_connections` — which `IncidentTracker`
     always treats as "new" since they have no dedup key. The fix reuses the
     filter the old `is_auto_diagnose_worthy` applied to diagnosis, reintroduced
     under a new name and purpose, `agent::is_journal_worthy`, to gate journaling
     instead. (`swap_pressure`/`low_memory` dedupe fine via `IncidentTracker` just
     like `high_load` does but are simply outside the journal-worthy set by
     design, left to the interactive `a` flow.)
  2. **A gated, human-approved fix-execution path replaces the unconditional
     block.** `vigil investigate` may append a `## Proposed fix` JSON plan
     (`{"plan": [{"category", "description", "target_hint"}, ...]}`) when the
     agent is confident about a specific, narrow, low-risk fix — most diagnoses
     still won't have one. `vigil fix <incident-file>` prompts for per-step
     approval in the terminal, then hands *only* the approved steps to a
     separate, dedicated execute-agent session
     (`agent/src/vigil_agent/execute.py`) whose tool config is built fresh per
     invocation from exactly the fix categories present in what was approved:
     `kill_process` (`kill`/`killall`/`pkill`), `delete_path` (`rm`/`rmdir`/`mv`),
     `system_setting` (`defaults write/delete`,
     `launchctl unload/bootout/remove`). A non-liftable hard floor
     (`sudo`/`su`/`dd`/`diskutil erase`-family/`shutdown`/`reboot`/`halt`/`chmod`/
     `chown`, plus `Write`/`Edit`/`NotebookEdit`) stays blocked regardless of
     what's approved — no plan can ever unlock it.
  3. **An agent executes the plan, not a frozen Rust-issued shell command.** The
     execute-agent must re-verify each step's `target_hint` against current
     system state before acting and abort all remaining steps the moment one
     fails or its target has diverged. This directly answers a race this project
     already hit in the field once: a pid captured at alert-fire time can already
     refer to an unrelated process by the time anything acts on it (see
     `2026-08-07-14-20-56-cpu-hog-27339.md`, fixed for diagnosis by capturing
     `Alert::command` synchronously). A frozen shell command has no way to notice
     a stale target; an agent re-verifying does.
  4. **Executing a fix never marks an incident resolved by itself.**
     `alerts::IncidentTracker`'s open/closed state stays driven purely by whether
     the alert keeps re-firing — "the agent reports it took an action" and "the
     condition actually cleared" are different claims.
- Alternatives considered: **a structured plan executed literally by Rust
  (`Command::new` per approved step), no second agent session** — initially the
  frontrunner for being safer-by-construction (no LLM in the execution loop at
  all), but rejected once weighed against this project's own field data: the exact
  PID-reuse race a frozen command can't detect is not hypothetical here, it
  already happened once. An agent that re-verifies before acting closes that gap;
  a literal command interpreter can't. **Keeping auto-diagnosis but adding a fix
  step on top** — rejected because it doesn't address the token-spend cost that
  was half the motivation; opt-in investigation is what actually stops paying for
  diagnoses nobody reads.
- Consequences: `agent::is_auto_diagnose_worthy` and
  `agent_process::maybe_diagnose_alert_async` are gone, along with
  `alerts::RecentAlerts` (its only consumer was the now-removed auto-diagnosis
  question-building). Two new CLI subcommands, `vigil investigate` and `vigil
  fix`, and a new Python module, `agent/src/vigil_agent/execute.py`, alongside
  `diagnose.py` rather than merged into it — the two configs' allowed blast
  radius is fundamentally different, and conflating them risks a future edit to
  one accidentally loosening the other. Two new files join the coverage gate's
  `--ignore-filename-regex` list (`investigate_process.rs`, `fix_process.rs`) for
  the same reason `agent_process.rs` already does — real process spawns a unit
  test shouldn't trigger. Full design at
  [docs/superpowers/specs/2026-08-09-fix-execution-workflow-design.md](../superpowers/specs/2026-08-09-fix-execution-workflow-design.md).
