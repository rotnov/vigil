# Investigate/Fix Desktop UI Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Build `vigil-ui`, a Tauri companion app that turns a journal-worthy
alert notification (or a click on the existing `vigil menubar` tray icon)
into a window showing the agent's diagnosis, a live process tree scoped to
the incident, and a proposed-fix approve/reject card — a GUI client of the
already-shipped `vigil investigate`/`vigil fix` CLI, not a reimplementation
of it.

**Architecture:** Two processes. `vigil menubar` (existing Rust binary)
keeps its tray icon and dropdown, only its click handler changes — it hands
off to `vigil-ui` via a `vigil://incident/<path>` URL instead of opening raw
markdown. `vigil-ui` (new Tauri app, background-resident, no Dock icon, no
tray icon of its own) owns: polling `~/.vigil/incidents/` and posting its
own clickable macOS notification for new journal-worthy stubs, receiving
the `vigil://` handoff from the menubar, and the investigate/fix window
itself — which shells out to `vigil investigate`/`vigil fix`/`vigil
incidents --show --json` as subprocesses and independently queries live
process state via `sysinfo` for the tree.

**Tech Stack:** Rust (existing crate, `clap`/`sysinfo`/`serde` already
dependencies) for the CLI-side changes; Tauri v2 (Rust backend + vanilla
HTML/CSS/JS frontend, reusing the approved mockup) for `vigil-ui`, a new
project at `ui/`; `tauri-plugin-single-instance` and `tauri-plugin-deep-link`
for the `vigil://` URL scheme; `tauri-plugin-notification` for the
clickable notification.

## Global Constraints

- Design spec: `docs/superpowers/specs/2026-08-12-investigate-ui-design.md`
  — every requirement in it must map to a task below.
- Approved visual reference: the mockup at
  `/private/tmp/claude-501/-Users-denis-projects/fe6ff656-3d80-4322-8a64-689d0aadc0a3/scratchpad/vigil-investigate-mockup.html`
  — its exact CSS custom properties, class names, and layout are the visual
  contract for the frontend task. Do not redesign it.
- `vigil-ui` never reimplements `vigil investigate`/`vigil fix`'s logic — it
  only shells out to the real binaries and parses their output. Do not add
  any diagnosis or execution logic to the Tauri project.
- Rust CLI-side changes (`src/`) follow this project's existing conventions:
  pure logic separated from OS-boundary glue, `cargo test`, the coverage
  gate (`cargo llvm-cov --workspace --ignore-filename-regex
  'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs'
  --fail-under-lines 99.5 --fail-under-regions 98`) must keep passing after
  every `src/` task.
- All repo content (code, comments, identifiers, commit messages, docs) is
  English, regardless of what language you're instructed in.
- One PR per task, per this project's existing Git workflow (branch →
  commit → push → `gh pr create` → `gh pr merge --squash --delete-branch`
  once tests are green locally — no CI configured).
- `vigil-ui` never re-implements safety rails — approve/reject decisions are
  collected in the UI, but the actual gating (per-step `y`/`N`, the hard
  floor, category scoping) lives entirely in the already-shipped
  `fix_process.rs`/`execute.py`, invoked as a real subprocess.

---

## Task 1: `incidents.rs` — extract alert key, diagnosis text, fix-execution text

**Files:**
- Modify: `src/incidents.rs`

**Interfaces:**
- Produces: `pub fn extract_alert_key(content: &str) -> Option<&str>`, `pub fn
  extract_diagnosis(content: &str) -> Option<&str>`, `pub fn
  extract_fix_execution(content: &str) -> Option<&str>`. Consumed by Task 2.
- Consumes: nothing new.

- [ ] **Step 1: Add the three functions and their tests to `src/incidents.rs`**

Insert after `extract_command`'s closing brace, before `fn slugify`:

```rust
/// The text after `**Alert key:**` on its own line, with its surrounding
/// backticks stripped (the field is written as `` `{key}` ``, see
/// `write_stub`). `None` if the line is missing or malformed.
pub fn extract_alert_key(content: &str) -> Option<&str> {
    content
        .lines()
        .find_map(|l| l.trim().strip_prefix("**Alert key:**"))
        .map(str::trim)
        .and_then(|s| s.strip_prefix('`')?.strip_suffix('`'))
        .filter(|s| !s.is_empty())
}

/// The agent's diagnosis text — everything between the `## Agent
/// diagnosis` heading `append_diagnosis` wrote and whichever comes first of
/// the `## Proposed fix`/`## Fix execution` headings that may follow it (or
/// end of file, if neither does). `## Proposed fix` is deliberately treated
/// as a boundary here even though it's nested *inside* what the agent
/// wrote, not a heading vigil itself added — the JSON plan under it is
/// parsed separately by `fixplan::extract_proposed_fix_json`, so excluding
/// it from the plain-text diagnosis avoids duplicating that JSON block
/// inside a prose field a UI would otherwise render as plain text.
pub fn extract_diagnosis(content: &str) -> Option<&str> {
    let start = content.find("## Agent diagnosis")? + "## Agent diagnosis".len();
    let rest = &content[start..];
    let end = ["## Proposed fix", "## Fix execution"]
        .iter()
        .filter_map(|h| rest.find(h))
        .min()
        .unwrap_or(rest.len());
    let text = rest[..end].trim();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}

/// The execute-agent's report — everything after the `## Fix execution`
/// heading `append_fix_execution` wrote, to end of file (it's always the
/// last section any of `write_stub`/`append_diagnosis`/
/// `append_fix_execution` ever add, so there's no later heading to stop
/// at).
pub fn extract_fix_execution(content: &str) -> Option<&str> {
    let start = content.find("## Fix execution")? + "## Fix execution".len();
    let text = content[start..].trim();
    if text.is_empty() {
        None
    } else {
        Some(text)
    }
}
```

Add these tests inside the existing `#[cfg(test)] mod tests` block, near
the other `extract_*` tests:

```rust
    #[test]
    fn extract_alert_key_strips_backticks() {
        let content = "# t\n\n**Alert key:** `cpu_hog:1234`\n\n**Rule message:** m\n";
        assert_eq!(extract_alert_key(content), Some("cpu_hog:1234"));
    }

    #[test]
    fn extract_alert_key_is_none_when_the_field_is_absent() {
        assert_eq!(extract_alert_key("# t\n\n**Rule message:** m\n"), None);
    }

    #[test]
    fn extract_diagnosis_reads_up_to_end_of_file_when_there_is_no_proposed_fix() {
        let content = "# t\n\n**Rule message:** m\n\n## Agent diagnosis\n\nThe culprit is pycharm.\n";
        assert_eq!(extract_diagnosis(content), Some("The culprit is pycharm."));
    }

    #[test]
    fn extract_diagnosis_stops_before_a_nested_proposed_fix_heading() {
        let content = "# t\n\n## Agent diagnosis\n\n## Diagnosis\n\ntext\n\n## Proposed fix\n\n```json\n{}\n```\n";
        let diagnosis = extract_diagnosis(content).unwrap();
        assert!(diagnosis.contains("## Diagnosis"));
        assert!(diagnosis.contains("text"));
        assert!(!diagnosis.contains("## Proposed fix"));
    }

    #[test]
    fn extract_diagnosis_stops_before_a_later_fix_execution_heading() {
        let content = "# t\n\n## Agent diagnosis\n\ntext\n\n## Fix execution\n\n1. done\n";
        assert_eq!(extract_diagnosis(content), Some("text"));
    }

    #[test]
    fn extract_diagnosis_is_none_when_the_heading_is_absent() {
        assert_eq!(extract_diagnosis("# t\n\nno diagnosis here\n"), None);
    }

    #[test]
    fn extract_fix_execution_reads_to_end_of_file() {
        let content = "# t\n\n## Fix execution\n\n_Approved: 2026-08-09 02:30 (steps 1 of 1)_\n\n1. done\n";
        let report = extract_fix_execution(content).unwrap();
        assert!(report.contains("1. done"));
        assert!(report.starts_with("_Approved:"));
    }

    #[test]
    fn extract_fix_execution_is_none_when_the_heading_is_absent() {
        assert_eq!(extract_fix_execution("# t\n\nno fix execution here\n"), None);
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test incidents::`
Expected: every test in `incidents::tests` PASSES, including the 7 new
ones.

- [ ] **Step 3: Commit**

