---
id: 0005
title: "Stable IncidentTracker targets for aggregate alerts, sustained-duration gating for high_load"
status: accepted
---

## 0005: Stable incident targets + sustained high_load

- Status: accepted
- Context: the user reported vigil's native notifications firing too often. `grep`ing
  `~/.vigil/watch.log` for `high_load` confirmed it with real data: on this machine
  (chronically near its load/memory ceiling — see the many incidents referenced
  throughout `AGENTS.md`), `high_load`'s own message cites a "top consumer" that
  rotates almost every firing — Claude Helper, Activity Monitor, pycharm, suggestd,
  vigil, Creative Cloud, iTerm2, Discord Helper, mdworker_shared, contactsd, codex —
  because `top_cpu.first()` is whoever happens to be busiest in that one 5-second
  sample, not a claim about what's actually causing sustained elevated load (one
  incident's own agent diagnosis said as much: "summing every process in `top_cpu`
  gives ~737%, nowhere near explaining load 29.6 — the real driver is memory
  pressure"). `alerts::IncidentTracker` dedups by `Alert::target`, which for
  `high_load` was set to that same rotating top-consumer name — so nearly every
  firing looked like "a different target" and got a fresh notification, a fresh
  background agent diagnosis (real token cost — several of these ran $0.50–$1.75 each
  per the token footer), and a fresh journal entry, even when firings were minutes
  apart and the actual condition (system-wide overload) never stopped.
- Decision:
  1. **`target` means different things for different rules, and that's made explicit
     now** (see `Alert::target`'s doc comment). For rules fundamentally *about* one
     specific process/group — `cpu_hog:<pid>`, `high_process_count:<name>` — `target`
     stays the actual process/group name, since a genuinely different process
     triggering the rule is a genuinely different incident (this is the behavior
     `IncidentTracker` was originally built for, and it's still correct there). For
     rules about an *aggregate system condition* — `high_load`, `swap_pressure`,
     `low_memory`, `battery_low` — `target` is now a fixed sentinel equal to the
     rule's own key (`Some("high_load")`, etc.), not the rotating top-consumer name.
     The message text is untouched — it still names the actual top consumer as a
     diagnostic hint — only the dedup key changed.
  2. **`high_load` additionally requires the load average to stay above threshold
     continuously for `HIGH_LOAD_SUSTAINED_DURATION` (30s)** before firing at all —
     a new `AlertState::high_load_since: Option<Instant>`, set on first crossing,
     cleared the moment load drops back under threshold. This is a *different* fix
     for a *different* (smaller) problem than point 1: `load_avg.one` is already a
     1-minute OS-smoothed average, not noisy sample-to-sample the way instantaneous
     CPU% is, so this isn't a debounce against transient spikes (there mostly aren't
     any at this granularity) — it only filters a threshold crossing that happens to
     sit right at the boundary and dips back down within the 30s window, the same
     spirit as `cpu_hog_streak`'s 3-sample requirement but duration-based instead of
     sample-count-based (so it doesn't depend on `--interval`).
- Alternatives considered: **requiring a sustained streak for `high_load` alone,
  without the stable-target fix** — considered and rejected once real `watch.log`
  data was checked: load average genuinely stays elevated for extended periods on
  this machine (confirmed by the diagnoses themselves), so the repeat firings weren't
  transient spikes needing debounce — they were the same sustained condition being
  misclassified as new each time because of the rotating process name. A streak
  requirement alone wouldn't have fixed the reported spam. **Coalescing by time
  window instead of by target** — rejected again for the same reason
  `agent::DiagnosisCoalescer` was rejected in favor of `IncidentTracker` in the first
  place (see `AGENTS.md`'s "live incident-monitoring loop" section): a genuinely
  independent `cpu_hog`/`high_process_count` finding in the same window would get
  silently dropped. The fix here narrows *which* alerts use target-based dedup
  instead of replacing the mechanism.
- Consequences: `high_load`/`swap_pressure`/`low_memory`/`battery_low` now correctly
  read as one ongoing incident across repeat firings, cutting both notification
  frequency and background-diagnosis token spend for the common case (the machine
  staying loaded for an extended stretch) without touching `cpu_hog`/
  `high_process_count`'s existing, correct, per-process behavior.
  `HIGH_LOAD_SUSTAINED_DURATION` is a first-guess constant, same as every other
  threshold in this file — expect to tune from further field data.
