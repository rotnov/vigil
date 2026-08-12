# Investigate/fix desktop UI: a Tauri companion to the CLI

Status: approved design, pending implementation plan.

## Why

`vigil investigate <key>`/`vigil fix <file>` (shipped in
[2026-08-09-fix-execution-workflow-design.md](2026-08-09-fix-execution-workflow-design.md))
work, but only from a terminal: read a markdown file, run a command, watch a
`y`/`N` prompt scroll by. For the common case — a push notification lands, the
user wants to glance at what's actually running and decide in a few seconds
— that's real friction. This design adds a small desktop companion that
turns the same CLI machinery into a glanceable window: tap a notification (or
click the existing menu-bar icon), see the diagnosis and the actual current
process tree behind it, approve or reject a proposed fix with a click.

A working visual mockup of the target screen already exists and was approved
before this design: `vigil-investigate-mockup.html` (published as a Claude
Artifact during this session) — diagnosis card, a process tree scoped to the
incident's flagged group (with zombie/idle/leak status chips, live
CPU/RSS/age), and a proposed-fix approve/reject card. This spec is the
architecture behind making that screen real, not a redesign of it.

## Non-goals

- No redesign of the approved mockup's visual language — implementation
  should match it, not reinterpret it.
- No change to `vigil investigate`/`vigil fix`'s actual behavior, output
  format, or safety rails (categories, hard floor, per-step approval
  semantics) — the UI is a client of that existing, already-reviewed CLI
  surface, not a reimplementation of it. It shells out to the real binaries.
- No change to alert *rules* (thresholds, `is_journal_worthy`'s key list) —
  orthogonal to this design.
- Not cross-platform. macOS only, matching the rest of vigil.

## Architecture

Two processes, not one — deliberately, per the user's explicit choice not to
retire the existing Rust menu-bar app:

1. **`vigil menubar`** (existing Rust binary, `src/menubar.rs`/
   `src/menubar_loop.rs`) — unchanged in every respect except one: its
   dropdown's click handler, which today opens an incident's raw `.md` file
   in whatever app handles markdown, instead hands off to `vigil-ui` (below)
   with the incident's path, so the same click now opens the rich window
   instead of a text file. Everything else about it — the tray icon, its
   color logic, its own polling of the incidents dir and the status file —
   is untouched. `docs/decisions/0002-menu-bar-health-indicator.md` still
   describes it accurately.
2. **`vigil-ui`** (new — a Tauri app, its own top-level project directory
   alongside `src/` and `agent/`, e.g. `ui/`) — a background-resident helper
   (`LSUIElement`, no Dock icon, no tray icon of its own — one tray icon in
   this system is enough) responsible for:
   - Polling `~/.vigil/incidents/` for new journal-worthy stubs (same
     detection logic `menubar_loop.rs` already has for its dropdown — a
     shared small crate, or just a second, independent implementation of
     the same `incidents::list`-based scan, is an implementation-time
     call, not a design one).
   - Posting the *actual* macOS notification for those stubs, via Tauri's
     notification plugin (which, unlike `osascript display notification`,
     supports a real click action because it's posted by a genuinely
     signed app bundle) — clicking it opens/focuses the investigate window
     for that specific incident. This is trigger **A**.
   - Listening for open-requests from `vigil menubar`'s dropdown (a custom
     URL scheme, e.g. `vigil://incident/<path>`, or Tauri's single-instance
     plugin routing a re-invocation with an argument to the already-running
     instance — an implementation choice, not a design one) and opening/
     focusing the same window. This is trigger **B**.
   - Owning the investigate/fix window itself (below).

**`src/watch.rs`/`src/ui_loop.rs` change once more, narrowly:** the
`is_journal_worthy` branch that currently calls
`crate::alerts::notify(&crate::agent::augment_with_investigate_hint(...))`
stops doing that — journal-worthy alerts still get their stub written
exactly as today, they just no longer also fire vigil's own notification,
since `vigil-ui` now owns notifying for exactly that set of alerts. The
non-journal-worthy branch (plain `notify(&alert)`, no stub) is completely
unaffected. This was a deliberate choice over accepting a redundant
double-notification, made explicitly during brainstorming.

