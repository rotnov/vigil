---
id: 0001
title: "Network connection monitoring: netstat over lsof, and a same-listen-port heuristic for 'incoming'"
status: accepted
---

## 0001: Network connection monitoring

Note: `agent::is_auto_diagnose_worthy`, referenced below, was later replaced by
`agent::is_journal_worthy` — see ADR 0006.

- Status: accepted
- Context: vigil had no network visibility at all — no field in `Snapshot`, no alert
  rule, `sysinfo` (already a dependency) exposes only interface-level RX/TX byte
  counters, not per-connection state. The user asked for a connection-count metric,
  then specifically for a dedicated alert on *incoming* connections (as opposed to
  just a raw total), matching this project's existing pattern of shelling out to a
  system tool for data `sysinfo` doesn't cover (`pmset` for battery in
  `read_battery`/`parse_battery_line`).
- Decision:
  1. **Collection**: shell out to `netstat -an -f inet` and `netstat -an -f inet6`
     (two calls, concatenated) rather than `lsof -i`. Plain `netstat -an` (no `-f`
     filter) was tried first and rejected — on this machine it returns tens of
     thousands of Unix-domain-socket lines before any TCP/UDP entries, making it both
     slow to parse and noisy. `-f inet`/`-f inet6` alone returned 743/260 lines in a
     live test — fast, and every line is either a real TCP/UDP row or a fixed two-line
     header, both cheap to filter in `parse_netstat_output`. `lsof -i` was considered
     for its per-process attribution (an alert could then name the offending process,
     matching `Alert.target`'s pattern for other rules), but it's markedly slower to
     invoke, and per-connection process attribution isn't needed for a threshold-only
     rule — `alerts.rs` never attributes network alerts to a `target` (see point 3).
  2. **Parsing**: a pure function, `parse_netstat_output(output: &str) ->
     ConnectionCounts`, kept separate from the two `Command` calls in
     `collect_connections()` — the same split every other OS-shelling collector in
     this codebase already uses (`parse_battery_line` vs. `read_battery`,
     `build_args` vs. `ask`'s spawn). Classifies each `tcp*`-prefixed row by its last
     whitespace-separated column (`ESTABLISHED`/`LISTEN`/`TIME_WAIT`/`CLOSE_WAIT`/
     other) into a `ConnectionCounts` struct on `Snapshot`. Tested against a real
     captured sample (`NETSTAT_SAMPLE` in `src/main.rs`'s test module) rather than only
     synthetic lines, since `netstat`'s column layout is exactly the kind of thing an
     invented fixture could get subtly wrong.
  3. **"Incoming" heuristic**: `netstat` records connection *state*, not *direction* —
     there is no column saying who dialed whom. `ConnectionCounts.incoming` counts
     `ESTABLISHED` rows where (a) the local port matches one of this machine's own
     `LISTEN` ports (collected in a first pass over the same output) and (b) the
     remote/foreign address is not loopback. Rationale: a connection *we* initiate
     almost always uses an ephemeral local port, not one of our own listening ports;
     a connection someone *else* opened to a service we're running keeps our side's
     local port pinned to that service's listening port. Loopback is excluded
     specifically because two local processes talking over `127.0.0.1` (e.g. an IDE
     and its language server — the majority of this machine's own `ESTABLISHED`
     traffic in the sample captured for the test fixture) would otherwise match the
     same local-port condition and produce constant false positives; excluding it
     leaves only genuine external inbound connections. This is a heuristic, not a
     kernel-verified fact — no attempt is made to distinguish it from, say, a
     hairpinned connection through a non-loopback local IP back to a local service.
  4. **Alert rules**: two new rules in `alerts.rs`, both single-sample threshold
     checks (no streak, matching `swap_pressure`/`low_memory`'s shape rather than
     `cpu_hog`'s) against `HIGH_CONNECTION_COUNT_THRESHOLD` (1500, total) and
     `INCOMING_CONNECTIONS_THRESHOLD` (10, `incoming`). Neither rule sets
     `Alert.target` — there's no single process a connection-count anomaly can be
     pinned to the way a CPU/memory alert names a `ProcInfo`.
- Alternatives considered: `lsof -i -n -P` for direct per-process breakdown — rejected
  per point 1 (slower, and unneeded for a threshold-only rule). A single `netstat -an`
  call with protocol filtering done in Rust — rejected per point 1 (the unfiltered
  Unix-socket volume makes even one call materially slower and the parse noisier for
  no benefit, since `-f inet`/`-f inet6` already do that filtering for free). True
  direction detection via `lsof`'s or `netstat`'s more verbose per-socket detail, or
  correlating against `procfs`-equivalent state — none exist as a lightweight
  cross-checkable signal on macOS the way they might on Linux; the listen-port
  heuristic was judged good enough for an anomaly *alert* (not a security audit tool)
  given the false-positive risk is bounded by also requiring non-loopback.
- Consequences: `Snapshot` gains an optional `connections: Option<ConnectionCounts>`
  field (`None` only if both `netstat` invocations fail to spawn, e.g. if it's ever
  missing from `$PATH`). Two new alert keys, `high_connection_count` and
  `incoming_connections`, join `agent::is_auto_diagnose_worthy`'s exclusion list
  implicitly (neither is in that allowlist, so — like disk/plain-memory alerts —
  they don't auto-trigger a background diagnosis; the interactive `a`-key ask can
  still be pointed at one). Both thresholds are explicit first guesses (see the
  comment above their `const` definitions in `alerts.rs`) calibrated against a single
  live capture on one dev workstation (~570 total connections observed, near-zero
  genuine external incoming), not a fleet baseline — expect to retune from real
  incident data the way other rules' thresholds already have been (see
  `AGENTS.md`'s "live incident-monitoring loop" section).
