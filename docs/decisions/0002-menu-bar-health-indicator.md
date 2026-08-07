---
id: 0002
title: "Menu bar health indicator: status-file handoff instead of a second sampling loop"
status: accepted
---

## 0002: Menu bar health indicator

- Status: accepted
- Context: native macOS notifications (`osascript display notification`) were the only
  passive signal vigil produced, and even after `alerts::IncidentTracker` (see the
  commit replacing `agent::DiagnosisCoalescer`) cut down repeat firings, a notification
  is still an interruption by design — it has to be, to be noticed at all. The user
  asked for a persistent, glanceable macOS menu bar status item instead: transparent/
  unobtrusive when the machine is healthy, a color otherwise, and a way to see recent
  incidents without that meaning a fresh interruption each time. `vigil incidents`
  already solved "read the journal from a plain shell"; this is the passive-glance
  equivalent.
- Decision:
  1. **Dependencies**: `tray-icon` + `tao` (not a second GUI toolkit, not a Swift/
     SwiftUI helper app). `tray-icon` needs a native event loop on macOS for the
     `NSStatusItem`/`NSMenu` machinery; `tao` (a `winit` fork maintained by the same
     org as `tray-icon`/`muda`) is the event loop it's built and documented against.
  2. **Shape**: a new `vigil menubar` subcommand, not a separate crate/binary. Same
     reasoning as `vigil ui` already being a subcommand rather than its own binary —
     one build, one install step, consistent with every other vigil surface. Like
     `vigil ui`, it blocks for the process's whole lifetime (`tao`'s
     `EventLoop::run` never returns), so it's launched as its own long-running
     process the same way `vigil watch`/`vigil ui` already are.
  3. **Health source: a status file written by `vigil watch`, not a second sampling
     loop in `vigil menubar` itself.** This was the real design question. A menu bar
     process could re-run `take_snapshot` + `alerts::evaluate` on its own timer, but
     that duplicates real sampling/evaluation work (`sysinfo` refresh, `netstat`/
     `pmset` shells) alongside the `vigil watch` process already doing exactly that
     — directly working against this project's own governing design goal (vigil's
     own overhead counts against it; `IncidentTracker` itself exists for the same
     reason). Instead, `vigil watch`'s tick loop writes a small JSON status file
     (`~/.vigil/status.json` by default, `--status-file` to override) every tick:
     `{"updated_unix": <u64>, "open_count": <usize>}`, where `open_count` comes from
     a new `IncidentTracker::open_count(timeout, now)` — a read-only count of targets
     still within their open incident window, reusing state the tracker already
     maintains. `vigil menubar` only polls this file (a `stat`+small read, default
     every 3s via `--poll-secs`) — no snapshot-taking of its own. The dropdown menu's
     recent-incidents list is populated separately, directly from
     `incidents::list`/`extract_title` (the same functions `vigil incidents` uses) —
     it doesn't need anything from the status file, since a journal file only exists
     once a diagnosis has actually completed and been written.
  4. **Health classification** (pure, unit-tested, no `tao`/`tray-icon` involved):
     `open_count == 0` → `Ok` (transparent/neutral icon — explicitly "don't disturb"
     per the user's own framing), `== 1` → `Warning` (yellow), `>= 2` → `Critical`
     (red, "multiple concurrent" reading of the user's ambiguous "green/transparent"
     framing — resolved as: those are the same state, described twice). A fourth
     state, `Unknown` (status file missing, or its `updated_unix` older than
     `3 * poll_secs`, meaning `vigil watch` isn't running or died) — shown as a
     distinct gray icon rather than silently defaulting to the healthy-looking
     transparent one, since "no data" and "confirmed healthy" are different facts
     and collapsing them would misrepresent a dead watcher as an all-clear.
  5. **Icon rendering**: a small RGBA circle drawn procedurally at runtime
     (`tray_icon::Icon::from_rgba`), not a bundled asset file — one less thing to
     ship/locate at install time, and the four states are simple enough (a filled or
     mostly-transparent circle) that hand-drawing pixels is less code than wiring in
     an image-asset pipeline for four tiny PNGs.
- Alternatives considered: a second independent sampling loop in `vigil menubar` —
  rejected per point 3 (duplicate OS-shelling/CPU cost, the exact kind of overhead
  `IncidentTracker` was just built to cut down elsewhere). A local web dashboard —
  rejected earlier in the same conversation as a heavier, network-adjacent surface
  that also doesn't solve "glanceable without an open tab," which was the actual
  complaint. Bundling an icon file — rejected per point 5. Deriving health from the
  incident *journal's* file timestamps alone (no status file, no `vigil watch`
  change) — considered and rejected: `IncidentTracker` only writes one journal file
  per incident *lifecycle*, so a long-running incident that keeps re-firing every few
  minutes for an hour produces exactly one file with a stale mtime, which is
  indistinguishable from "resolved an hour ago" using file timestamps alone. Only
  `vigil watch`'s own live `IncidentTracker` state actually knows the difference.
- Consequences: `vigil watch` gains a `--status-file` flag and an unconditional
  per-tick status write (independent of `--no-notify`, so the menu bar still reflects
  reality even if native notifications are suppressed; the count is naturally 0 in
  that mode today since alert evaluation itself lives inside the `!no_notify` branch —
  acceptable, since `--no-notify` already means "I don't want vigil telling me
  things"). `vigil menubar` has no access to *why* an incident is open beyond the
  count — for the actual diagnosis text, its dropdown points at the journal the same
  way `vigil incidents` does. `alerts::IncidentTracker` gains one new read-only method,
  `open_count`, alongside the existing mutating `is_new_incident`.
