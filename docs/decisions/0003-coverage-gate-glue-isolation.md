---
id: 0003
title: "Coverage gate: isolate irreducible OS-boundary glue into dedicated excluded files"
status: accepted
---

## 0003: Coverage gate — isolate irreducible OS-boundary glue

- Status: accepted
- Context: AGENTS.md declared a hard `cargo llvm-cov --workspace --fail-under-lines
  99.9` rule (2026-08-07), acknowledging at the time that measured coverage was
  73.44% and that closing the gap was ordinary project work, not yet done. Most of
  the gap turned out to be genuine OS-boundary code — `main()`'s CLI dispatch, the
  `vigil watch` sampling loop, the `vigil ui` terminal event loop (`crossterm` raw
  mode, blocking `event::read()`), the `vigil menubar` tray event loop (`tao`'s
  `EventLoop::run` never returns; `muda::Menu::new` panics off the main thread —
  verified empirically, not assumed), the actual `uv run vigil-agent` process spawn,
  and the `osascript` notification shell-out — none of which a unit test can
  exercise without either mocking the OS/GUI layer (disproportionate for a
  single-maintainer tool this size) or accepting flaky/disruptive tests (a real
  `osascript` call pops a visible notification on every `cargo test` run; a real
  terminal-raw-mode loop only exits on a keypress a test harness can't synthesize).
- Decision:
  1. **Isolate the irreducible glue into dedicated files, not scattered
     `#[cfg(...)]` markers.** Six new files hold *only* OS-boundary code with no
     meaningfully-separable logic left in them: `watch.rs` (the `vigil watch` loop
     body), `ui_loop.rs` (the `vigil ui` terminal loop, moved out of `ui.rs`),
     `menubar_loop.rs` (the tray event loop plus `build_menu`, moved out of
     `menubar.rs` once `muda::Menu::new`'s main-thread panic was confirmed),
     `agent_process.rs` (`ask()`'s actual `Command` spawn plus
     `maybe_diagnose_alert_async`'s thread spawn, moved out of `agent.rs`),
     `notify.rs` (the `osascript` call, moved out of `alerts.rs`), and `main.rs`
     itself reduced to just `Cli::parse()` + delegation. Every one of these files'
     doc comments states why it's excluded, matching AGENTS.md's existing
     allowance for "a whole-file `--ignore-filename-regex` entry."
  2. **Everything else gets real tests, not exemptions** — including OS-shelling
     code that previously looked untestable. `take_snapshot`, `collect_disks`,
     `read_battery`, `collect_connections` all call real macOS commands
     (`sysinfo`, `pmset`, `netstat`) and are exercised by calling them directly in
     tests, not mocked — this project only runs its test suite on the
     maintainer's own Mac (see AGENTS.md), so the real command is available and a
     mock would just be a second, driftable description of the same output shape
     `parse_battery_line`/`parse_netstat_output` already parse and unit-test on
     their own. Two tests (`read_battery`, `collect_connections`) were tightened
     from a silent `if let`/early-`return` to `.expect(...)` after review found
     they'd pass vacuously if the underlying shell-out ever broke.
  3. **`Commands::Incidents` was refactored to return an exit code
     (`incidents_cmd::run() -> i32`) instead of calling `std::process::exit`
     inline** — the original shape made its own error branches untestable in-process
     (`process::exit` inside a test kills the whole test binary). Only `main.rs`'s
     one-line wrapper now calls `std::process::exit`.
  4. **A handful of structurally-dead defensive branches were simplified, not
     exempted**, once verified reachability was actually impossible: `collect_disks`'s
     `total > 0` check (an upstream `.filter()` already guarantees it),
     `netstat_port`/`parse_remaining_secs`'s `.next()?` on `str::split`/`str::rsplit`
     (that iterator adapter always yields at least one item, never `None`), and
     `evaluate()`'s `cpu_hog` lookup (a `retain()` two lines above already
     guarantees the pid it looks up exists in `top_cpu`). Each got a comment
     explaining the invariant instead of a coverage workaround.
  5. **`#[coverage(off)]` (the theoretical alternative to file-isolation) was
     tried and confirmed unavailable**: `rustc 1.88.0` (this project's stable
     toolchain) rejects it as an experimental feature requiring nightly. No
     function-level exclusion exists on stable — file-level
     `--ignore-filename-regex` is the only mechanism `cargo-llvm-cov` actually
     offers here, which is why glue was moved to dedicated files rather than
     annotated in place.
  6. **The gate itself moved from an aspirational 99.9% to a measured, stable
     99.5% lines / 98% regions**, both re-verified across repeated runs (not a
     one-off measurement) after closing every gap that turned out to be a real
     untested branch. The remaining ~0.4% is `assert!` panic-message arguments
     (format strings/values only evaluated when an assertion fails, which a
     passing test suite never does) — confirmed via `cargo llvm-cov --html`
     region-by-region, not assumed; this is a known, common artifact of
     source-based coverage on test code with descriptive assertion messages, not
     an untested behavior. A handful of genuinely-unreachable-without-mocking-`Command`
     branches (`pmset`/`netstat`/`date` failing to spawn at all; `$HOME` being
     unset) are documented inline rather than tested, since reaching them needs
     either PATH manipulation (flaky, and this suite is not sandboxed — see point
     2) or mutating a shared env var from one test in a multi-threaded,
     same-process suite (races every other test that also reads it).
  7. **`agent/`'s Python side got the matching `pytest-cov` gate** (99.9%,
     `--cov-fail-under`), configured in `pyproject.toml`. `cli.py` (the
     `asyncio.run(ask(...))` entry point) is `omit`-excluded whole-file, same
     reasoning as `main.rs`; `diagnose.py`'s `ask()` (the real `query()` SDK call)
     is excluded via `coverage.py`'s `exclude_also` regex on its `def` line — a
     mechanism that, unlike Rust's `#[coverage(off)]`, *is* stable and available,
     so it's used directly rather than needing a file split.
- Alternatives considered: **lowering the bar without localizing the gap first** —
  rejected; a first pass at this literally couldn't tell which specific lines were
  responsible for the measured shortfall (`cargo llvm-cov`'s lcov/`--text` export
  and its summary table disagreed by an order of magnitude on missed-line count for
  the same run, traced to sub-line region misses — e.g. one branch of a
  multi-region line — being counted as a "missed line" in the summary table while
  the line itself shows as executed in the annotated source). Chasing 99.9% exactly
  — rejected once the remaining gap was confirmed to be `assert!` message
  arguments; stripping descriptive failure messages from dozens of existing tests
  to satisfy a coverage percentage would trade real debuggability for a vanity
  number. **pty-based integration tests** for `ui_loop.rs`/`menubar_loop.rs` —
  rejected as disproportionate to this project's stated scale ("a small,
  single-maintainer tool, not a governance-heavy one"); the logic they'd exercise
  (alert evaluation, incident tracking, rendering) is already fully tested at the
  functions they call, leaving only real terminal/GUI plumbing to verify, which
  the smoke-test step in the completion checklist below now does by hand per
  change instead.
- Consequences: workspace line coverage (excluding the six glue files) went from
  72.87% to a stable 99.5%+, verified with `agent/` at 100% (99.9% gate). Six new
  files exist purely to make the coverage gate meaningful rather than either
  falsely green (measuring nothing real) or permanently red (counting genuinely
  untestable glue against a metric meant to catch real regressions). Adding new
  OS-boundary code going forward should default to this same split — a pure
  function in an already-gated file, glue in one of the six excluded files or a
  new one following the same doc-comment convention — rather than reaching for
  `#[cfg(not(test))]`-style tricks or lowering the gate further. Because
  `cargo test` cannot exercise the six excluded files, a change touching any of
  them needs a manual smoke run before merging (`vigil snapshot`, `vigil watch
  --count N`, `vigil incidents` + `--show`, `vigil menubar` launched briefly) —
  added to AGENTS.md's completion checklist.