```bash
git checkout -b investigate-ui-incidents-extractors
git add src/incidents.rs
git commit -m "Add incidents::extract_alert_key/extract_diagnosis/extract_fix_execution"
git push -u origin investigate-ui-incidents-extractors
gh pr create --title "Add incidents.rs extractors for the --json flag" --body "Part of the investigate/fix UI plan (docs/superpowers/plans/2026-08-12-investigate-ui.md, Task 1). Pure additions, nothing calls them yet — Task 2."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 2: `incidents_cmd.rs` — `IncidentJson` and the `--json` flag's logic

**Files:**
- Modify: `src/incidents_cmd.rs`

**Interfaces:**
- Produces: `pub struct IncidentJson<'a> { pub title: &'a str, pub
  alert_key: Option<&'a str>, pub rule_message: Option<&'a str>, pub
  command: Option<&'a str>, pub diagnosis: Option<&'a str>, pub
  proposed_fix: Option<crate::fixplan::Plan>, pub fix_execution: Option<&'a
  str> }` (serde `Serialize`); `pub fn build_incident_json(content: &str) ->
  IncidentJson<'_>`. `pub fn run(dir: &str, show: Option<&str>, limit:
  usize, json: bool) -> i32` (signature changes — one new `bool` parameter
  appended). Consumed by Task 3 (CLI wiring) and, indirectly, by the Tauri
  backend in Task 8 (which parses this exact JSON shape).
- Consumes: `crate::incidents::{extract_title, extract_alert_key,
  extract_rule_message, extract_command, extract_diagnosis,
  extract_fix_execution}` (Task 1 + pre-existing), `crate::fixplan::{Plan,
  extract_proposed_fix_json, parse_plan}` (pre-existing).

- [ ] **Step 1: Add `IncidentJson` and `build_incident_json` to `src/incidents_cmd.rs`**

Insert after the module doc comment, before `pub fn run`:

```rust
/// The same fields `vigil incidents --show <file>` prints as raw markdown,
/// pulled apart into one structured value instead — for `--json`, so a
/// caller (namely `vigil-ui`'s Tauri backend) doesn't have to re-parse
/// markdown by hand. `proposed_fix` embeds `fixplan::Plan` directly (it
/// already derives `Serialize`) rather than re-encoding its JSON as a
/// string.
#[derive(serde::Serialize)]
pub struct IncidentJson<'a> {
    pub title: &'a str,
    pub alert_key: Option<&'a str>,
    pub rule_message: Option<&'a str>,
    pub command: Option<&'a str>,
    pub diagnosis: Option<&'a str>,
    pub proposed_fix: Option<crate::fixplan::Plan>,
    pub fix_execution: Option<&'a str>,
}

/// Pure — composes `incidents::extract_*`/`fixplan::extract_proposed_fix_json`
/// into one value. A `## Proposed fix` block that fails to parse (malformed
/// JSON, an unknown category) is treated the same as "no proposed fix" here
/// — `--json`'s job is to hand back what's actually usable, not to surface
/// a parse error for a field nothing requires.
pub fn build_incident_json(content: &str) -> IncidentJson<'_> {
    IncidentJson {
        title: crate::incidents::extract_title(content),
        alert_key: crate::incidents::extract_alert_key(content),
        rule_message: crate::incidents::extract_rule_message(content),
        command: crate::incidents::extract_command(content),
        diagnosis: crate::incidents::extract_diagnosis(content),
        proposed_fix: crate::fixplan::extract_proposed_fix_json(content)
            .and_then(|j| crate::fixplan::parse_plan(j).ok()),
        fix_execution: crate::incidents::extract_fix_execution(content),
    }
}
```

- [ ] **Step 2: Thread a `json: bool` parameter through `run`**

Read the current `pub fn run(dir: &str, show: Option<&str>, limit: usize)
-> i32` first. Change its signature to `pub fn run(dir: &str, show:
Option<&str>, limit: usize, json: bool) -> i32`. In the `[single] =>`
match arm (the one that currently does `match std::fs::read_to_string(single) {
Ok(content) => { print!("{content}"); 0 } ...}`), change the `Ok(content)`
branch to:

```rust
                Ok(content) => {
                    if json {
                        match serde_json::to_string(&build_incident_json(&content)) {
                            Ok(j) => {
                                println!("{j}");
                                0
                            }
                            Err(e) => {
                                eprintln!("[vigil] failed to serialize {}: {e}", single.display());
                                1
                            }
                        }
                    } else {
                        print!("{content}");
                        0
                    }
                }
