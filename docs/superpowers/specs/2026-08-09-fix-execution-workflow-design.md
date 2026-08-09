# Fix-execution workflow: opt-in investigate, propose, approve, execute

Status: approved design, pending implementation plan (Task #34/#35 tracker in this
session; a formal ADR under `docs/decisions/` should follow once implemented, since
this amends the "Rust never decides or fixes anything" rule in AGENTS.md).

## Why

Today `vigil` is investigate-only by design (see AGENTS.md's "Core architectural
rule: Rust never decides or fixes anything"): an alert fires, the background agent
diagnoses automatically, and every suggestion — restart PyCharm, close a stale
session, kill a leaked process — requires the user to act themselves. That rule has
held for good reason (`DISALLOWED_TOOLS` blocks `kill*`/`rm*`/`sudo*`/etc.
unconditionally, in both the interactive and auto-triggered paths, so nothing on the
machine changes without the user doing it), but living with it in the field surfaced
two costs:

1. **Investigation itself is not free.** Every alert firing spends agent tokens/cost
   (observed: $0.5–$2.5 per diagnosis) whether or not the user ends up wanting to
   read it — `is_auto_diagnose_worthy` already exists to filter *some* noise, but a
   chronically-loaded machine (this session's own field data: 30+ `cpu_hog`/`high_load`
   incidents in one ~8h overnight window, mostly the same root cause repeating) still
   burns real spend on repeats the user will skim past.
2. **The diagnosis dead-ends at "here's what to run yourself."** For low-risk,
   well-scoped fixes (kill a confirmed-stale process, delete an orphaned cache dir,
   flip a `defaults`/`launchctl` setting) the user has to context-switch out of
   whatever they're doing to copy-paste a command the agent already identified.

This design keeps the "Rust never decides or fixes anything" spirit — Rust still
never reasons about *why*, and no fix ever executes without an explicit, specific,
human-approved plan — but replaces the unconditional block with a gated,
human-approved execution path, and makes investigation itself opt-in rather than
automatic.

## Non-goals

- No change to the read-only interactive `a`-key ask flow in `vigil ui` — it keeps
  its current `ALLOWED_TOOLS`/`DISALLOWED_TOOLS` contract unchanged.
- No UI redesign (Tauri or otherwise) and no MCP server — both explicitly deferred to
  their own separate brainstorms per this session's decomposition.
- No change to alert *rules* themselves (thresholds, sustained-duration filtering) —
  orthogonal to this design, tracked separately (Task #35, and a follow-up to extend
  PR #14's sustained-duration filtering from `high_load` to `cpu_hog`, motivated by
  this session's own field data showing `cpu_hog:37489` firing 8+ times in one
  morning for the same self-resolving PyCharm burst).

## Flow overview

Today: alert fires → Rust synchronously shells out to the agent for a full diagnosis
→ `incidents::record` writes the complete file (title, rule message, diagnosis) in
one shot → native notification.

New:

1. **Alert fires.** Rust writes a stub incident file immediately — title, alert key,
   rule message only, no diagnosis section — and sends a notification naming the
   investigate command: *"new incident: `cpu_hog:37489` — investigate?
   `vigil investigate cpu_hog:37489`"*. No agent call happens yet.
2. **`vigil investigate <alert-key>`** — user-triggered. Runs the existing read-only
   agent (`ALLOWED_TOOLS`/`DISALLOWED_TOOLS` unchanged from today) against the
   already-stubbed incident file, appending its `## Diagnosis` and `## Suggestions`
   sections exactly as it does now. If, and only if, the agent identifies a specific,
   safe, well-scoped fix, it additionally appends a `## Proposed fix` section
   containing a structured JSON plan (schema below). Most diagnoses will have no such
   section — chronic memory pressure with five contributing processes is not a
   "propose a fix" situation, and the agent should not be pushed toward inventing one.
3. **`vigil fix <incident-file>`** — only meaningful (and only available) if the file
   has a `## Proposed fix` block. Prints the plan, asks for step-by-step or
   all-at-once confirmation in the terminal. On approval, Rust constructs a
   per-plan-scoped tool config (below) and launches a dedicated execute-agent session
   whose *only* instruction is the approved plan steps — not the investigation
   transcript, not the incident file, just the JSON.
4. **Execution results** are appended to the same incident file as a
   `## Fix execution` section.

Investigation and fix are two separate, explicit CLI invocations — nothing runs
automatically past step 1.

## Plan format and permission scoping

### Schema

`## Proposed fix` contains a fenced JSON block:

```json
{
  "plan": [
    {
      "category": "kill_process",
      "description": "Kill the stale claude session (was pid 72837 at proposal time — re-verify before killing)",
      "target_hint": "claude --worktree nervous-cori-c94163 --resume skill-adopt"
    },
    {
      "category": "delete_path",
      "description": "Remove the orphaned node_modules cache left behind by that session",
      "target_hint": "/Users/denis/projects/some-worktree/node_modules/.cache"
    }
  ]
}
```

- `category` — one of the three fix categories below. Required; the execute-agent's
  tool unlocks are derived from the set of categories present in the *approved*
  subset of steps, nothing else.
- `description` — human-readable, is what's shown to the user for approval and is
  the literal instruction text handed to the execute-agent for that step. Written by
  the investigating agent, not free-form Rust-generated text.
- `target_hint` — best-effort identifying detail (command line, path, setting key)
  captured at proposal time. Explicitly a *hint*, not a frozen target: the
  execute-agent must re-verify the current state (e.g. re-run `ps` to confirm the
  hinted command line still matches before killing a pid) before acting, the same
  discipline already required of an interactive investigation. This directly answers
  the PID-reuse race this project already hit once in the field (Task #27,
  incident `2026-08-07-14-20-56-cpu-hog-27339.md`) — a frozen shell command executed
  by Rust has no way to notice the target changed between proposal and execution; an
  agent re-verifying does.

### Fix categories and their tool unlocks

Three categories, chosen to cover what this session's field diagnoses actually
recommended over ~40 incidents (kill a stale process, clear an orphaned
cache/snapshot/temp file, flip a `defaults`/`launchctl` setting) without opening the
door wider than that:

| category | unlocks (removed from `DISALLOWED_TOOLS`) |
|---|---|
| `kill_process` | `Bash(kill *)`, `Bash(killall *)`, `Bash(pkill *)` |
| `delete_path` | `Bash(rm *)`, `Bash(rmdir *)`, `Bash(mv *)` |
| `system_setting` | `Bash(defaults write*)`, `Bash(defaults delete*)`, `Bash(launchctl unload*)`, `Bash(launchctl bootout*)`, `Bash(launchctl remove*)` |

### Non-liftable hard floor

Regardless of what's approved, these remain in `DISALLOWED_TOOLS` unconditionally —
they fall outside all three categories and there is no path to unlocking them via a
plan: `Bash(sudo *)`, `Bash(su *)`, `Bash(dd *)`, `Bash(diskutil erase*)`,
`Bash(diskutil partition*)`, `Bash(diskutil eraseVolume*)`, `Bash(shutdown *)`,
`Bash(reboot *)`, `Bash(halt *)`, `Bash(chmod *)`, `Bash(chown *)`. `Write`/`Edit`/
`NotebookEdit` also stay disallowed — the execute-agent acts only through the
unlocked Bash patterns above, never by writing/editing files directly.

### Per-plan scoping

The execute-agent's tool config is built fresh per invocation from exactly the
categories present in the user-approved subset of steps — a `kill_process`-only plan
still has `rm`/`defaults write`/etc. blocked. This mirrors `diagnose.py`'s existing
pattern (a full allow-list plus a denylist carved out of it) rather than inventing a
new mechanism; the new execute-agent config lives alongside `diagnose.py` in
`agent/src/vigil_agent/`, not merged into it, since the two configs' allowed
blast radius is fundamentally different and conflating them risks a future edit to
one accidentally loosening the other.

## CLI and trigger surface

New subcommands (`src/cli.rs`, alongside `Snapshot`/`Watch`/`Ui`/`Incidents`/
`Menubar`):

- `vigil investigate <alert-key>` — resolves to the most recent open incident file
  for that key (via the same `incidents::list`/lookup the `Incidents` command
  already uses), runs the read-only agent against it, appends `## Diagnosis` /
  `## Suggestions` / optionally `## Proposed fix`.
- `vigil fix <incident-file>` — takes the incident file path directly (as shown in
  the notification and in `vigil incidents --show`). Errors clearly if the file has
  no `## Proposed fix` block. Otherwise parses the plan JSON, prompts for
  step-by-step or all-steps approval, builds the scoped tool config, runs the
  execute-agent, appends `## Fix execution`.

`vigil watch`'s alert-fired path changes from "call `maybe_diagnose_alert_async`"
to "write the stub incident file (title/key/rule message only) and notify with the
`vigil investigate <key>` hint" — no agent process is spawned from the watch loop
anymore. `is_auto_diagnose_worthy` (today's noise filter on *which alerts* auto-
diagnose) becomes moot for the auto path and is deleted; its job is superseded by
the user's own opt-in decision.

## Error handling and journal format

**Partial approval:** if the user approves only some steps of a multi-step plan, the
execute-agent's instruction contains only the approved subset — it has no knowledge
of the rejected steps at all, not even that they existed.

**Mid-plan failure:** if a step fails — the re-verify check doesn't match what the
plan expected, or the action itself errors — the execute-agent **aborts remaining
steps** rather than continuing. This follows directly from the PID-reuse precedent
above: once reality has diverged from what the plan assumed, continuing on the
original plan is riskier than stopping and surfacing that divergence to the user.

**Journal format** — `## Fix execution` appended to the same incident file:

```
## Fix execution
_Approved: 2026-08-09 02:30 (steps 1, 2 of 2)_

1. kill_process — Kill stale claude session (pid 72837)
   done — verified pid 72837 no longer running

2. delete_path — remove orphaned node_modules cache
   aborted — target path changed since plan was proposed, stopped before acting

---
_Tokens: ... — ~$..._
```

Same token/cost footer convention as `## Diagnosis` already uses
(`format_usage_footer` in `diagnose.py`), reused for the execute-agent's own session.

**`IncidentTracker` interaction:** executing a fix does **not** mark the incident
resolved. `IncidentTracker::is_new_incident`'s open/closed state stays driven purely
by whether the target keeps re-firing within its timeout window, unrelated to
whether a fix was ever proposed or run. This is deliberate: "the agent reports it
took an action" and "the underlying condition actually cleared" are different claims,
and conflating them would let a failed or partially-effective fix read as resolved
when the next snapshot might show the same alert firing again five minutes later.

## What's explicitly out of scope for this spec

- The interactive terminal approval UX (single global y/n vs. per-step) is a small
  implementation detail, not a design fork — left to the implementation plan.
- Prompt tuning for the investigating agent's `## Proposed fix` judgment (when it
  should vs. shouldn't propose one) is expected to need live iteration once built;
  this spec fixes the *mechanism*, not the exact prompt wording.
- A formal ADR documenting this as an amendment to AGENTS.md's "Rust never decides or
  fixes anything" rule should be written alongside implementation, following this
  project's existing convention (`docs/decisions/0001`–`0005`).