**Consequence worth stating plainly:** `vigil-ui` must be running for
journal-worthy alerts to notify at all now — it joins `vigil watch`/`vigil
menubar` in the set of processes this project's own workflow already
requires restarting after a merge (see AGENTS.md's Git workflow section),
and should be registered as a `LaunchAgent` so it survives reboots the same
way a login item would, rather than depending on someone remembering to
launch it by hand each session.

## Data flow

`vigil-ui` does not reimplement any diagnosis or execution logic — it is a
thin client over the same CLI surface a terminal user already has:

- **Opening a window for a stub with no diagnosis yet** (the normal case for
  trigger A/B on a fresh journal-worthy alert): the window opens into a
  loading state and `vigil-ui`'s Rust backend spawns `vigil investigate
  <key> --incidents-dir ... [--watch-log ...]` as a child process, waits for
  it to exit, then re-reads the now-updated incident file.
- **Reading the incident file's structure without re-parsing markdown by
  hand in JS:** add a small, additive CLI surface —
  `vigil incidents --show <file> --json` — that emits the same information
  `incidents::extract_title`/`extract_rule_message`/`extract_command`
  already parse out, plus the `## Proposed fix` plan (if
  `fixplan::extract_proposed_fix_json`/`parse_plan` found one) and the `##
  Fix execution` report (if present), as one structured JSON object instead
  of raw markdown. This is a small, additive flag on an existing command —
  it doesn't change `--show`'s existing plain-text output, it's a new mode.
- **The process tree is not sourced from the agent's diagnosis text** — that
  would be fragile prose-parsing of something an LLM wrote for a human to
  read, not a machine to consume. Instead, `vigil-ui`'s Rust backend
  independently queries live process state (`sysinfo`, the same crate
  `src/snapshot.rs` already depends on) scoped to whatever the alert key
  names — a process name for `cpu_hog:<pid>`/`high_process_count:<name>`, or
  the specific pid for a `cpu_hog:<pid>` alert — at the moment the window
  renders, not from whatever the snapshot looked like when the alert fired.
  This is what makes the tree "only actual, current" processes, per the
  original request, and is also what supplies zombie status (process state)
  and current RSS/CPU/age, none of which the incident file itself carries.
- **Approving/rejecting a fix step:** the window's proposed-fix card is
  rendered from the `--json` output's plan. On approve/reject, `vigil-ui`
  spawns `vigil fix <incident-file>` as a child process with a piped stdin,
  reads its printed per-step prompt the same way a terminal would, and
  writes `"y\n"`/`"N\n"` to that stdin instead of a human typing it — the
  existing interactive prompt loop in `fix_process.rs` is completely
  unaware it's talking to a program instead of a person, and needs no
  changes. Output is streamed back to the window as it's produced.

## Error handling

- `vigil investigate`/`vigil fix` failing to spawn, exiting non-zero, or
  hanging past a reasonable timeout: the window shows an explicit error
  state (what failed, and the raw stderr if any) instead of an indefinite
  spinner. No silent retries.
- If `vigil-ui` itself isn't running when a journal-worthy alert fires, that
  alert is silent until `vigil-ui` starts and next polls the incidents
  directory and notices the (already-written, undiagnosed) stub — it is not
  lost, since the stub file itself was still written by `vigil watch`
  regardless of whether `vigil-ui` was up to notify about it. This is a
  degraded, not broken, failure mode, and is exactly why persistent
  (LaunchAgent) operation matters.

## Testing

`vigil-ui`'s Rust backend (subprocess spawning/stdin-writing, JSON parsing
of `--json` output, the `sysinfo` process-tree query, incident-file
polling/dedup logic) is unit-tested the same way the rest of this project
is — pure logic separated from OS-boundary glue, `cargo test`, following
this project's existing split (see AGENTS.md's testing section). The new
`vigil incidents --show --json` flag's JSON-shape-building logic is pure and
testable the same way `extract_rule_message`/`extract_command` already are;
wiring it into the CLI is OS-boundary glue like the rest of `main.rs`. The
webview frontend (the mockup's HTML/CSS/JS, made data-driven) has no
automated test suite for this first version — manual smoke testing only,
proportionate to this project's size, matching how the TUI and menu bar are
already only verified by hand per AGENTS.md's testing section.

## Consequences

- `docs/decisions/0002-menu-bar-health-indicator.md` remains accurate as
  written — `vigil menubar` isn't being replaced, just given one new
  integration point (its dropdown click hands off instead of opening a raw
  file).
- A new top-level project (`ui/`, Tauri: Rust + a webview) joins the
  repository's existing two (`src/`, the Rust CLI; `agent/`, the Python
  diagnosis layer) — a third build target, a third place dependencies live,
  and a third thing to keep running in the background. Worth a fresh look at
  whether `AGENTS.md`'s project-wide sections (testing command, "the live
  incident-monitoring loop") need a `vigil-ui` subsection once this is
  implemented, not just left implicit.
- `src/watch.rs`/`src/ui_loop.rs` need one small, targeted follow-up change
  (removing the journal-worthy notify call) on top of already-shipped,
  reviewed code — small in size, but real: it needs its own task, its own
  test-suite re-run, and its own review, the same discipline the rest of
  this project's changes get.