```

`json` only affects the `--show` + single-match path — listing (no
`--show`) and the ambiguous/no-match branches are completely unaffected;
`--json` without `--show` is simply ignored (not an error), matching this
project's general "extra unused flags degrade gracefully" style elsewhere
(e.g. `--json` has nothing to do when there's no `content` to serialize).

- [ ] **Step 3: Update every existing test call site and add new tests**

Every existing call to `run(...)` in this file's test module currently
passes 3 arguments — add a fourth, `false`, to each (`cargo build` will
list exactly which ones if any are missed). Then add:

```rust
    #[test]
    fn show_with_json_prints_structured_json_instead_of_raw_markdown() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("2026-08-09-01-00-00-cpu-hog-1.md"),
            "# vigil: cpu hog\n\n**Alert key:** `cpu_hog:1`\n\n**Rule message:** m\n\n## Agent diagnosis\n\ntext\n",
        )
        .unwrap();
        assert_eq!(run(dir.to_str().unwrap(), Some("cpu-hog-1"), 20, true), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_incident_json_composes_every_field() {
        let content = "# vigil: cpu hog\n\n**Alert key:** `cpu_hog:1`\n\n**Rule message:** m\n\n**Command:** /usr/bin/foo\n\n## Agent diagnosis\n\ndiag text\n\n## Proposed fix\n\n```json\n{\"plan\": [{\"category\": \"kill_process\", \"description\": \"d\", \"target_hint\": \"h\"}]}\n```\n\n## Fix execution\n\n_Approved: 2026-08-09 02:30 (steps 1 of 1)_\n\n1. done\n";
        let json = build_incident_json(content);
        assert_eq!(json.title, "vigil: cpu hog");
        assert_eq!(json.alert_key, Some("cpu_hog:1"));
        assert_eq!(json.rule_message, Some("m"));
        assert_eq!(json.command, Some("/usr/bin/foo"));
        assert_eq!(json.diagnosis, Some("diag text"));
        assert!(json.proposed_fix.is_some());
        assert_eq!(json.proposed_fix.unwrap().plan.len(), 1);
        assert!(json.fix_execution.unwrap().starts_with("_Approved:"));
    }

    #[test]
    fn build_incident_json_treats_a_malformed_proposed_fix_as_absent() {
        let content = "# t\n\n## Agent diagnosis\n\ntext\n\n## Proposed fix\n\n```json\nnot valid json\n```\n";
        let json = build_incident_json(content);
        assert!(json.proposed_fix.is_none());
    }
```

- [ ] **Step 4: Run the tests**

Run: `cargo test incidents_cmd::`
Expected: all tests PASS, including the 3 new ones and every pre-existing
one with its now-4-argument `run(...)` call.

- [ ] **Step 5: Commit**

```bash
git checkout -b investigate-ui-incidents-json
git add src/incidents_cmd.rs
git commit -m "Add IncidentJson + build_incident_json, thread a json flag through incidents_cmd::run"
git push -u origin investigate-ui-incidents-json
gh pr create --title "Add IncidentJson to incidents_cmd.rs" --body "Part of the investigate/fix UI plan (Task 2). run()'s new json parameter isn't wired to the CLI yet — Task 3."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 3: Wire `--json` into `Commands::Incidents`

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Produces: `Commands::Incidents { dir: String, show: Option<String>,
  limit: usize, json: bool }` (one new field).
- Consumes: `incidents_cmd::run` (Task 2's new 4-argument signature).

- [ ] **Step 1: Add the `--json` flag to `Commands::Incidents` in `src/cli.rs`**

In the `Incidents { ... }` variant, add, after `limit`:

```rust
        /// Print `--show`'s match as structured JSON instead of raw markdown
        #[arg(long, default_value_t = false)]
        json: bool,
```

- [ ] **Step 2: Update the dispatch call in `src/main.rs`**

Find `Commands::Incidents { dir, show, limit } => { std::process::exit(incidents_cmd::run(&dir, show.as_deref(), limit)); }`
and change it to:

```rust
        Commands::Incidents { dir, show, limit, json } => {
            std::process::exit(incidents_cmd::run(&dir, show.as_deref(), limit, json));
        }
```

- [ ] **Step 3: Add a CLI-parsing test**

Add to `src/cli.rs`'s existing `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn incidents_json_flag_defaults_to_false_and_can_be_set() {
        let cli = Cli::try_parse_from(["vigil", "incidents"]).unwrap();
        assert!(matches!(&cli.command, Commands::Incidents { json: false, .. }));

        let cli = Cli::try_parse_from(["vigil", "incidents", "--json"]).unwrap();
        assert!(matches!(&cli.command, Commands::Incidents { json: true, .. }));
    }
```

- [ ] **Step 4: Run the tests and confirm the crate builds**

Run: `cargo test cli::`
Expected: all tests PASS, including the new one.

Run: `cargo build`
Expected: builds cleanly.

- [ ] **Step 5: Commit**

```bash
git checkout -b investigate-ui-json-cli-wiring
git add src/cli.rs src/main.rs
git commit -m "Wire --json into vigil incidents --show"
git push -u origin investigate-ui-json-cli-wiring
gh pr create --title "Wire --json into vigil incidents --show" --body "Part of the investigate/fix UI plan (Task 3). vigil incidents --show <file> --json is now runnable end to end."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

Manual smoke check (this touches no excluded file, but confirming the real
CLI output shape end to end is worth 30 seconds before moving on):

```bash
cargo build --release
mkdir -p /tmp/vigil-json-smoke
cat > /tmp/vigil-json-smoke/2026-08-12-00-00-00-cpu-hog-1.md <<'EOF'
# vigil: cpu hog

**Alert key:** `cpu_hog:1`

**Rule message:** test
EOF
./target/release/vigil incidents --dir /tmp/vigil-json-smoke --show cpu-hog-1 --json
rm -rf /tmp/vigil-json-smoke
```
Expected output: a single line of JSON with `"title"`, `"alert_key":
"cpu_hog:1"`, `"rule_message": "test"`, and `"diagnosis": null` (no
diagnosis was ever appended to this hand-written stub).

---

## Task 4: Stop `watch.rs`/`ui_loop.rs` notifying journal-worthy alerts

**Files:**
- Modify: `src/watch.rs`
- Modify: `src/ui_loop.rs`
- Modify: `src/agent.rs` (delete `augment_with_investigate_hint` + its 2
  tests — becomes fully dead once its only two callers, below, are
  removed)

**Interfaces:**
- Removes: `agent::augment_with_investigate_hint`. Nothing later depends
  on it.
- Consumes: nothing new.

Per the design's decision (confirmed with the user during brainstorming):
`vigil-ui` now owns notifying for journal-worthy alerts entirely.
`vigil watch`/`vigil ui` still write the stub file — that part is
unchanged — they just stop also firing their own notification for it.

- [ ] **Step 1: Update `src/watch.rs`'s journal-worthy branch**

Replace:

```rust
                    if crate::agent::is_journal_worthy(&alert.key) {
                        let incidents_dir = std::path::Path::new(&args.incidents_dir);
                        let stub = crate::incidents::IncidentStub {
                            alert_key: &alert.key,
                            alert_title: &alert.title,
                            alert_message: &alert.message,
                            command: alert.command.as_deref(),
                        };
                        match crate::incidents::write_stub(incidents_dir, &stub) {
                            Ok(_) => {
                                crate::alerts::notify(&crate::agent::augment_with_investigate_hint(&alert, watch_log_path.as_deref()));
                            }
                            Err(e) => {
                                eprintln!("[vigil] failed to write incident stub: {e}");
                                crate::alerts::notify(&alert);
                            }
                        }
                    } else {
                        crate::alerts::notify(&alert);
                    }
```

with:

```rust
                    if crate::agent::is_journal_worthy(&alert.key) {
                        let incidents_dir = std::path::Path::new(&args.incidents_dir);
                        let stub = crate::incidents::IncidentStub {
                            alert_key: &alert.key,
                            alert_title: &alert.title,
                            alert_message: &alert.message,
                            command: alert.command.as_deref(),
                        };
                        // No notification here on success: `vigil-ui` polls
                        // this directory and owns notifying for
                        // journal-worthy alerts, with a real clickable
                        // notification watch.rs can't produce on its own
                        // (see docs/superpowers/specs/2026-08-12-investigate-ui-design.md).
                        // On a write failure there's no stub for vigil-ui to
                        // find, so fall back to a plain notification rather
                        // than going silent.
                        if let Err(e) = crate::incidents::write_stub(incidents_dir, &stub) {
                            eprintln!("[vigil] failed to write incident stub: {e}");
                            crate::alerts::notify(&alert);
                        }
                    } else {
                        crate::alerts::notify(&alert);
                    }
```

Then remove the now-dead `watch_log_path` computation — find and delete
this line near the top of `run`:

```rust
    let watch_log_path = std::fs::canonicalize(&args.out).ok().map(|p| p.to_string_lossy().to_string());
```

(Confirm nothing else in the file references `watch_log_path` before
deleting — `grep -n watch_log_path src/watch.rs` should show nothing after
this edit.)

- [ ] **Step 2: Update `src/ui_loop.rs`'s journal-worthy branch**

Replace:

```rust
                        if crate::agent::is_journal_worthy(&alert.key) {
                            let incidents_dir = std::path::Path::new(&opts.incidents_dir);
                            let stub = crate::incidents::IncidentStub {
                                alert_key: &alert.key,
                                alert_title: &alert.title,
                                alert_message: &alert.message,
                                command: alert.command.as_deref(),
                            };
                            match crate::incidents::write_stub(incidents_dir, &stub) {
                                // vigil ui's own snapshot loop doesn't write a persistent JSONL log
                                Ok(_) => crate::alerts::notify(&crate::agent::augment_with_investigate_hint(&alert, None)),
                                Err(e) => {
                                    app.push_alert(format!("[vigil] failed to write incident stub: {e}"));
                                    crate::alerts::notify(&alert);
                                }
                            }
                        } else {
                            crate::alerts::notify(&alert);
                        }
```

with:

```rust
                        if crate::agent::is_journal_worthy(&alert.key) {
                            let incidents_dir = std::path::Path::new(&opts.incidents_dir);
                            let stub = crate::incidents::IncidentStub {
                                alert_key: &alert.key,
                                alert_title: &alert.title,
                                alert_message: &alert.message,
                                command: alert.command.as_deref(),
                            };
                            // Same reasoning as watch.rs: vigil-ui owns
                            // notifying journal-worthy alerts now.
                            if let Err(e) = crate::incidents::write_stub(incidents_dir, &stub) {
                                app.push_alert(format!("[vigil] failed to write incident stub: {e}"));
                                crate::alerts::notify(&alert);
                            }
                        } else {
                            crate::alerts::notify(&alert);
                        }
```

- [ ] **Step 3: Delete `agent::augment_with_investigate_hint` from `src/agent.rs`**

Delete the function itself (its doc comment through its closing brace):

```rust
pub(crate) fn augment_with_investigate_hint(
    alert: &crate::alerts::Alert,
    watch_log_path: Option<&str>,
) -> crate::alerts::Alert {
    ...
}
```

Delete its two tests:
`augment_with_investigate_hint_appends_the_command_and_keeps_other_fields`
and `augment_with_investigate_hint_omits_watch_log_flag_when_none`.

- [ ] **Step 4: Confirm no dangling references and run the tests**

Run: `grep -rn "augment_with_investigate_hint\|watch_log_path" src/*.rs`
Expected: no output at all (both are now fully gone from the codebase —
`build_diagnosis_question`'s own, separate `watch_log_path` parameter lives
in `agent.rs`/`investigate_process.rs` and is untouched by this task, so if
grep still shows hits there, re-check you only deleted what Step 3 asked
for).

Run: `cargo build 2>&1 | grep -i warning`
Expected: no output.

Run: `cargo test`
Expected: all tests PASS (2 fewer than before — the deleted
`augment_with_investigate_hint` tests are gone, not failing).

Run the coverage gate:
```bash
cargo llvm-cov --workspace --ignore-filename-regex 'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs' --fail-under-lines 99.5 --fail-under-regions 98
```
Expected: PASSES (exit 0).

- [ ] **Step 5: Commit**

```bash
git checkout -b investigate-ui-drop-watch-notify
git add src/watch.rs src/ui_loop.rs src/agent.rs
git commit -m "Stop watch.rs/ui_loop.rs notifying journal-worthy alerts — vigil-ui owns that now"
git push -u origin investigate-ui-drop-watch-notify
gh pr create --title "Hand journal-worthy notifications to vigil-ui" --body "Part of the investigate/fix UI plan (Task 4). Journal-worthy alerts still write a stub exactly as before; vigil watch/vigil ui just stop also firing a plain notification for them, since vigil-ui (Task 9) will own a real clickable one. Deletes agent::augment_with_investigate_hint, now fully unused."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

After merging, restart any running `vigil watch`/`vigil ui`/`vigil menubar`
background processes from the merged `master` — this changes their runtime
behavior (journal-worthy alerts go quiet from vigil's own binaries until
`vigil-ui` exists, later in this plan).

---

## Task 5: `menubar_loop.rs` — hand off dropdown clicks to `vigil-ui`

**Files:**
- Modify: `src/menubar_loop.rs`
- Modify: `Cargo.toml` (add `urlencoding = "2"`)

**Interfaces:**
- Consumes: nothing new from within `src/`.
- Produces: the `vigil://incident/<url-encoded-path>` URL shape Task 9's
  Tauri deep-link handler must parse — keep this exact scheme/host/path
  shape in mind when writing that task.

This file is in the coverage `--ignore-filename-regex` exclusion list (real
`NSStatusItem`/`tao` events) — no unit tests here, matching its existing
convention.

- [ ] **Step 1: Add the dependency**

In `Cargo.toml`'s `[dependencies]`, add (alphabetized among the existing
list):

```toml
urlencoding = "2"
```

- [ ] **Step 2: Change the dropdown click handler**

Find, in `src/menubar_loop.rs`'s `run` function:

```rust
            Event::UserEvent(UserEvent::Menu(menu_event)) => {
                if menu_event.id == "quit" {
                    *control_flow = ControlFlow::Exit;
                } else if let Some(path) = incident_paths.iter().find(|p| p.to_string_lossy() == menu_event.id.0) {
                    let _ = std::process::Command::new("open").arg(path).spawn();
                }
            }
```

Replace the `else if` body with:

```rust
            Event::UserEvent(UserEvent::Menu(menu_event)) => {
                if menu_event.id == "quit" {
                    *control_flow = ControlFlow::Exit;
                } else if let Some(path) = incident_paths.iter().find(|p| p.to_string_lossy() == menu_event.id.0) {
                    // Hand off to vigil-ui instead of opening raw markdown —
                    // see docs/superpowers/specs/2026-08-12-investigate-ui-design.md.
                    // macOS routes this URL to vigil-ui via the `vigil://`
                    // scheme it registers (tauri-plugin-deep-link, Task 9).
                    let url = format!("vigil://incident/{}", urlencoding::encode(&path.to_string_lossy()));
                    let _ = std::process::Command::new("open").arg(url).spawn();
                }
            }
