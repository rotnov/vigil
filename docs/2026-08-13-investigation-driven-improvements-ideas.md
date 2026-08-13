# Ideas: feeding real investigations back into vigil (raw capture, not a design)

Status: unreviewed idea capture from a live `vigil-ui` testing session. Not a
design spec — brainstorming was paused mid-way to get this written down
before continuing. No approach chosen yet for any of the three ideas below.

## Context

`vigil-ui` (merged in PR #16, 2026-08-12/13) made it possible to watch a real
`vigil investigate` run end-to-end for the first time. Two real things
surfaced during that live session that motivated these ideas:

1. A diagnosis for `cpu_hog:20133` (a legitimate pytest run under a ChatGPT
   Codex agent, not a runaway process) noted, unprompted, that `sysmond`
   (root, pid 751) was pinned near 98% CPU with ~168h of accumulated
   runtime since 2026-06-17 — abnormal, unrelated to the alert that
   triggered the investigation. A real finding an investigation surfaced on
   its own, not something the alert rule was looking for.
2. Watching the investigate-agent's process tree live showed it spawning
   three unused MCP server child processes (`@playwright/mcp`,
   `@upstash/context7-mcp`, `firebase-tools mcp`) that the `claude` CLI
   auto-loads from global config, despite the diagnose agent's
   `allowedTools` being `Bash,Read,Grep,Glob` only (see
   `agent/src/vigil_agent/diagnose.py`) — real startup overhead on every
   `vigil investigate`/`vigil fix` call, working directly against the
   project's own governing design goal (vigil's own CPU/token/turn cost
   counts against it).

## Idea 1 — pre-compute diagnostic context into the diagnose prompt (concrete, well-scoped)

Give the diagnose agent cheap-to-precompute facts vigil already has, instead
of letting it spend Bash-tool-call turns re-deriving them (parent pid,
process status, etc. via `ps`/`sample`).

Starting point discussed: the process tree. `ui/src-tauri/src/process_tree.rs`
already has a tested, working `query_process_tree`/`scope_for_alert_key`
(scopes a live process tree to an alert key — `Pid` or `Name` — via
`sysinfo`), currently only consumed by the UI. The diagnose prompt itself is
built in `src/agent.rs::build_diagnosis_question` (Rust side, becomes the
Python agent's user question) and the system prompt lives in
`agent/src/vigil_agent/prompts.py`.

Open questions for when this gets designed:
- Does the process-tree logic move to the main crate (so both `src/` and
  `ui/` can use it without duplication — `ui/` currently reimplements it
  independently per the investigate-ui design's own stated tradeoff), or
  does `agent.rs` grow its own copy?
- How much tree context is worth the extra prompt tokens vs. leaving it for
  the agent to fetch on demand only when actually needed?
- Format: inline text in the question, or a structured block the system
  prompt tells the agent how to read?

## Idea 2 — accumulate a table of investigation findings over time (open-ended, likely a separate project)

As real investigations land (not just real *incidents* — the diagnosis
*content* itself), collect what each one needed, found, or worked around
into some structured record, and periodically look at that record for
patterns worth turning into new vigil capability (new context to
pre-compute, a new tool, a new alert rule). Distinct from the project's
existing incident-pattern loop (AGENTS.md's "live incident-monitoring
loop" section), which is about alert *rules* re-firing — this is about
what investigations themselves reveal or need, which can surface from a
single real investigation (both examples above did).

Not scoped at all yet: where this record lives (a file? a `vigil`
subcommand? something Claude just does by re-reading `~/.vigil/incidents/`
periodically, no new tooling required?), how often it's reviewed, what
counts as "a pattern" worth acting on for a *investigation-need* signal
vs. an *alert-rule* signal.

## Idea 3 — recognize known-benign resource patterns to reduce/suppress noise

Raised directly from the `sleep`/pytest example: if a resource-heavy
process is recognizable as a known-benign pattern (a test run, a build,
etc.), maybe it shouldn't alert (or shouldn't trigger a full investigation)
at all. User's own framing: "maybe write some skills for the agent, I don't
know" — genuinely unscoped; could mean agent-side heuristics, alert-rule-side
allowlisting, or something else. Needs real thought before this becomes a
design, since false-negative risk (suppressing a real problem because it
superficially resembles a benign pattern) is the obvious danger to design
against.

## Next step

Resume brainstorming (`superpowers:brainstorming`) on Idea 1 first — it's
the concrete, well-scoped one — when there's time for the full
clarifying-questions/approach-comparison process. Ideas 2 and 3 stay here
as notes until they're picked up, and may end up as separate specs rather
than bundled with Idea 1.
