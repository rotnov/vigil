---
id: 0004
title: "high_process_count: detecting leaked/zombie process accumulation"
status: accepted
---

## 0004: Leaked process detection

Note: `agent::is_auto_diagnose_worthy`, referenced below, was later replaced by
`agent::is_journal_worthy` — see ADR 0006.

- Status: accepted
- Context: a live investigation (2026-08-07) found 224 `node` processes on the
  maintainer's machine sitting at 0.0% combined CPU — turned out to be MCP server
  subprocesses (`playwright-mcp`, `mcp-server-puppeteer`, `mcp-remote`,
  `context7-mcp`, `firebase mcp`, `mcp-server-github`, `pubmed-mcp-server`, ...)
  spawned via `npx` by Claude Code/Codex/Devin sessions that had long since closed,
  never cleaned up — some over two weeks old per `run_time_secs`. None of vigil's
  existing rules caught this: `cpu_hog`/`high_load` only look at per-process or
  overall CPU (each individual leaked process used ~0%), and `swap_pressure`/
  `low_memory` only cite a process *group* when its combined memory materially
  dominates the single top process (`GROUP_VS_SINGLE_RATIO`) — the group's ~9.6GB
  never crossed that bar against a much larger single process (PyCharm, ~17GB).
  There was no signal at all for "abnormally many instances of one process,
  collectively idle" — a distinct failure mode (process/resource leakage) from load
  or memory pressure, which is what every existing rule actually watches for.
- Decision:
  1. **New rule, `high_process_count`, over `Snapshot::top_mem_groups`** (the same
     per-name aggregation `swap_pressure`/`low_memory` already use, see
     `group_by_name`) — fires when a group's `count` exceeds
     `PROCESS_COUNT_THRESHOLD` (first-guess 100) **and** its `total_cpu_pct` is
     under `LEAKED_GROUP_CPU_IDLE_PCT` (first-guess 5%). Both conditions together,
     not either alone: count alone would false-positive on legitimately
     multi-process apps (36 Chrome renderer helpers, 80 WebKit processes observed
     on this machine are both normal), and near-zero CPU alone would false-positive
     on any briefly-idle moment for a small, unremarkable process group. The
     combination — *many* instances, *collectively* barely doing anything — is what
     actually distinguishes an accumulating leak from ordinary multi-process
     software.
  2. **Concrete pid/ppid/age detail in the message, not just a count.** `ProcGroup`
     gains `oldest: Vec<ProcSample>` (`{pid, ppid, run_time_secs}`), populated by
     `group_by_name` as the `OLDEST_SAMPLES_PER_GROUP` (3) longest-running members
     of the group. The alert message renders these directly (`pid 41181 (ppid
     40723, 14d), ...`) — something to actually act on (`ps -p <pid> -o
     ppid,etime,command`) rather than a bare "224 node processes" that gives no
     starting point for the next step. `ProcInfo` gains a matching `ppid:
     Option<u32>` (from `sysinfo::Process::parent()`).
  3. **`ppid` is logged for context, not used as the detection signal itself.**
     Checked empirically against the real leaked processes before deciding this:
     none had `ppid == 1` (reparented to `launchd`, the classic "true orphan"
     signal) — their parents were still-alive `npm exec` wrapper processes, several
     layers removed from the actual dead session that originally caused the leak.
     Walking that chain to find a genuinely dead ancestor is real complexity for a
     signal that, in the one real case observed, wouldn't even have fired. Age +
     idle CPU is simpler and is what the actual data showed.
  4. **Auto-triggers the background agent diagnosis** (`agent::is_auto_diagnose_worthy`
     gained `high_process_count:` alongside `high_load`/`cpu_hog:`/`battery_low`) —
     confirming a flagged group is actually safe to kill (vs. e.g. a legitimate
     worker pool doing bursty background work vigil's one-sample threshold happened
     to catch mid-idle) needs the same kind of live investigation a CPU spike does,
     not a static rule's one-shot judgment.
- Alternatives considered: **count alone** — rejected, would misfire on Chrome/
  WebKit's naturally large process counts. **Walking the parent chain to detect true
  orphans (`ppid == 1` after following dead intermediate parents)** — rejected per
  point 3, doesn't match the real leak pattern and adds real complexity. **A
  dedicated `Snapshot` field carrying the full, untruncated process list** (instead
  of extending `ProcGroup` with a bounded sample) — rejected as unnecessary scope
  increase; `oldest`'s 3-per-group cap keeps every snapshot's size bounded the same
  way `top_cpu`/`top_mem`/`top_mem_groups` already are, and 3 concrete examples is
  enough to act on without needing every one of 224 pids.
- Consequences: `ProcInfo` and `ProcGroup` (both part of the JSON `Snapshot`
  contract the Python agent also reads) gained fields — `to_proc_info`/
  `group_by_name` are the only producers, both already pure/unit-tested, so this
  didn't touch `take_snapshot`'s other logic. Both thresholds are explicitly
  first-guess, calibrated against the one real machine/incident that motivated this
  rule, the same as `docs/decisions/0001-network-connection-monitoring.md`'s
  connection-count thresholds — expect to tune from further field data.