```

- [ ] **Step 3: Confirm the crate builds**

Run: `cargo build 2>&1 | grep -i warning`
Expected: no output.

Run: `cargo test`
Expected: all tests still pass (this file has none of its own, but confirm
nothing else broke).

- [ ] **Step 4: Commit**

```bash
git checkout -b investigate-ui-menubar-handoff
git add src/menubar_loop.rs Cargo.toml Cargo.lock
git commit -m "Hand off vigil menubar's dropdown clicks to vigil-ui via a vigil:// URL"
git push -u origin investigate-ui-menubar-handoff
gh pr create --title "menubar_loop.rs: hand off to vigil-ui" --body "Part of the investigate/fix UI plan (Task 5). Nothing handles vigil:// yet until vigil-ui registers the scheme (Task 9) — until then, clicking a dropdown item will just fail silently (\`open\` finds no handler), same practical effect as before vigil-ui exists."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

Manual smoke check (this touches an excluded file): `cargo build --release
&& ./target/release/vigil menubar`, let it run a few seconds, click the
tray icon, confirm the dropdown still opens and lists incidents (clicking
an item will silently no-op until Task 9 registers the `vigil://` handler
— that's expected at this point in the plan). Kill the process when done
(`pkill -f "vigil menubar"`).

---

## Task 6: Scaffold the `vigil-ui` Tauri project

**Files:**
- Create: `ui/` (a new Tauri v2 project — `ui/src-tauri/Cargo.toml`,
  `ui/src-tauri/src/main.rs`, `ui/src-tauri/tauri.conf.json`, `ui/src/`
  frontend directory, `ui/package.json`, plus whatever else the scaffold
  tool generates)

**Interfaces:**
- Produces: a running (empty) Tauri app — the foundation every later task
  in this plan adds to.
- Consumes: nothing.

No automated tests for a scaffold step — verified by actually launching the
generated app.

- [ ] **Step 1: Run the Tauri scaffold tool from the repo root**

```bash
npm create tauri-app@latest ui
```

Answer its prompts (or pass non-interactive flags if the installed version
of the tool supports them — flags have changed across Tauri CLI versions,
so prefer answering the prompts interactively if unsure):
- App name: `vigil-ui`
- Window title: `vigil`
- Frontend language: **TypeScript / JavaScript** (not Rust — this needs to
  render arbitrary HTML/CSS matching the mockup, a JS frontend is the
  natural fit)
- Package manager: **npm**
- UI template: **Vanilla** (no framework — the mockup is already plain
  HTML/CSS/JS, a framework would only add translation overhead for no
  benefit at this scale)

This creates `ui/` as a sibling of `src/` and `agent/` at the repo root.

- [ ] **Step 2: Verify the scaffold actually runs**

```bash
cd ui
npm install
npm run tauri dev
```

Expected: a native window opens showing the Tauri template's default
placeholder content. Close the window (or Ctrl-C the dev server) once
confirmed.

- [ ] **Step 3: Set the app to run as a background accessory (no Dock icon)**

Per the design, `vigil-ui` is `LSUIElement`-style background-resident, not
a normal Dock app. Open `ui/src-tauri/tauri.conf.json` and, under the
top-level `"bundle"` key's `"macOS"` section (create the section if the
scaffold didn't generate one), add:

```json
"macOS": {
  "minimumSystemVersion": "10.13"
}
```

and in `ui/src-tauri/Info.plist` (create it if the scaffold didn't — check
`ui/src-tauri/tauri.conf.json`'s `"bundle"` section for whether it already
references one), add:

```xml
<key>LSUIElement</key>
<true/>
```

If `ui/src-tauri/Info.plist` doesn't exist yet, create it with:

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>LSUIElement</key>
    <true/>
</dict>
</plist>
```

- [ ] **Step 4: Verify the accessory setting took effect**

```bash
npm run tauri dev
```

Expected: the app window still opens, but no icon appears in the Dock
while it's running (check the Dock directly). Close/Ctrl-C when confirmed.

- [ ] **Step 5: Commit**

```bash
cd /Users/denis/projects/vigil
git checkout -b investigate-ui-scaffold
git add ui/
git commit -m "Scaffold the vigil-ui Tauri project"
git push -u origin investigate-ui-scaffold
gh pr create --title "Scaffold vigil-ui" --body "Part of the investigate/fix UI plan (Task 6). Empty Tauri app, background-accessory (no Dock icon), verified running. No vigil-specific logic yet — subsequent tasks add it."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 7: `vigil-ui` — `process_tree.rs`, live process data scoped to an alert

**Files:**
- Create: `ui/src-tauri/src/process_tree.rs`
- Modify: `ui/src-tauri/src/main.rs` (add `mod process_tree;`)
- Modify: `ui/src-tauri/Cargo.toml` (add `sysinfo = "0.32"`, matching the
  main `vigil` crate's version)

**Interfaces:**
- Produces: `pub struct ProcessNode { pub pid: u32, pub ppid: Option<u32>,
  pub name: String, pub cpu_pct: f32, pub mem_bytes: u64, pub
  run_time_secs: u64, pub is_zombie: bool }` (serde `Serialize`); `pub enum
  Scope { Pid(u32), Name(String), None }`; `pub fn
  scope_for_alert_key(alert_key: &str) -> Scope`; `pub fn
  query_process_tree(sys: &mut sysinfo::System, scope: &Scope) ->
  Vec<ProcessNode>`. Consumed by Task 9 (the `#[tauri::command]` wrapper).
- Consumes: nothing from the main `vigil` crate — `ui/` is a fully separate
  Rust project/crate, this is a fresh implementation over `sysinfo`
  directly (the main crate's `snapshot.rs`/`ProcInfo` are not
  cross-crate-importable without publishing them, which is out of scope
  here — some structural duplication with `snapshot.rs::to_proc_info` is
  accepted, see the design spec's consequences section).

- [ ] **Step 1: Add the dependency**

In `ui/src-tauri/Cargo.toml`'s `[dependencies]`:

```toml
sysinfo = "0.32"
```

- [ ] **Step 2: Write `ui/src-tauri/src/process_tree.rs` with its full test module**

```rust
//! Live, on-demand process data scoped to one incident's alert key —
//! deliberately not sourced from the agent's diagnosis prose (which is
//! written for a human to read, not a machine to parse) or from whatever
//! the snapshot looked like when the alert fired (which can be stale by
//! the time this window renders). `sysinfo` is queried fresh every time
//! `query_process_tree` is called.

use serde::Serialize;
use sysinfo::{Pid, ProcessStatus, System};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProcessNode {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub run_time_secs: u64,
    pub is_zombie: bool,
}

/// What to scope a process-tree query to, derived from an alert key. An
/// alert key vigil doesn't currently name a specific process/group for
/// (e.g. `high_load`, `swap_pressure`) has nothing meaningful to scope a
/// tree to — `Scope::None` — and the caller should skip rendering a tree
/// section entirely rather than dumping every process on the machine.
#[derive(Debug, Clone, PartialEq)]
pub enum Scope {
    Pid(u32),
    Name(String),
    None,
}

/// Pure — parses vigil's alert-key conventions (`cpu_hog:<pid>`,
/// `high_process_count:<name>`) without touching the system. Mirrors the
/// same key shapes `agent::is_journal_worthy` on the main `vigil` crate
/// already gates on, kept here as an independent parse since `ui/` doesn't
/// share a crate with `vigil`.
pub fn scope_for_alert_key(alert_key: &str) -> Scope {
    if let Some(pid_str) = alert_key.strip_prefix("cpu_hog:") {
        if let Ok(pid) = pid_str.parse::<u32>() {
            return Scope::Pid(pid);
        }
    }
    if let Some(name) = alert_key.strip_prefix("high_process_count:") {
        if !name.is_empty() {
            return Scope::Name(name.to_string());
        }
    }
    Scope::None
}

/// Refreshes `sys` and returns every currently-running process matching
/// `scope`: for `Scope::Pid`, that one pid plus any process whose parent
/// chain leads back to it (its direct children — this project's incidents
/// have not needed grandchildren-of-children trees so far, and going
/// deeper risks pulling in unrelated system processes that happen to
/// share an ancestor far up the tree); for `Scope::Name`, every process
/// whose name matches exactly; for `Scope::None`, an empty list.
pub fn query_process_tree(sys: &mut System, scope: &Scope) -> Vec<ProcessNode> {
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    match scope {
        Scope::None => Vec::new(),
        Scope::Name(name) => sys
            .processes()
            .iter()
            .filter(|(_, p)| p.name().to_string_lossy() == *name)
            .map(|(pid, p)| to_node(pid, p))
            .collect(),
        Scope::Pid(target_pid) => {
            let target = Pid::from_u32(*target_pid);
            sys.processes()
                .iter()
                .filter(|(pid, p)| **pid == target || p.parent() == Some(target))
                .map(|(pid, p)| to_node(pid, p))
                .collect()
        }
    }
}

fn to_node(pid: &Pid, p: &sysinfo::Process) -> ProcessNode {
    ProcessNode {
        pid: pid.as_u32(),
        ppid: p.parent().map(|p| p.as_u32()),
        name: p.name().to_string_lossy().to_string(),
        cpu_pct: p.cpu_usage(),
        mem_bytes: p.memory(),
        run_time_secs: p.run_time(),
        is_zombie: p.status() == ProcessStatus::Zombie,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_for_alert_key_parses_cpu_hog() {
        assert_eq!(scope_for_alert_key("cpu_hog:37489"), Scope::Pid(37489));
    }

    #[test]
    fn scope_for_alert_key_parses_high_process_count() {
        assert_eq!(scope_for_alert_key("high_process_count:node"), Scope::Name("node".to_string()));
    }

    #[test]
    fn scope_for_alert_key_falls_back_to_none_for_unrecognized_keys() {
        assert_eq!(scope_for_alert_key("high_load"), Scope::None);
        assert_eq!(scope_for_alert_key("swap_pressure"), Scope::None);
        assert_eq!(scope_for_alert_key("battery_low"), Scope::None);
    }

    #[test]
    fn scope_for_alert_key_falls_back_to_none_for_a_non_numeric_cpu_hog_pid() {
        assert_eq!(scope_for_alert_key("cpu_hog:not-a-number"), Scope::None);
    }

    #[test]
    fn query_process_tree_is_empty_for_scope_none() {
        let mut sys = System::new_all();
        assert_eq!(query_process_tree(&mut sys, &Scope::None), Vec::new());
    }

    #[test]
    fn query_process_tree_by_name_finds_this_test_process_on_this_machine() {
        // Real sysinfo call against the actual running test binary, same
        // convention as the main vigil crate's own snapshot.rs tests (see
        // AGENTS.md's testing section) -- this process is guaranteed to be
        // running while the test runs.
        let mut sys = System::new_all();
        let own_pid = sysinfo::get_current_pid().expect("must be able to read our own pid");
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let own_name = sys.process(own_pid).expect("our own process must be visible to sysinfo").name().to_string_lossy().to_string();

        let nodes = query_process_tree(&mut sys, &Scope::Name(own_name.clone()));
        assert!(nodes.iter().any(|n| n.pid == own_pid.as_u32()), "expected to find our own pid among processes named {own_name:?}");
    }

    #[test]
    fn query_process_tree_by_pid_includes_the_target_pid_itself() {
        let mut sys = System::new_all();
        let own_pid = sysinfo::get_current_pid().expect("must be able to read our own pid");
        let nodes = query_process_tree(&mut sys, &Scope::Pid(own_pid.as_u32()));
        assert!(nodes.iter().any(|n| n.pid == own_pid.as_u32()));
    }

    #[test]
    fn query_process_tree_by_unmatched_name_is_empty() {
        let mut sys = System::new_all();
        let nodes = query_process_tree(&mut sys, &Scope::Name("definitely-not-a-real-process-name-xyz".to_string()));
        assert_eq!(nodes, Vec::new());
    }
}
```

- [ ] **Step 3: Register the module**

In `ui/src-tauri/src/main.rs`, add `mod process_tree;` near the top
(alongside whatever module declarations the scaffold already generated).

- [ ] **Step 4: Run the tests**

Run: `cd ui/src-tauri && cargo test`
Expected: all tests in `process_tree::tests` PASS (8 tests).

- [ ] **Step 5: Commit**

```bash
cd /Users/denis/projects/vigil
git checkout -b investigate-ui-process-tree
git add ui/
git commit -m "Add vigil-ui process_tree.rs: live sysinfo query scoped to an alert key"
git push -u origin investigate-ui-process-tree
gh pr create --title "Add vigil-ui process_tree.rs" --body "Part of the investigate/fix UI plan (Task 7). Standalone, fully tested; not yet wired to a Tauri command — Task 9."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 8: `vigil-ui` — subprocess wrappers around the `vigil` CLI

**Files:**
- Create: `ui/src-tauri/src/vigil_cli.rs`
- Modify: `ui/src-tauri/src/main.rs` (add `mod vigil_cli;`)

**Interfaces:**
- Produces: `pub fn read_incident_json(vigil_bin: &str, incidents_dir: &str,
  path: &str) -> Result<String, String>` (returns the raw JSON string —
  parsing it into a typed struct happens in Task 9's Tauri command, which
  is where the frontend's expected shape actually matters); `pub fn
  build_investigate_args(vigil_bin: &str, alert_key: &str, incidents_dir:
  &str) -> Vec<String>`; `pub fn build_fix_stdin(approvals: &[bool]) ->
  String`. Consumed by Task 9.
- Consumes: nothing new — this file has no dependency on the main `vigil`
  crate or on `process_tree.rs`.

Per this project's established pattern (`agent.rs`/`agent_process.rs` on
the main crate), pure argv/stdin construction is separated from the actual
process spawn so it's unit-testable without spawning anything. The actual
spawning of `vigil investigate`/`vigil fix` (which run a real, costly
Claude Agent SDK session) happens in Task 9's `#[tauri::command]`
functions, not here — this file only builds the pieces those spawns need.

- [ ] **Step 1: Write `ui/src-tauri/src/vigil_cli.rs` with its full test module**

```rust
//! Pure construction of the argv/stdin `vigil-ui`'s Tauri commands (Task 9)
//! use to shell out to the real `vigil` binary — kept separate from the
//! actual `Command::spawn` calls so it's testable without spawning
//! anything, the same split `vigil`'s own `agent.rs`/`agent_process.rs`
//! already use.

/// Argv for `vigil investigate <alert_key> --incidents-dir <incidents_dir>`.
pub fn build_investigate_args(vigil_bin: &str, alert_key: &str, incidents_dir: &str) -> Vec<String> {
    vec![
        vigil_bin.to_string(),
        "investigate".to_string(),
        alert_key.to_string(),
        "--incidents-dir".to_string(),
        incidents_dir.to_string(),
    ]
}

/// Argv for `vigil incidents --dir <incidents_dir> --show <path> --json`.
/// `path` is passed as `--show`'s query — since it's a caller-supplied
/// exact file path (not a user-typed substring), it matches itself
/// exactly, the same way `--show` already resolves any unambiguous
/// substring.
pub fn build_show_json_args(vigil_bin: &str, incidents_dir: &str, path: &str) -> Vec<String> {
    vec![
        vigil_bin.to_string(),
        "incidents".to_string(),
        "--dir".to_string(),
        incidents_dir.to_string(),
        "--show".to_string(),
        path.to_string(),
        "--json".to_string(),
    ]
}

/// Argv for `vigil fix <path>`.
pub fn build_fix_args(vigil_bin: &str, path: &str) -> Vec<String> {
    vec![vigil_bin.to_string(), "fix".to_string(), path.to_string()]
}

/// The full stdin `vigil fix`'s interactive per-step prompt loop expects:
/// one `y\n`/`N\n` line per step, in plan order. Written all at once before
/// the spawned process's stdin is closed — `fix_process.rs`'s prompt loop
/// reads one line per step sequentially, and a handful of short lines
/// comfortably fits the pipe buffer without needing to interleave writes
/// with reads (see the design spec's data-flow section for why this is an
/// accepted simplification for a plan with a small step count).
pub fn build_fix_stdin(approvals: &[bool]) -> String {
    approvals.iter().map(|&a| if a { "y\n" } else { "N\n" }).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_investigate_args_has_the_alert_key_and_incidents_dir() {
        let args = build_investigate_args("/usr/local/bin/vigil", "cpu_hog:1", "/tmp/incidents");
        assert_eq!(args, vec!["/usr/local/bin/vigil", "investigate", "cpu_hog:1", "--incidents-dir", "/tmp/incidents"]);
    }

    #[test]
    fn build_show_json_args_includes_the_json_flag() {
        let args = build_show_json_args("vigil", "/tmp/incidents", "/tmp/incidents/x.md");
        assert_eq!(args, vec!["vigil", "incidents", "--dir", "/tmp/incidents", "--show", "/tmp/incidents/x.md", "--json"]);
    }

    #[test]
    fn build_fix_args_has_the_incident_path() {
        let args = build_fix_args("vigil", "/tmp/incidents/x.md");
        assert_eq!(args, vec!["vigil", "fix", "/tmp/incidents/x.md"]);
    }

    #[test]
    fn build_fix_stdin_writes_one_line_per_approval_in_order() {
        assert_eq!(build_fix_stdin(&[true, false, true]), "y\nN\ny\n");
    }

    #[test]
    fn build_fix_stdin_is_empty_for_no_steps() {
        assert_eq!(build_fix_stdin(&[]), "");
    }

    #[test]
    fn build_fix_stdin_all_rejected() {
        assert_eq!(build_fix_stdin(&[false, false]), "N\nN\n");
    }
}
```

- [ ] **Step 2: Register the module**

In `ui/src-tauri/src/main.rs`, add `mod vigil_cli;`.

- [ ] **Step 3: Run the tests**

Run: `cd ui/src-tauri && cargo test`
Expected: all tests PASS, including the 6 new `vigil_cli::tests`.

- [ ] **Step 4: Commit**

```bash
cd /Users/denis/projects/vigil
git checkout -b investigate-ui-cli-wrappers
git add ui/
git commit -m "Add vigil-ui vigil_cli.rs: pure argv/stdin construction for shelling to vigil"
git push -u origin investigate-ui-cli-wrappers
gh pr create --title "Add vigil-ui vigil_cli.rs" --body "Part of the investigate/fix UI plan (Task 8). Pure construction only; the actual process spawns are Task 9's Tauri commands."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 9: `vigil-ui` — Tauri commands, tray-free deep-link + notification wiring

**Files:**
- Modify: `ui/src-tauri/src/main.rs`
- Modify: `ui/src-tauri/Cargo.toml` (add `tauri-plugin-single-instance`,
  `tauri-plugin-deep-link`, `tauri-plugin-notification`)
- Modify: `ui/src-tauri/tauri.conf.json` (register the `vigil` URL scheme)

**Interfaces:**
- Produces: four `#[tauri::command]` functions the frontend (Task 10)
  calls via `invoke()`: `investigate(alert_key: String, incidents_dir:
  String) -> Result<(), String>` (spawns `vigil investigate`, blocks until
  it exits, returns nothing — the frontend re-fetches via
  `read_incident_json` afterward rather than this command returning the
  content directly, keeping "spawn and wait" separate from "read current
  state"); `read_incident_json(incidents_dir: String, path: String) ->
  Result<serde_json::Value, String>`; `process_tree(alert_key: String) ->
  Vec<crate::process_tree::ProcessNode>`; `run_fix(path: String, approvals:
  Vec<bool>) -> Result<String, String>`.
- Consumes: `crate::process_tree::{Scope, scope_for_alert_key,
  query_process_tree}` (Task 7), `crate::vigil_cli::{build_investigate_args,
  build_show_json_args, build_fix_args, build_fix_stdin}` (Task 8).

This file is genuine OS-boundary glue (real process spawns, real deep-link/
notification OS APIs) — no unit tests, matching the main `vigil` crate's
own convention for files in this category (`agent_process.rs`,
`menubar_loop.rs`). Verified by manual smoke test at the end of this task
and again in Task 13.

**IMPORTANT — verified against Tauri v2's actual current docs before
writing this task (do not re-derive these from first principles):**
`tauri-plugin-notification`'s "Actions API" (custom buttons embedded in a
notification, with a callback naming which button was clicked) is
**mobile-only** — it does not exist on macOS. There is no way to have the
notification plugin itself tell you "the user clicked the notification for
*this* incident." The design's promise ("tap the notification, the window
opens to that incident") is still achievable, just via a different
mechanism than a click-carrying-data callback: **prepare the window's
content for the new incident at the moment the poller detects it — before
posting the notification, not in response to a click — then post a plain
notification with no action.** Clicking a real, properly-posted
`UNUserNotificationCenter` notification activates (foregrounds) the app
that posted it by default OS behavior, independent of any custom action; an
already-content-ready window is what the user then sees. Steps 4-5 below
are written for this corrected design, not the original "click carries the
path" framing in this plan's earlier sections — if you're implementing
from a stale mental model of this task, re-read this note first.

Separately, also verified: **macOS cannot register a custom URL scheme
(`vigil://`) at runtime for an app run via `npm run tauri dev`** — deep
links only resolve for the actual bundled, installed `.app` (Tauri's own
docs: "deep links can only be tested on the bundled application, which
must be installed in the `/Applications` directory"). Step 6's manual test
plan accounts for this — trigger B (the menu-bar handoff) cannot be smoke
tested until Task 13's real build, only trigger A can be tested in dev
mode.

- [ ] **Step 1: Add the three plugin dependencies**

In `ui/src-tauri/Cargo.toml`'s `[dependencies]`:

```toml
tauri-plugin-single-instance = "2"
tauri-plugin-deep-link = "2.0.0"
tauri-plugin-notification = "2"
```

Run `cd ui/src-tauri && cargo build` once after adding these to confirm
they resolve.

- [ ] **Step 2: Register the `vigil` URL scheme**

In `ui/src-tauri/tauri.conf.json`, add:

```json
{
  "plugins": {
    "deep-link": {
      "desktop": {
        "schemes": ["vigil"]
      }
    }
  }
}
```

(merge into the existing top-level object rather than replacing it, and
merge into an existing `"plugins"` key if the scaffold already added one).
This is the config `menubar_loop.rs` (Task 5)'s `open
vigil://incident/<path>` needs to route to this app.

- [ ] **Step 3: Write the four Tauri commands and the incidents-directory poller in `ui/src-tauri/src/main.rs`**

```rust
#[tauri::command]
fn investigate(alert_key: String, incidents_dir: String) -> Result<(), String> {
    let args = crate::vigil_cli::build_investigate_args("vigil", &alert_key, &incidents_dir);
    let output = std::process::Command::new(&args[0])
        .args(&args[1..])
        .output()
        .map_err(|e| format!("failed to launch vigil investigate: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}

#[tauri::command]
fn read_incident_json(incidents_dir: String, path: String) -> Result<serde_json::Value, String> {
    let args = crate::vigil_cli::build_show_json_args("vigil", &incidents_dir, &path);
    let output = std::process::Command::new(&args[0])
        .args(&args[1..])
        .output()
        .map_err(|e| format!("failed to launch vigil incidents: {e}"))?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stderr).trim().to_string());
    }
    serde_json::from_slice(&output.stdout).map_err(|e| format!("failed to parse vigil's JSON output: {e}"))
}

#[tauri::command]
fn process_tree(alert_key: String) -> Vec<crate::process_tree::ProcessNode> {
    let scope = crate::process_tree::scope_for_alert_key(&alert_key);
    let mut sys = sysinfo::System::new_all();
    crate::process_tree::query_process_tree(&mut sys, &scope)
}

#[tauri::command]
fn run_fix(path: String, approvals: Vec<bool>) -> Result<String, String> {
    use std::io::Write;
    use std::process::Stdio;

    let args = crate::vigil_cli::build_fix_args("vigil", &path);
    let mut child = std::process::Command::new(&args[0])
        .args(&args[1..])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| format!("failed to launch vigil fix: {e}"))?;

    let stdin_text = crate::vigil_cli::build_fix_stdin(&approvals);
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(stdin_text.as_bytes());
    }

    let output = child.wait_with_output().map_err(|e| format!("vigil fix did not exit cleanly: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        Err(String::from_utf8_lossy(&output.stderr).trim().to_string())
    }
}
```

Register all four in the `tauri::Builder` chain (find wherever the scaffold
generated `.invoke_handler(tauri::generate_handler![...])` and add these
four names to that list).

- [ ] **Step 4: Wire the deep-link handler (trigger B: menu-bar handoff)**

In `ui/src-tauri/src/main.rs`, add a helper both this step and Step 5 call:

```rust
fn open_incident_window(app: &tauri::AppHandle, path: &str) {
    let url = format!("index.html?path={}", urlencoding::encode(path));
    if let Some(window) = app.get_webview_window("main") {
        let _ = window.eval(&format!("window.location.replace('{url}')"));
        let _ = window.show();
        let _ = window.set_focus();
    }
}
```

(`urlencoding = "2"` — add it to `ui/src-tauri/Cargo.toml`; same crate
`menubar_loop.rs`, Task 5, already uses on the main crate side, but `ui/`
is a separate Rust project so it needs its own copy of the dependency
declaration.)

Register the deep-link plugin and its two callbacks — verified against
`tauri-plugin-deep-link`'s actual current API:

```rust
use tauri_plugin_deep_link::DeepLinkExt;

// Inside the tauri::Builder chain, alongside the other .plugin(...) calls:
.plugin(tauri_plugin_deep_link::init())
.setup(|app| {
    let handle = app.handle().clone();
    // Cold start: the app was launched *by* a `vigil://` URL.
    if let Ok(Some(urls)) = app.deep_link().get_current() {
        if let Some(path) = urls.first().and_then(|u| parse_incident_url(&u.to_string())) {
            open_incident_window(&handle, &path);
        }
    }
    // Already running: single-instance (below) routes the second
    // invocation's URL here instead of spawning a second process.
    let handle = app.handle().clone();
    app.deep_link().on_open_url(move |event| {
        if let Some(path) = event.urls().first().and_then(|u| parse_incident_url(&u.to_string())) {
            open_incident_window(&handle, &path);
        }
    });
    Ok(())
})
```

Add the URL-parsing helper as a plain string function — deliberately
`&str` in, not whatever type the plugin's `.urls()`/`.get_current()`
return (`.to_string()` at each call site above converts either way, so
this helper doesn't need to match the plugin's exact wrapped type, only
its `Display`/`ToString` output) — pure, no Tauri types, testable on its
own:

```rust
fn parse_incident_url(url: &str) -> Option<String> {
    let path = url.strip_prefix("vigil://incident/")?;
    urlencoding::decode(path).ok().map(|s| s.into_owned())
}
```

**Known limitation, confirmed against Tauri's own docs:** custom URL
schemes cannot be registered at runtime for an app launched via `npm run
tauri dev` on macOS — this only resolves once the app is bundled and
installed under `/Applications`. Trigger B cannot be smoke tested until
Task 13's real build; Step 6 below only exercises trigger A.

- [ ] **Step 5: Wire the incidents-directory poller and notification (trigger A: clickable notification)**

Add a function `fn incidents_dir() -> PathBuf { std::env::var("VIGIL_UI_INCIDENTS_DIR").map(PathBuf::from).unwrap_or_else(|_| dirs::home_dir().unwrap_or_default().join(".vigil").join("incidents")) }`
(add the `dirs = "5"` crate to `ui/src-tauri/Cargo.toml` for
`home_dir()` — a small, standard crate for exactly this, avoiding a
hand-rolled `$HOME` lookup). This is the one poll-target override Step 6's
smoke test needs — set `VIGIL_UI_INCIDENTS_DIR=/tmp/vigil-ui-smoke` in the
environment `npm run tauri dev` runs in.

Add a background task (Tauri's `tauri::async_runtime::spawn` with a loop
and a `tokio::time::sleep`, or a plain `std::thread::spawn` with
`std::thread::sleep` — either is acceptable for a poll interval measured in
seconds) that, every few seconds:
1. Lists `{incidents_dir()}/*.md` (the same convention `incidents::list`
   uses on the main crate — reimplemented here with `std::fs::read_dir`
   directly, filtering to `.md` files and sorting, since `ui/` doesn't
   share a crate with `vigil`).
2. Tracks which paths it has already handled (an in-memory `HashSet` is
   sufficient — this doesn't need to survive a restart, since a missed
   notification on `vigil-ui` restart just means the user finds the
   incident via the menu-bar dropdown instead).
3. For each newly-seen path, reads it and checks (via a plain string
   `contains("**Alert key:**")` presence check) that it's a stub `vigil
   watch`/`vigil ui` wrote. If so:
   - Call `open_incident_window(&app_handle, &path)` (Step 4's helper) —
     this prepares the window's content *now*, before the user has clicked
     anything, per the corrected design at the top of this task. `.show()`
     without `.set_focus()` may be preferable here specifically (vs. the
     deep-link path, which the user just actively triggered) so the window
     becomes ready without yanking focus away from whatever the user is
     doing at that moment — use your judgment on `.show()` vs. leaving it
     hidden-but-content-ready until the notification click activates the
     app; either satisfies "tap the notification, see the right incident,"
     the difference is only whether the window is visibly present a moment
     before the tap.
   - Post a plain notification (no action — none is available on macOS):

```rust
use tauri_plugin_notification::NotificationExt;

app.notification()
    .builder()
    .title("vigil")
    .body(&rule_message) // from incidents::extract_rule_message's JS-side
                          // equivalent, or just read the stub's raw text —
                          // this file has no dependency on the main crate,
                          // so re-extract with a simple string search the
                          // same way the poller's own stub-detection does
    .show()
    .map_err(|e| eprintln!("[vigil-ui] failed to post notification: {e}"))
    .ok();
```

- [ ] **Step 6: Manual smoke test**

```bash
mkdir -p /tmp/vigil-ui-smoke
cat > /tmp/vigil-ui-smoke/2026-08-12-00-00-00-cpu-hog-1.md <<'EOF'
# vigil: process hogging CPU

**Alert key:** `cpu_hog:1`

**Rule message:** test process holding CPU
EOF
cd ui
VIGIL_UI_INCIDENTS_DIR=/tmp/vigil-ui-smoke npm run tauri dev
```

Expected: within the poll interval, the window's content updates for this
incident (per the corrected Step 5 design — check via the dev console or a
visible `.show()` that it actually happened) and a native notification
appears; clicking the notification brings the app to the foreground.

Do **not** attempt to test the `vigil://` URL scheme in this dev-mode
session — per Step 4's noted limitation, it cannot resolve here regardless
of whether the code is correct. That path is verified for real in Task
13's Step 5, against the actual bundled and installed app.

Clean up: `rm -rf /tmp/vigil-ui-smoke`.

- [ ] **Step 7: Commit**

```bash
cd /Users/denis/projects/vigil
git checkout -b investigate-ui-tauri-commands
git add ui/
git commit -m "Add vigil-ui Tauri commands, deep-link handoff, and incident-poll notifications"
git push -u origin investigate-ui-tauri-commands
gh pr create --title "Add vigil-ui backend commands + triggers" --body "Part of the investigate/fix UI plan (Task 9). Both triggers (notification click, menubar handoff) now open a window; the window itself is still the scaffold's placeholder content — Task 10 makes it real."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 10: `vigil-ui` — data-driven frontend from the approved mockup

**Files:**
- Modify: `ui/src/index.html` (or wherever the scaffold put the frontend
  entry point — replace its content with the mockup's, adapted)
- Create: `ui/src/incident.js` (the data-fetching/rendering logic)

**Interfaces:**
- Consumes: the four Tauri commands from Task 9, via `window.__TAURI__.core.invoke(...)`
  (or the `@tauri-apps/api` package's `invoke` import, whichever the
  scaffold's template already sets up).

No automated tests for this task (see the design spec's Testing section) —
manual smoke testing only, proportionate to this project's established
size.

- [ ] **Step 1: Copy the mockup's CSS and static structure into `ui/src/index.html`**

Copy the `<style>` block and the `<div class="page">...</div>` structure
verbatim from
`/private/tmp/claude-501/-Users-denis-projects/fe6ff656-3d80-4322-8a64-689d0aadc0a3/scratchpad/vigil-investigate-mockup.html`
into `ui/src/index.html`'s `<body>`. Keep every CSS custom property, class
name, and the light/dark `:root[data-theme]` handling exactly as written —
this is the approved visual contract.

Remove the mockup's hardcoded content inside the following elements (they
become populated by `incident.js` instead): the `.notif-title`/`.notif-body`
text, the `.vitals` block's four values, the diagnosis card's `.card-title`
and `.diagnosis-body`, the process-tree card's `.group`/`.row` elements, and
the fix card's `.fix-category`/`.fix-desc`/`.fix-target` content and step
count. Give each of these a stable `id` attribute (e.g. `id="diagnosis-body"`)
so `incident.js` can target them. Leave the mockup's `<svg>` icons,
structural chip/badge markup, and the fix-actions `approve`/`reject`
buttons' markup as-is (their `onclick` handlers get replaced, not their
markup).

- [ ] **Step 2: Write `ui/src/incident.js`**

```javascript
const { invoke } = window.__TAURI__.core;

function getIncidentPath() {
  const params = new URLSearchParams(window.location.search);
  return params.get("path");
}

function getIncidentsDir(path) {
  // The incidents directory is the path's parent directory.
  const idx = path.lastIndexOf("/");
  return idx === -1 ? "." : path.slice(0, idx);
}

async function loadIncident(path) {
  const incidentsDir = getIncidentsDir(path);
  let incident = await invoke("read_incident_json", { incidentsDir, path });

  if (!incident.diagnosis) {
    setThinking(true);
    try {
      await invoke("investigate", { alertKey: incident.alert_key, incidentsDir });
      incident = await invoke("read_incident_json", { incidentsDir, path });
    } catch (err) {
      showError(`Investigation failed: ${err}`);
      setThinking(false);
      return;
    }
    setThinking(false);
  }

  renderDiagnosis(incident);

  if (incident.alert_key) {
    const tree = await invoke("process_tree", { alertKey: incident.alert_key });
    renderProcessTree(tree);
  }

  if (incident.proposed_fix) {
    renderFixCard(incident.proposed_fix, path);
  }
}

function setThinking(isThinking) {
  document.getElementById("diagnosis-body").textContent = isThinking
    ? "Investigating…"
    : "";
}

function showError(message) {
  document.getElementById("diagnosis-body").textContent = message;
}

function renderDiagnosis(incident) {
  document.querySelector(".card-title .display").textContent = incident.title;
  document.querySelector(".alert-key").textContent = incident.alert_key ?? "";
  document.getElementById("diagnosis-body").textContent = incident.diagnosis ?? "No diagnosis yet.";
}

function renderProcessTree(nodes) {
  const container = document.getElementById("tree-container");
  container.innerHTML = "";
  for (const node of nodes) {
    const row = document.createElement("div");
    row.className = "row";
    const statusChip = node.is_zombie ? '<span class="chip leak">zombie</span>' : node.ppid === null ? '<span class="chip idle">orphan</span>' : '<span class="chip idle">child</span>';
    row.innerHTML = `
      <div class="proc-cell"><span class="proc-name-sm mono">${escapeHtml(node.name)}</span></div>
      <span class="parent mono">${node.ppid ?? "—"}</span>
      <span>${statusChip}</span>
      <span class="age mono">${formatDuration(node.run_time_secs)}</span>
      <span class="cpu mono">${node.cpu_pct.toFixed(1)}%</span>
      <span class="ram mono">${formatBytes(node.mem_bytes)}</span>
    `;
    container.appendChild(row);
  }
}

function renderFixCard(plan, path) {
  const card = document.querySelector(".fix-card");
  card.style.display = "";
  const steps = plan.plan;
  document.querySelector(".fix-step-label").textContent = `${steps.length} step${steps.length === 1 ? "" : "s"}`;

  const body = document.querySelector(".fix-body");
  const stepsContainer = document.createElement("div");
  const approvals = new Array(steps.length).fill(false);

  steps.forEach((step, i) => {
    const stepEl = document.createElement("div");
    stepEl.className = "fix-step";
    stepEl.innerHTML = `
      <div class="fix-category"><span class="dot"></span>${escapeHtml(step.category)}</div>
      <p class="fix-desc">${escapeHtml(step.description)}</p>
      <div class="fix-target"><span class="k">Target</span> <span class="v mono">${escapeHtml(step.target_hint)}</span></div>
      <label><input type="checkbox" data-step="${i}"> Approve this step</label>
    `;
    stepEl.querySelector("input").addEventListener("change", (e) => {
      approvals[i] = e.target.checked;
    });
    stepsContainer.appendChild(stepEl);
  });
  body.appendChild(stepsContainer);

  document.querySelector("button.approve").addEventListener("click", async () => {
    const result = await invoke("run_fix", { path, approvals });
    document.getElementById("fix-actions").style.display = "none";
    document.getElementById("result-approve").classList.add("show");
    document.getElementById("result-approve").textContent = result;
  });
  document.querySelector("button.reject").addEventListener("click", () => {
    document.getElementById("fix-actions").style.display = "none";
    document.getElementById("result-reject").classList.add("show");
  });
}

function escapeHtml(s) {
  const div = document.createElement("div");
  div.textContent = s;
  return div.innerHTML;
}

function formatDuration(secs) {
  const days = Math.floor(secs / 86400);
  const hours = Math.floor((secs % 86400) / 3600);
  if (days > 0) return `${days}d ${hours}h`;
  const mins = Math.floor((secs % 3600) / 60);
  if (hours > 0) return `${hours}h ${mins}m`;
  return `${mins}m`;
}

function formatBytes(bytes) {
  const mb = bytes / (1024 * 1024);
  if (mb > 1024) return `${(mb / 1024).toFixed(1)} GB`;
  return `${Math.round(mb)} MB`;
}

window.addEventListener("DOMContentLoaded", () => {
  const path = getIncidentPath();
  if (path) {
    loadIncident(path);
  } else {
    showError("No incident path provided.");
  }
});
```

Include this script in `ui/src/index.html` with `<script type="module" src="/incident.js"></script>`
before the closing `</body>`.

- [ ] **Step 2: Update Task 9's deep-link/notification-click handler to navigate with the path**

Go back to `ui/src-tauri/src/main.rs`'s window-opening code (Task 9, Steps
4-5) and confirm it sets the window's URL to include `?path=<the incident
file path>` so `incident.js`'s `getIncidentPath()` can read it — e.g.
`window.eval(&format!("window.location.search = '?path={}'",
urlencoding::encode(&path)))` or, more robustly, by constructing the
window with that URL from the start (`WebviewWindowBuilder::new(...).url(...)`)
rather than mutating an already-loaded page. Adjust Task 9's code for this
if it wasn't already accounting for it.

- [ ] **Step 3: Manual smoke test**

Using the same fake incident stub from Task 9's Step 6 (recreate it if
already cleaned up), run `npm run tauri dev`, trigger the window open via
either path (notification click or the `open 'vigil://incident/...'`
command), and confirm: the diagnosis card shows a "thinking" state then
(after `vigil investigate` actually runs — this spends real agent tokens)
the real diagnosis text; the process tree section shows real, currently-
running processes named/pidded per the alert key (for a `cpu_hog:<pid>`
test stub, use a real pid on the test machine, e.g. your own shell's pid,
so the tree isn't empty); if the agent proposed a fix, the fix card appears
with real category/description/target text and approve/reject work,
ending in `vigil fix` actually running (approve only a synthetic, harmless
step for this smoke test, the same discipline the original fix-execution
plan's own smoke test used).

- [ ] **Step 4: Commit**

```bash
cd /Users/denis/projects/vigil
git checkout -b investigate-ui-frontend
git add ui/
git commit -m "Wire vigil-ui's frontend to real data from the mockup's markup"
git push -u origin investigate-ui-frontend
gh pr create --title "vigil-ui: data-driven frontend" --body "Part of the investigate/fix UI plan (Task 10). The window now shows real diagnosis/process-tree/fix data instead of the mockup's hardcoded example, verified via manual smoke test (real vigil investigate/vigil fix runs, real tokens spent)."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 11: `LaunchAgent` for persistent `vigil-ui` operation

**Files:**
- Create: `ui/com.vigil.ui.plist` (a `launchd` agent definition)
- Modify: `README.md` (install instructions for it)

**Interfaces:** none — this is packaging/ops, not code.

- [ ] **Step 1: Write the LaunchAgent plist**

```xml
<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
    <key>Label</key>
    <string>com.vigil.ui</string>
    <key>ProgramArguments</key>
    <array>
        <string>/Applications/vigil-ui.app/Contents/MacOS/vigil-ui</string>
    </array>
    <key>RunAtLoad</key>
    <true/>
    <key>KeepAlive</key>
    <true/>
    <key>StandardOutPath</key>
    <string>/tmp/vigil-ui.log</string>
    <key>StandardErrorPath</key>
    <string>/tmp/vigil-ui.log</string>
</dict>
</plist>
```

- [ ] **Step 2: Document install/uninstall in `README.md`**

Add a subsection under wherever the README documents running `vigil
watch`/`vigil menubar` persistently, e.g.:

```markdown
### Running `vigil-ui` persistently

`vigil-ui` needs to be running for journal-worthy alert notifications to
appear at all (see [docs/superpowers/specs/2026-08-12-investigate-ui-design.md](docs/superpowers/specs/2026-08-12-investigate-ui-design.md)) —
install it as a LaunchAgent so it survives reboots:

```bash
cp ui/com.vigil.ui.plist ~/Library/LaunchAgents/com.vigil.ui.plist
launchctl load ~/Library/LaunchAgents/com.vigil.ui.plist
```

To stop it: `launchctl unload ~/Library/LaunchAgents/com.vigil.ui.plist`.
```

- [ ] **Step 3: Commit**

```bash
git checkout -b investigate-ui-launchagent
git add ui/com.vigil.ui.plist README.md
git commit -m "Add a LaunchAgent for persistent vigil-ui operation"
git push -u origin investigate-ui-launchagent
gh pr create --title "Add vigil-ui LaunchAgent" --body "Part of the investigate/fix UI plan (Task 11)."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 12: Update `AGENTS.md` and `README.md` for `vigil-ui`

**Files:**
- Modify: `AGENTS.md`
- Modify: `README.md`

No automated test — documentation only.

- [ ] **Step 1: Add `vigil-ui` to AGENTS.md's project-wide sections**

In AGENTS.md's testing section, add a short bullet noting `ui/`'s test
command (`cd ui/src-tauri && cargo test`) alongside the existing `cargo
test`/`uv run pytest` bullets — three test suites now, not two.

In "The live incident-monitoring loop" section, add one sentence noting
that `vigil-ui` (not `vigil watch`/`vigil ui`) is what actually notifies
for journal-worthy alerts now, and that it needs to be running (per Task
11's LaunchAgent) for that to happen — cross-reference
`docs/superpowers/specs/2026-08-12-investigate-ui-design.md` rather than
restating its whole rationale.

- [ ] **Step 2: Update README.md's Architecture section**

Add `ui/` as a third top-level project alongside the existing `src/`/
`agent/` two-column diagram, following whatever brief style the existing
diagram uses — a short one-paragraph description (Tauri companion app,
what triggers it, what it shells out to) is enough; it doesn't need its own
internal file-by-file diagram the way `src/` does, since `ui/` is a
standard Tauri project layout any Tauri-familiar reader already knows.

- [ ] **Step 3: Re-read both files once for internal consistency**

Confirm AGENTS.md's testing section, the live-incident-loop section, and
README's Architecture section all agree on the same facts (which process
notifies journal-worthy alerts, what `ui/`'s test command is) — no
contradictions between the three.

- [ ] **Step 4: Commit**

```bash
git checkout -b investigate-ui-docs
git add AGENTS.md README.md
git commit -m "Document vigil-ui in AGENTS.md and README.md"
git push -u origin investigate-ui-docs
gh pr create --title "Document vigil-ui" --body "Part of the investigate/fix UI plan (Task 12). Documentation only."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 13: Final verification and end-to-end manual smoke test

**Files:** none (verification only).

- [ ] **Step 1: Full test suites**

```bash
cargo test --release
cargo llvm-cov --workspace --ignore-filename-regex 'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs' --fail-under-lines 99.5 --fail-under-regions 98
cd agent && uv run pytest
cd ../ui/src-tauri && cargo test
```
Expected: all four PASS.

- [ ] **Step 2: Build everything for real**

```bash
cd /Users/denis/projects/vigil
cargo build --release
cd ui && npm run tauri build
```
Expected: both succeed; `ui/src-tauri/target/release/bundle/macos/` contains
a `vigil-ui.app`.

- [ ] **Step 3: Install and run the LaunchAgent**

```bash
cp ui/src-tauri/target/release/bundle/macos/vigil-ui.app /Applications/
cp ui/com.vigil.ui.plist ~/Library/LaunchAgents/com.vigil.ui.plist
launchctl load ~/Library/LaunchAgents/com.vigil.ui.plist
```
Confirm no Dock icon appears and `ps aux | grep vigil-ui` shows it running.

- [ ] **Step 4: End-to-end trigger A (notification)**

Restart `vigil watch` from the newly-built release binary (kill the old
PID, relaunch per this project's usual `nohup`/`disown` pattern). Wait for
a real journal-worthy alert to fire (or trigger one artificially, e.g. a
`cpu_hog` by running something CPU-heavy briefly). Confirm: a clickable
notification appears (not the old plain osascript one), and clicking it
opens the investigate window showing real, live data for that incident.

- [ ] **Step 5: End-to-end trigger B (menubar)**

Restart `vigil menubar` from the newly-built release binary. Click the
tray icon, click an incident in the dropdown. Confirm the same investigate
window opens (reusing the already-running `vigil-ui` instance via
`tauri-plugin-single-instance`, not spawning a second one).

- [ ] **Step 6: Clean up test artifacts and report**

Kill/unload anything spun up purely for this smoke test that shouldn't
stay running long-term (test LaunchAgent, if a throwaway plist path was
used instead of the real one). Write a short summary of what passed and
any deviations encountered (e.g. an actual plugin crate name that differed
from what Task 9 specified) directly in this task's completion notes — no
separate report file needed, this is the last task in the plan.
