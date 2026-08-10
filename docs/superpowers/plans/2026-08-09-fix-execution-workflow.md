# Fix-Execution Workflow Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Replace vigil's automatic, always-diagnose alert path with an opt-in
`vigil investigate <alert-key>` → (agent may propose a fix) → `vigil fix
<incident-file>` (per-step human approval) → narrowly-scoped execute-agent
pipeline, per the approved design in
`docs/superpowers/specs/2026-08-09-fix-execution-workflow-design.md`.

**Architecture:** An alert firing writes a header-only incident stub and notifies
with the command to investigate it — no agent spawns automatically. `vigil
investigate` runs today's unchanged read-only diagnosis agent and appends its
answer (optionally including a `## Proposed fix` JSON plan) to the stub. `vigil
fix` parses that plan, prompts for per-step terminal approval, and hands only the
approved steps to a brand-new, separate Python module (`execute.py`) whose
`ClaudeAgentOptions` unlocks only the Bash patterns the approved fix categories
call for, on top of a non-liftable hard floor. Every new piece of logic is split
pure-function-first (parsing, formatting, question-building) from OS-boundary glue
(process spawns, file IO), mirroring `agent.rs`/`agent_process.rs`'s existing split,
so the coverage gate stays met without inflating the `--ignore-filename-regex` list
beyond genuinely irreducible glue.

**Tech Stack:** Rust (clap, serde/serde_json, sysinfo — all already dependencies),
Python (Claude Agent SDK, argparse — already dependencies), `uv` for all Python
tooling.

## Global Constraints

- Rust: `cargo llvm-cov --workspace --ignore-filename-regex
  'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs'
  --fail-under-lines 99.5 --fail-under-regions 98` must pass after Task 13 (which
  updates the regex); until then, run the OLD regex (from AGENTS.md as it exists
  today: `'src/(main|watch|ui_loop|menubar_loop|agent_process|notify)\.rs'`) with
  `|investigate_process|fix_process` appended ad hoc once those two files exist
  (Tasks 6-7), since they're genuine OS-boundary glue same as `agent_process.rs`.
- Python: `cd agent && uv run pytest` (`--cov-fail-under=99.9`, from
  `agent/pyproject.toml`) must pass after every Python task.
- Always `uv` for Python — `uv sync`, `uv run pytest`, `uv run vigil-agent ...`.
  Never a manual `venv`/`pip install`.
- Everything written into the repo (code, identifiers, comments, docs, commit
  messages) is English — this is an explicit standing project rule (AGENTS.md's
  Language section), regardless of what language you're conversing with the user in.
- One PR per task, following this project's existing Git workflow (branch → commit
  → push → `gh pr create` → `gh pr merge --squash --delete-branch` once
  `cargo test --release`/`uv run pytest` are green locally — no CI configured yet).
  After merging a task that touches `watch.rs`, `ui_loop.rs`, `agent_process.rs`,
  `investigate_process.rs`, or `fix_process.rs`, rebuild and restart any running
  `vigil watch`/`vigil menubar` background processes from the merged `master`.
- New Rust logic gets a pure, unit-tested function kept separate from any
  `Command`/file-IO call, per AGENTS.md's testing section — follow the
  `agent.rs`/`agent_process.rs` split for every new file pair below.

---

## Task 1: `fixplan.rs` — plan schema, parsing, and pure formatting

**Files:**
- Create: `src/fixplan.rs`
- Modify: `src/main.rs:1-14` (add `mod fixplan;` to the alphabetized module list)

**Interfaces:**
- Produces: `pub enum FixCategory { KillProcess, DeletePath, SystemSetting }` (serde
  `rename_all = "snake_case"`) with `pub fn as_str(&self) -> &'static str`; `pub
  struct PlanStep { pub category: FixCategory, pub description: String, pub
  target_hint: String }`; `pub struct Plan { pub plan: Vec<PlanStep> }`; `pub fn
  extract_proposed_fix_json(content: &str) -> Option<&str>`; `pub fn
  parse_plan(json: &str) -> Result<Plan, String>`; `pub fn
  approved_steps_json(steps: &[PlanStep], approved: &[bool]) -> String`; `pub fn
  approved_header(approved: &[bool], timestamp: &str) -> String`. Consumed by
  `investigate_process.rs` (indirectly, via the incident file it writes) and
  `fix_process.rs` (directly, Task 7).

- [ ] **Step 1: Write `src/fixplan.rs` with its full test module**

```rust
//! Parses and represents the `## Proposed fix` JSON plan an investigating
//! agent may append to an incident file, and the small amount of pure logic
//! around it (selecting the user-approved subset, building the report
//! header). Execution itself — actually running the plan through a
//! dedicated agent session — lives in `fix_process.rs`; this file has no
//! process-spawning or file IO at all, so it's fully unit-tested.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixCategory {
    KillProcess,
    DeletePath,
    SystemSetting,
}

impl FixCategory {
    /// The exact string `vigil-agent execute` expects among its plan
    /// steps' `category` values — kept in sync with `serde`'s
    /// `rename_all` above via a test, since the two are independently
    /// spelled out.
    pub fn as_str(&self) -> &'static str {
        match self {
            FixCategory::KillProcess => "kill_process",
            FixCategory::DeletePath => "delete_path",
            FixCategory::SystemSetting => "system_setting",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub category: FixCategory,
    pub description: String,
    pub target_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub plan: Vec<PlanStep>,
}

/// Extract the fenced ```json block that follows a `## Proposed fix`
/// heading in an incident file's markdown. `None` if there's no such
/// heading, or the heading has no well-formed fenced JSON block after it —
/// both read as "this incident has no proposed fix", not an error.
pub fn extract_proposed_fix_json(content: &str) -> Option<&str> {
    let heading_pos = content.find("## Proposed fix")?;
    let after_heading = &content[heading_pos..];
    let fence_start = after_heading.find("```json")?;
    let json_start = fence_start + "```json".len();
    let rest = &after_heading[json_start..];
    let fence_end = rest.find("```")?;
    let json = rest[..fence_end].trim();
    if json.is_empty() {
        None
    } else {
        Some(json)
    }
}

pub fn parse_plan(json: &str) -> Result<Plan, String> {
    serde_json::from_str(json).map_err(|e| format!("failed to parse proposed fix plan: {e}"))
}

/// JSON array of just the approved steps, in their original order — the
/// exact text handed to `vigil-agent execute --plan-json`. `approved` must
/// be the same length as `steps`; a mismatch is a caller bug (the
/// interactive prompt loop always builds one bool per step), not a
/// user-facing error, so this asserts rather than returning `Result`.
pub fn approved_steps_json(steps: &[PlanStep], approved: &[bool]) -> String {
    assert_eq!(steps.len(), approved.len(), "approved.len() must match steps.len()");
    let approved_steps: Vec<&PlanStep> =
        steps.iter().zip(approved.iter()).filter(|(_, &a)| a).map(|(s, _)| s).collect();
    serde_json::to_string(&approved_steps).expect("PlanStep serialization cannot fail")
}

/// The `_Approved: ..._` line prefixed to the execute-agent's own report
/// before it's appended under the incident file's `## Fix execution`
/// heading. `approved` is 0-indexed positions into the plan's steps; the
/// header numbers them 1-indexed to match what the interactive prompt
/// showed the user.
pub fn approved_header(approved: &[bool], timestamp: &str) -> String {
    let total = approved.len();
    let numbers: Vec<String> =
        approved.iter().enumerate().filter(|(_, &a)| a).map(|(i, _)| (i + 1).to_string()).collect();
    format!("_Approved: {timestamp} (steps {} of {total})_", numbers.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PLAN_JSON: &str = r#"{
      "plan": [
        {
          "category": "kill_process",
          "description": "Kill the stale claude session",
          "target_hint": "claude --worktree nervous-cori-c94163 --resume skill-adopt"
        },
        {
          "category": "delete_path",
          "description": "Remove the orphaned node_modules cache",
          "target_hint": "/tmp/some/node_modules/.cache"
        }
      ]
    }"#;

    #[test]
    fn fix_category_as_str_matches_its_serde_rename() {
        for cat in [FixCategory::KillProcess, FixCategory::DeletePath, FixCategory::SystemSetting] {
            let serialized = serde_json::to_string(&cat).unwrap();
            assert_eq!(serialized, format!("\"{}\"", cat.as_str()));
        }
    }

    #[test]
    fn extract_proposed_fix_json_finds_the_fenced_block_after_the_heading() {
        let content = format!("# vigil: cpu hog\n\n## Diagnosis\n\ntext\n\n## Proposed fix\n\n```json\n{SAMPLE_PLAN_JSON}\n```\n");
        let extracted = extract_proposed_fix_json(&content).unwrap();
        assert!(extracted.contains("kill_process"));
    }

    #[test]
    fn extract_proposed_fix_json_is_none_without_the_heading() {
        let content = "# vigil: cpu hog\n\n## Diagnosis\n\nno fix here.\n";
        assert_eq!(extract_proposed_fix_json(content), None);
    }

    #[test]
    fn extract_proposed_fix_json_is_none_when_heading_has_no_fenced_block() {
        let content = "# vigil\n\n## Proposed fix\n\n(none identified)\n";
        assert_eq!(extract_proposed_fix_json(content), None);
    }

    #[test]
    fn extract_proposed_fix_json_is_none_for_an_empty_fenced_block() {
        let content = "## Proposed fix\n\n```json\n\n```\n";
        assert_eq!(extract_proposed_fix_json(content), None);
    }

    #[test]
    fn parse_plan_reads_categories_descriptions_and_target_hints() {
        let plan = parse_plan(SAMPLE_PLAN_JSON).unwrap();
        assert_eq!(plan.plan.len(), 2);
        assert_eq!(plan.plan[0].category, FixCategory::KillProcess);
        assert_eq!(plan.plan[0].description, "Kill the stale claude session");
        assert_eq!(plan.plan[1].category, FixCategory::DeletePath);
        assert_eq!(plan.plan[1].target_hint, "/tmp/some/node_modules/.cache");
    }

    #[test]
    fn parse_plan_rejects_an_unknown_category() {
        let json = r#"{"plan": [{"category": "reboot_machine", "description": "d", "target_hint": "h"}]}"#;
        assert!(parse_plan(json).is_err());
    }

    #[test]
    fn parse_plan_rejects_malformed_json() {
        assert!(parse_plan("not json").is_err());
    }

    #[test]
    fn approved_steps_json_includes_only_approved_steps_in_order() {
        let plan = parse_plan(SAMPLE_PLAN_JSON).unwrap();
        let json = approved_steps_json(&plan.plan, &[false, true]);
        let reparsed: Vec<PlanStep> = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed.len(), 1);
        assert_eq!(reparsed[0].category, FixCategory::DeletePath);
    }

    #[test]
    fn approved_steps_json_is_an_empty_array_when_nothing_is_approved() {
        let plan = parse_plan(SAMPLE_PLAN_JSON).unwrap();
        assert_eq!(approved_steps_json(&plan.plan, &[false, false]), "[]");
    }

    #[test]
    #[should_panic(expected = "approved.len() must match steps.len()")]
    fn approved_steps_json_panics_on_a_length_mismatch() {
        let plan = parse_plan(SAMPLE_PLAN_JSON).unwrap();
        approved_steps_json(&plan.plan, &[true]);
    }

    #[test]
    fn approved_header_numbers_only_approved_steps_one_indexed() {
        let header = approved_header(&[false, true, true], "2026-08-09 02:30");
        assert_eq!(header, "_Approved: 2026-08-09 02:30 (steps 2, 3 of 3)_");
    }

    #[test]
    fn approved_header_handles_a_single_step_plan() {
        let header = approved_header(&[true], "2026-08-09 02:30");
        assert_eq!(header, "_Approved: 2026-08-09 02:30 (steps 1 of 1)_");
    }
}
```

- [ ] **Step 2: Add the module to `src/main.rs`**

In `src/main.rs`, insert `mod fixplan;` into the alphabetized `mod` list (after
`cli;`, before `incidents;`).

- [ ] **Step 3: Run the tests**

Run: `cargo test fixplan`
Expected: all 14 tests in `fixplan::tests` PASS.

- [ ] **Step 4: Commit**

```bash
git checkout -b fix-workflow-fixplan
git add src/fixplan.rs src/main.rs
git commit -m "Add fixplan.rs: proposed-fix JSON plan parsing and formatting"
git push -u origin fix-workflow-fixplan
gh pr create --title "Add fixplan.rs: proposed-fix plan parsing" --body "Part of the fix-execution workflow (docs/superpowers/plans/2026-08-09-fix-execution-workflow.md, Task 1). Pure schema/parsing/formatting for the \`## Proposed fix\` JSON block — no behavior change yet, nothing calls this."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 2: `incidents.rs` — stub-write and append-section API

**Files:**
- Modify: `src/incidents.rs` (full rewrite — see below)

**Interfaces:**
- Produces: `pub struct IncidentStub<'a> { pub alert_key: &'a str, pub alert_title:
  &'a str, pub alert_message: &'a str }`; `pub fn write_stub(dir: &Path, stub:
  &IncidentStub) -> Result<PathBuf, String>`; `pub fn append_diagnosis(path: &Path,
  diagnosis: &str) -> Result<(), String>`; `pub fn append_fix_execution(path:
  &Path, journal: &str) -> Result<(), String>`; `pub fn extract_rule_message(content:
  &str) -> Option<&str>`; `pub fn human_timestamp() -> String`; `pub(crate) fn
  slugify(key: &str) -> String` (visibility widened from private). Existing `pub fn
  default_dir() -> PathBuf`, `pub fn list(dir: &Path) -> Result<Vec<PathBuf>,
  String>`, `pub fn extract_title(content: &str) -> &str` are unchanged. The old
  `pub struct Incident` and `pub fn record(...)` stay in place for this task — they
  still have their existing callers (`agent_process::maybe_diagnose_alert_async`)
  until Task 9's cutover removes both sides together.
- Consumes: nothing new.

- [ ] **Step 1: Add the new functions, struct, and tests to `src/incidents.rs`**

Replace the module doc comment at the top of the file (lines 1-6) with:

```rust
//! Persists incident data to a local markdown journal —
//! `<dir>/<date>-<time>-<slug>.md`, one file per alert-worthy incident.
//!
//! An alert firing writes a stub immediately (`write_stub`): title, alert
//! key, rule message only, no diagnosis yet. `vigil investigate <key>`
//! appends a `## Agent diagnosis` section later (`append_diagnosis`), and
//! `vigil fix <file>` appends a `## Fix execution` section after that
//! (`append_fix_execution`) if the diagnosis proposed one. Nothing here
//! runs automatically — see `investigate_process.rs`/`fix_process.rs` for
//! what calls these functions and when.
//!
//! Only alert-fired incidents are logged here — the interactive 'a'/'w'
//! flow in the UI is deliberately not, it stays on-screen only.
```

Immediately after `default_dir()`'s closing brace and before `pub struct Incident`,
insert:

```rust
/// The header-only content an alert firing writes immediately — see the
/// module doc comment for the full lifecycle.
pub struct IncidentStub<'a> {
    pub alert_key: &'a str,
    pub alert_title: &'a str,
    pub alert_message: &'a str,
}

/// Write a new incident file into `dir` (created if missing): title, alert
/// key, and rule message only — no diagnosis section. Returns the path
/// written, which the caller (a notification, an interactive prompt) can
/// point back at.
pub fn write_stub(dir: &Path, stub: &IncidentStub) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("failed to create incidents dir {}: {e}", dir.display()))?;

    let filename = format!("{}-{}.md", timestamp_prefix(), slugify(stub.alert_key));
    let path = dir.join(filename);

    let body = format!(
        "# {}\n\n**Alert key:** `{}`\n\n**Rule message:** {}\n",
        stub.alert_title, stub.alert_key, stub.alert_message
    );
    let mut f = std::fs::File::create(&path).map_err(|e| format!("failed to create {}: {e}", path.display()))?;
    // Coverage exemption (see AGENTS.md's testing section): triggering a
    // write failure on an already-successfully-created file needs a fault
    // (disk full, quota, revoked permissions mid-write) that isn't
    // reasonably reproducible from a unit test.
    f.write_all(body.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;

    Ok(path)
}

/// Append a `## Agent diagnosis` section to an existing incident file —
/// called once, by `vigil investigate`, after the stub was already
/// written. `diagnosis` is the agent's raw answer text, which may itself
/// contain its own `## Diagnosis`/`## Suggestions`/`## Proposed fix`
/// markdown headings nested under this one.
pub fn append_diagnosis(path: &Path, diagnosis: &str) -> Result<(), String> {
    append_section(path, "Agent diagnosis", diagnosis)
}

/// Append a `## Fix execution` section to an existing incident file —
/// called once, by `vigil fix`, after the execute-agent finished (or
/// aborted partway through) an approved plan.
pub fn append_fix_execution(path: &Path, journal: &str) -> Result<(), String> {
    append_section(path, "Fix execution", journal)
}

fn append_section(path: &Path, heading: &str, body: &str) -> Result<(), String> {
    let mut f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("failed to open {} for appending: {e}", path.display()))?;
    // Coverage exemption (see AGENTS.md's testing section): triggering a
    // write failure on an already-successfully-opened file needs a fault
    // (disk full, quota, revoked permissions mid-write) that isn't
    // reasonably reproducible from a unit test.
    write!(f, "\n## {heading}\n\n{}\n", body.trim_end())
        .map_err(|e| format!("failed to append to {}: {e}", path.display()))?;
    Ok(())
}
```

Immediately after `extract_title(...)`'s closing brace and before `fn slugify`,
insert:

```rust
/// The text after `**Rule message:**` on its own line — what `vigil
/// investigate` hands the agent as the thing to investigate, since the
/// stub file (not a live `Alert`) is all it has to go on.
pub fn extract_rule_message(content: &str) -> Option<&str> {
    content
        .lines()
        .find_map(|l| l.trim().strip_prefix("**Rule message:**"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}
```

Change `fn slugify` to `pub(crate) fn slugify` (needed by `investigate.rs` in
Task 5 to normalize a CLI-supplied alert key the exact same way `write_stub` did
when it named the file), and update its doc comment:

```rust
/// `alert.key` values can contain `:`/other punctuation (e.g. `cpu_hog:1234`)
/// that isn't filename-safe — normalize to lowercase hyphen-separated words.
/// `pub(crate)` (not private) because `investigate.rs` needs the exact same
/// normalization to resolve a CLI-supplied alert key back to its file.
pub(crate) fn slugify(key: &str) -> String {
```

Replace `fn timestamp_prefix() -> String { ... }` with a shared helper plus two
thin callers:

```rust
fn date_output(format: &str) -> String {
    Command::new("date")
        .arg(format)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        // Coverage exemption (see AGENTS.md's testing section): reaching
        // this fallback needs the system `date` binary itself to be
        // missing or broken, which isn't something to fake from a test
        // without mocking `Command` (the exact thing this file's pure/IO
        // split is meant to avoid needing).
        .unwrap_or_else(|| "unknown-time".to_string())
}

/// Shells out to `date` rather than pulling in a chrono-style dependency
/// just for this — consistent with how `pmset`/`osascript` are already
/// used elsewhere for OS-specific info.
fn timestamp_prefix() -> String {
    date_output("+%Y-%m-%d-%H-%M-%S")
}

/// Same idea, human-readable (`2026-08-09 02:30`) for the `_Approved:
/// ..._` line `fixplan::approved_header` builds.
pub fn human_timestamp() -> String {
    date_output("+%Y-%m-%d %H:%M")
}
```

Add these tests inside the existing `#[cfg(test)] mod tests` block, after the
existing `slugify_normalizes_punctuation_and_case` test and before the `record_*`
tests (which stay untouched in this task):

```rust
    #[test]
    fn write_stub_creates_markdown_with_header_only() {
        let dir = test_dir();
        let stub = IncidentStub {
            alert_key: "high_load",
            alert_title: "vigil: high load",
            alert_message: "Load average 12.0 ...",
        };

        let path = write_stub(&dir, &stub).unwrap();
        assert!(path.exists());
        assert!(path.file_name().unwrap().to_string_lossy().ends_with("-high-load.md"));

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("vigil: high load"));
        assert!(content.contains("`high_load`"));
        assert!(content.contains("Load average 12.0"));
        assert!(!content.contains("## Agent diagnosis"), "stub must not have a diagnosis section yet");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_stub_creates_missing_directory() {
        let dir = test_dir();
        assert!(!dir.exists());
        let stub = IncidentStub { alert_key: "battery_low", alert_title: "t", alert_message: "m" };
        write_stub(&dir, &stub).unwrap();
        assert!(dir.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_stub_fails_when_the_incidents_dir_cannot_be_created() {
        let parent = test_dir();
        std::fs::create_dir_all(&parent).unwrap();
        let mut perms = std::fs::metadata(&parent).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&parent, perms).unwrap();

        let dir = parent.join("cant-create-this");
        let stub = IncidentStub { alert_key: "k", alert_title: "t", alert_message: "m" };
        let result = write_stub(&dir, &stub);

        let mut writable = std::fs::metadata(&parent).unwrap().permissions();
        writable.set_readonly(false);
        std::fs::set_permissions(&parent, writable).unwrap();
        let _ = std::fs::remove_dir_all(&parent);

        assert!(result.is_err());
    }

    #[test]
    fn write_stub_fails_when_the_file_cannot_be_created() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&dir, perms).unwrap();

        let stub = IncidentStub { alert_key: "k", alert_title: "t", alert_message: "m" };
        let result = write_stub(&dir, &stub);

        let mut writable = std::fs::metadata(&dir).unwrap().permissions();
        writable.set_readonly(false);
        std::fs::set_permissions(&dir, writable).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
    }

    #[test]
    fn append_diagnosis_adds_a_heading_and_body_after_the_stub() {
        let dir = test_dir();
        let stub = IncidentStub { alert_key: "cpu_hog:1", alert_title: "vigil: cpu hog", alert_message: "m" };
        let path = write_stub(&dir, &stub).unwrap();

        append_diagnosis(&path, "## Diagnosis\n\nThe culprit is pycharm.").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Agent diagnosis"));
        assert!(content.contains("The culprit is pycharm."));
        let stub_pos = content.find("**Rule message:**").unwrap();
        let diag_pos = content.find("## Agent diagnosis").unwrap();
        assert!(stub_pos < diag_pos);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_diagnosis_fails_for_a_missing_file() {
        let dir = test_dir();
        let missing = dir.join("does-not-exist.md");
        assert!(append_diagnosis(&missing, "text").is_err());
    }

    #[test]
    fn append_fix_execution_adds_its_own_heading_after_diagnosis() {
        let dir = test_dir();
        let stub = IncidentStub { alert_key: "cpu_hog:1", alert_title: "vigil: cpu hog", alert_message: "m" };
        let path = write_stub(&dir, &stub).unwrap();
        append_diagnosis(&path, "## Diagnosis\n\ntext\n\n## Proposed fix\n\n```json\n{}\n```").unwrap();

        append_fix_execution(&path, "_Approved: 2026-08-09 02:30 (steps 1 of 1)_\n\n1. done").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Fix execution"));
        assert!(content.contains("1. done"));
        let diag_pos = content.find("## Agent diagnosis").unwrap();
        let fix_pos = content.find("## Fix execution").unwrap();
        assert!(diag_pos < fix_pos);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_fix_execution_fails_for_a_missing_file() {
        let dir = test_dir();
        let missing = dir.join("does-not-exist.md");
        assert!(append_fix_execution(&missing, "text").is_err());
    }

    #[test]
    fn extract_rule_message_reads_the_field_line() {
        let content = "# t\n\n**Alert key:** `k`\n\n**Rule message:** Load average 12.0 (threshold 24.0).\n";
        assert_eq!(extract_rule_message(content), Some("Load average 12.0 (threshold 24.0)."));
    }

    #[test]
    fn extract_rule_message_is_none_when_the_field_is_absent() {
        assert_eq!(extract_rule_message("# t\n\nno rule message field here\n"), None);
    }

    #[test]
    fn human_timestamp_matches_yyyy_mm_dd_hh_mm() {
        let ts = human_timestamp();
        assert_eq!(ts.len(), 16, "unexpected format: {ts}");
        assert_eq!(ts.chars().nth(4), Some('-'));
        assert_eq!(ts.chars().nth(7), Some('-'));
        assert_eq!(ts.chars().nth(10), Some(' '));
        assert_eq!(ts.chars().nth(13), Some(':'));
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test incidents::`
Expected: every test in `incidents::tests` PASSES, including the pre-existing
`record_*` ones (untouched) and the new `write_stub_*`/`append_*`/
`extract_rule_message_*`/`human_timestamp_*` ones.

- [ ] **Step 3: Commit**

```bash
git checkout -b fix-workflow-incidents-stub-api
git add src/incidents.rs
git commit -m "Add incidents::write_stub/append_diagnosis/append_fix_execution API"
git push -u origin fix-workflow-incidents-stub-api
gh pr create --title "Add incidents.rs stub-write + append API" --body "Part of the fix-execution workflow (Task 2). Additive — old record()/Incident stay in place until the Task 9 cutover removes both sides together."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 3: `agent.rs` — investigate-hint notification text and execute argv

**Files:**
- Modify: `src/agent.rs`

**Interfaces:**
- Produces: `pub(crate) fn augment_with_investigate_hint(alert: &crate::alerts::Alert,
  watch_log_path: Option<&str>) -> crate::alerts::Alert`; `pub(crate) fn
  build_execute_args(plan_json: &str, agent_dir: &str) -> Vec<String>`. Both consumed
  in Task 9 (watch.rs/ui_loop.rs) and Task 4 (agent_process.rs) respectively.
- Consumes: `crate::alerts::Alert` (existing struct, unchanged).

- [ ] **Step 1: Add the two functions and their tests to `src/agent.rs`**

Insert after `is_auto_diagnose_worthy`'s closing brace (that function itself is
untouched in this task — it's deleted in Task 9's cutover):

```rust
/// Builds the notification `Alert` for a just-fired incident: the message
/// now points at the explicit opt-in investigate command instead of
/// promising a diagnosis that no longer happens automatically. `watch_log`
/// is included in the hinted command when the caller has one (`vigil
/// watch` does; `vigil ui`'s own snapshot loop doesn't — see its call
/// site) so the eventual `vigil investigate` run can still check trend
/// history, not just the one snapshot at alert-fire time. Pure —
/// everything else about `alert` is carried through unchanged.
pub(crate) fn augment_with_investigate_hint(
    alert: &crate::alerts::Alert,
    watch_log_path: Option<&str>,
) -> crate::alerts::Alert {
    let hint = match watch_log_path {
        Some(p) => format!("`vigil investigate {} --watch-log {p}`", alert.key),
        None => format!("`vigil investigate {}`", alert.key),
    };
    crate::alerts::Alert {
        key: alert.key.clone(),
        title: alert.title.clone(),
        message: format!("{} — investigate? {hint}", alert.message),
        target: alert.target.clone(),
        command: alert.command.clone(),
    }
}

/// Pure argv construction for `vigil-agent execute`, the execute-agent
/// invocation `fix_process::run` spawns after the user approves a plan —
/// mirrors `build_args`'s split from the actual `Command` spawn.
/// `plan_json` is the JSON array of *already-approved* steps only (see
/// `fixplan::approved_steps_json`) — the execute-agent never receives the
/// rejected ones.
pub(crate) fn build_execute_args(plan_json: &str, agent_dir: &str) -> Vec<String> {
    vec![
        "uv".to_string(),
        "run".to_string(),
        "--project".to_string(),
        agent_dir.to_string(),
        "vigil-agent".to_string(),
        "execute".to_string(),
        "--plan-json".to_string(),
        plan_json.to_string(),
    ]
}
```

Add these tests inside the existing `#[cfg(test)] mod tests` block, near the
`auto_diagnose_worthy_*` tests:

```rust
    #[test]
    fn augment_with_investigate_hint_appends_the_command_and_keeps_other_fields() {
        let alert = crate::alerts::Alert {
            key: "cpu_hog:37489".to_string(),
            title: "vigil: process hogging CPU".to_string(),
            message: "pycharm (pid 37489) has held 204% CPU".to_string(),
            target: Some("pycharm".to_string()),
            command: Some("/Applications/PyCharm.app/Contents/MacOS/pycharm".to_string()),
        };
        let augmented = augment_with_investigate_hint(&alert, Some("/Users/denis/.vigil/watch.jsonl"));
        assert!(augmented.message.contains("pycharm (pid 37489) has held 204% CPU"));
        assert!(augmented.message.contains("vigil investigate cpu_hog:37489"));
        assert!(augmented.message.contains("--watch-log /Users/denis/.vigil/watch.jsonl"));
        assert_eq!(augmented.key, alert.key);
        assert_eq!(augmented.title, alert.title);
        assert_eq!(augmented.target, alert.target);
        assert_eq!(augmented.command, alert.command);
    }

    #[test]
    fn augment_with_investigate_hint_omits_watch_log_flag_when_none() {
        let alert = crate::alerts::Alert {
            key: "high_load".to_string(),
            title: "vigil: high load".to_string(),
            message: "Load average 25.0".to_string(),
            target: Some("high_load".to_string()),
            command: None,
        };
        let augmented = augment_with_investigate_hint(&alert, None);
        assert!(!augmented.message.contains("--watch-log"));
        assert!(augmented.message.contains("vigil investigate high_load"));
    }

    #[test]
    fn build_execute_args_wires_project_dir_and_plan_json() {
        let args = build_execute_args(r#"[{"category":"kill_process"}]"#, "agent");
        assert_eq!(args[0], "uv");
        assert_eq!(args[1], "run");
        assert!(args.windows(2).any(|w| w == ["--project".to_string(), "agent".to_string()]));
        assert_eq!(args.last().unwrap(), r#"[{"category":"kill_process"}]"#);
        assert!(args.contains(&"execute".to_string()));
    }
```

- [ ] **Step 2: Run the tests**

Run: `cargo test agent::`
Expected: all `agent::tests` PASS, including the 3 new ones.

- [ ] **Step 3: Commit**

```bash
git checkout -b fix-workflow-agent-hint-and-execargs
git add src/agent.rs
git commit -m "Add investigate-hint notification text and execute-agent argv building"
git push -u origin fix-workflow-agent-hint-and-execargs
gh pr create --title "Add agent.rs investigate-hint + execute argv builders" --body "Part of the fix-execution workflow (Task 3). Additive — is_auto_diagnose_worthy/maybe_diagnose_alert_async stay in place until Task 9."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 4: `agent_process.rs` — `execute_fix` process spawn

**Files:**
- Modify: `src/agent_process.rs`

**Interfaces:**
- Produces: `pub fn execute_fix(plan_json: &str, agent_dir: &str) -> Result<String,
  String>`. Consumed by `fix_process.rs` (Task 7).
- Consumes: `crate::agent::build_execute_args` (Task 3), `crate::agent::interpret_output`
  (existing, already `pub(crate)`).

This file is in the coverage `--ignore-filename-regex` list — no unit tests are
required or expected here (see AGENTS.md's testing section); it's exercised by the
manual smoke test in Task 16.

- [ ] **Step 1: Add `execute_fix` to `src/agent_process.rs`**

Insert after `ask(...)`'s closing brace, before `maybe_diagnose_alert_async` (which
stays in place until Task 9):

```rust
/// Spawn `uv run vigil-agent execute --plan-json <json>` and return its
/// stdout (trimmed) or an error — same shape as `ask`, for the same
/// reason: this does real, costly work (a real Claude Agent SDK session,
/// this time with some Bash patterns unlocked per `plan_json`'s
/// categories) that a unit test shouldn't trigger, hence this file's
/// coverage exclusion.
pub fn execute_fix(plan_json: &str, agent_dir: &str) -> Result<String, String> {
    let args = crate::agent::build_execute_args(plan_json, agent_dir);
    let output = std::process::Command::new(&args[0]).args(&args[1..]).output();
    let output = output.map_err(|e| format!("failed to launch vigil-agent (is `uv` installed?): {e}"))?;
    crate::agent::interpret_output(&output)
}
```

- [ ] **Step 2: Confirm the crate still builds**

Run: `cargo build`
Expected: builds cleanly (warnings about unused `execute_fix` are fine at this
point — nothing calls it yet).

- [ ] **Step 3: Commit**

```bash
git checkout -b fix-workflow-execute-fix-spawn
git add src/agent_process.rs
git commit -m "Add agent_process::execute_fix process spawn"
git push -u origin fix-workflow-execute-fix-spawn
gh pr create --title "Add agent_process::execute_fix" --body "Part of the fix-execution workflow (Task 4). Nothing calls this yet — wired up in Task 7."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 5: `investigate.rs` — resolve an alert key to its incident file

**Files:**
- Create: `src/investigate.rs`
- Modify: `src/main.rs` (add `mod investigate;` to the alphabetized module list)

**Interfaces:**
- Produces: `pub fn resolve_incident_file(dir: &Path, alert_key: &str) ->
  Result<PathBuf, String>`. Consumed by `investigate_process.rs` (Task 6).
- Consumes: `crate::incidents::list` (existing), `crate::incidents::slugify`
  (widened to `pub(crate)` in Task 2).

- [ ] **Step 1: Write `src/investigate.rs` with its full test module**

```rust
//! Pure logic for `vigil investigate <alert-key>`: resolving the CLI's
//! alert-key argument back to the incident file `incidents::write_stub`
//! created for it. The actual snapshot-taking, agent spawn, and file
//! append happen in `investigate_process.rs` — this file has no IO beyond
//! `incidents::list` (already itself a thin, tested `read_dir` wrapper),
//! so it's the one part of the `vigil investigate` path that's fully
//! unit-tested.

use std::path::{Path, PathBuf};

/// The most recent incident file in `dir` whose filename matches
/// `alert_key`'s slug — same substring-match convention `vigil incidents
/// --show` already uses, but keyed by the alert key's normalized form
/// (`incidents::slugify`) rather than an arbitrary user-typed substring,
/// since `alert_key` comes verbatim from the CLI arg and needs the exact
/// same normalization `write_stub` applied when naming the file.
pub fn resolve_incident_file(dir: &Path, alert_key: &str) -> Result<PathBuf, String> {
    let files = crate::incidents::list(dir)?;
    let slug = crate::incidents::slugify(alert_key);
    files
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(&slug)))
        .last()
        .ok_or_else(|| format!("no incident found for alert key \"{alert_key}\" in {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("vigil-investigate-test-{}-{n}", std::process::id()));
        p
    }

    #[test]
    fn resolve_incident_file_finds_the_most_recent_match() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-08-09-01-00-00-cpu-hog-37489.md"), "x").unwrap();
        std::fs::write(dir.join("2026-08-09-02-00-00-cpu-hog-37489.md"), "x").unwrap();
        std::fs::write(dir.join("2026-08-09-01-30-00-high-load.md"), "x").unwrap();

        let found = resolve_incident_file(&dir, "cpu_hog:37489").unwrap();
        assert_eq!(found.file_name().unwrap().to_string_lossy(), "2026-08-09-02-00-00-cpu-hog-37489.md");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_incident_file_errors_when_nothing_matches() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-08-09-01-00-00-high-load.md"), "x").unwrap();

        let result = resolve_incident_file(&dir, "cpu_hog:99999");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cpu_hog:99999"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_incident_file_errors_for_a_missing_directory() {
        let dir = test_dir();
        let result = resolve_incident_file(&dir, "cpu_hog:1");
        assert!(result.is_err());
    }
}
```

- [ ] **Step 2: Add the module to `src/main.rs`**

Insert `mod investigate;` into the alphabetized `mod` list (after `incidents_cmd;`,
before `menubar;`).

- [ ] **Step 3: Run the tests**

Run: `cargo test investigate::`
Expected: all 3 tests PASS.

- [ ] **Step 4: Commit**

```bash
git checkout -b fix-workflow-investigate-resolve
git add src/investigate.rs src/main.rs
git commit -m "Add investigate.rs: resolve an alert key to its incident file"
git push -u origin fix-workflow-investigate-resolve
gh pr create --title "Add investigate.rs alert-key resolution" --body "Part of the fix-execution workflow (Task 5)."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 6: `investigate_process.rs` — the `vigil investigate` orchestrator

**Files:**
- Create: `src/investigate_process.rs`
- Modify: `src/main.rs` (add `mod investigate_process;` to the alphabetized module
  list)

**Interfaces:**
- Produces: `pub fn run(alert_key: &str, agent_dir: &str, incidents_dir: &str,
  watch_log: Option<&str>) -> i32`. Consumed by `main.rs`'s dispatch (Task 8).
- Consumes: `crate::investigate::resolve_incident_file` (Task 5),
  `crate::incidents::extract_rule_message`/`append_diagnosis` (Task 2),
  `crate::agent::build_diagnosis_question` (existing, unchanged),
  `crate::agent_process::ask` (existing, unchanged), `crate::take_snapshot`
  (existing).

This is genuine OS-boundary glue (a real snapshot + a real, costly agent spawn) —
no unit tests here, same convention as `agent_process.rs`. It's exercised by the
manual smoke test in Task 16.

- [ ] **Step 1: Write `src/investigate_process.rs`**

```rust
//! The actual OS-boundary glue for `vigil investigate <alert-key>`:
//! resolving the incident file, taking a fresh snapshot, spawning the
//! agent, and appending its answer. Excluded from the coverage gate (see
//! AGENTS.md's testing section) for the same reason `agent_process.rs` is —
//! every branch here either spawns a real costly process or does file IO
//! whose failure modes are already covered where the pure logic lives
//! (`investigate.rs`, `incidents.rs`, `agent.rs`).

pub fn run(alert_key: &str, agent_dir: &str, incidents_dir: &str, watch_log: Option<&str>) -> i32 {
    let dir = std::path::Path::new(incidents_dir);
    let path = match crate::investigate::resolve_incident_file(dir, alert_key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[vigil] {e}");
            return 1;
        }
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vigil] failed to read {}: {e}", path.display());
            return 1;
        }
    };
    let rule_message = crate::incidents::extract_rule_message(&content).unwrap_or("(unknown alert)");

    let mut sys = sysinfo::System::new_all();
    let snap = crate::take_snapshot(&mut sys, 10);
    let snapshot_json = serde_json::to_string(&snap).unwrap();

    let question = crate::agent::build_diagnosis_question(rule_message, None, watch_log, None);

    match crate::agent_process::ask(&question, &snapshot_json, agent_dir) {
        Ok(answer) => match crate::incidents::append_diagnosis(&path, &answer) {
            Ok(_) => {
                println!("{answer}");
                println!("\nFull report: {}", path.display());
                0
            }
            Err(e) => {
                eprintln!("[vigil] investigation succeeded but failed to save it: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("[vigil] investigation failed: {e}");
            1
        }
    }
}
```

- [ ] **Step 2: Add the module to `src/main.rs`**

Insert `mod investigate_process;` into the alphabetized `mod` list (right after
`mod investigate;`).

- [ ] **Step 3: Confirm the crate still builds**

Run: `cargo build`
Expected: builds cleanly (unused-function warnings are fine — nothing calls `run`
yet until Task 8).

- [ ] **Step 4: Commit**

```bash
git checkout -b fix-workflow-investigate-process
git add src/investigate_process.rs src/main.rs
git commit -m "Add investigate_process.rs: the vigil investigate orchestrator"
git push -u origin fix-workflow-investigate-process
gh pr create --title "Add investigate_process.rs" --body "Part of the fix-execution workflow (Task 6). Not yet wired to the CLI — Task 8."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 7: `fix_process.rs` — the `vigil fix` orchestrator

**Files:**
- Create: `src/fix_process.rs`
- Modify: `src/main.rs` (add `mod fix_process;` to the alphabetized module list)

**Interfaces:**
- Produces: `pub fn run(incident_file: &str, agent_dir: &str) -> i32`. Consumed by
  `main.rs`'s dispatch (Task 8).
- Consumes: `crate::fixplan::{extract_proposed_fix_json, parse_plan,
  approved_steps_json, approved_header}` (Task 1), `crate::agent_process::execute_fix`
  (Task 4), `crate::incidents::{append_fix_execution, human_timestamp}` (Task 2).

Genuine OS-boundary glue (stdin prompting + a real, costly agent spawn) — no unit
tests here, same convention as `agent_process.rs`. Exercised by the manual smoke
test in Task 16.

- [ ] **Step 1: Write `src/fix_process.rs`**

```rust
//! The actual OS-boundary glue for `vigil fix <incident-file>`: parsing
//! the proposed plan out of an incident file, prompting for per-step
//! approval on stdin, spawning the scoped execute-agent, and appending its
//! report. Excluded from the coverage gate for the same reason
//! `agent_process.rs`/`investigate_process.rs` are — the pure plan
//! parsing/formatting this leans on is fully tested in `fixplan.rs`.

use std::io::{self, BufRead, Write};

pub fn run(incident_file: &str, agent_dir: &str) -> i32 {
    let path = std::path::Path::new(incident_file);
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vigil] failed to read {}: {e}", path.display());
            return 1;
        }
    };

    let Some(json) = crate::fixplan::extract_proposed_fix_json(&content) else {
        eprintln!(
            "[vigil] {} has no proposed fix — run `vigil investigate` first, or this incident didn't produce one",
            path.display()
        );
        return 1;
    };
    let plan = match crate::fixplan::parse_plan(json) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[vigil] {e}");
            return 1;
        }
    };

    let stdin = io::stdin();
    let mut approved = Vec::with_capacity(plan.plan.len());
    for (i, step) in plan.plan.iter().enumerate() {
        print!(
            "[{}/{}] {} — {}\n    target: {}\nApprove? [y/N] ",
            i + 1,
            plan.plan.len(),
            step.category.as_str(),
            step.description,
            step.target_hint
        );
        io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).is_err() {
            line.clear();
        }
        let answer = line.trim().eq_ignore_ascii_case("y") || line.trim().eq_ignore_ascii_case("yes");
        approved.push(answer);
    }

    if !approved.iter().any(|&a| a) {
        println!("No steps approved, nothing to execute.");
        return 0;
    }

    let plan_json = crate::fixplan::approved_steps_json(&plan.plan, &approved);
    match crate::agent_process::execute_fix(&plan_json, agent_dir) {
        Ok(report) => {
            let header = crate::fixplan::approved_header(&approved, &crate::incidents::human_timestamp());
            let body = format!("{header}\n\n{report}");
            match crate::incidents::append_fix_execution(path, &body) {
                Ok(_) => {
                    println!("{report}");
                    println!("\nFix execution appended to {}", path.display());
                    0
                }
                Err(e) => {
                    eprintln!("[vigil] fix ran but failed to save the report: {e}");
                    1
                }
            }
        }
        Err(e) => {
            eprintln!("[vigil] fix execution failed: {e}");
            1
        }
    }
}
```

- [ ] **Step 2: Add the module to `src/main.rs`**

Insert `mod fix_process;` into the alphabetized `mod` list (right after `mod
cli;`, before `mod fixplan;` — alphabetically `fix_process` < `fixplan`).

- [ ] **Step 3: Confirm the crate still builds**

Run: `cargo build`
Expected: builds cleanly.

- [ ] **Step 4: Commit**

```bash
git checkout -b fix-workflow-fix-process
git add src/fix_process.rs src/main.rs
git commit -m "Add fix_process.rs: the vigil fix orchestrator"
git push -u origin fix-workflow-fix-process
gh pr create --title "Add fix_process.rs" --body "Part of the fix-execution workflow (Task 7). Not yet wired to the CLI — Task 8."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 8: Wire `Investigate`/`Fix` into `cli.rs` and `main.rs`

**Files:**
- Modify: `src/cli.rs`
- Modify: `src/main.rs`

**Interfaces:**
- Produces: `Commands::Investigate { alert_key: String, agent_dir: String,
  incidents_dir: String, watch_log: Option<String> }`, `Commands::Fix {
  incident_file: String, agent_dir: String }` variants.
- Consumes: `investigate_process::run` (Task 6), `fix_process::run` (Task 7).

- [ ] **Step 1: Add the two subcommand variants to `src/cli.rs`**

Insert into the `Commands` enum, after the `Incidents { ... }` variant and before
`Menubar { ... }`:

```rust
    /// Investigate an alert: runs the read-only diagnosis agent against
    /// the incident it fired, appending a `## Agent diagnosis` section
    /// (and, if the agent identifies one, a `## Proposed fix`)
    Investigate {
        /// Alert key to investigate, e.g. `cpu_hog:37489` (shown in the notification)
        alert_key: String,
        /// Path to the vigil_agent project directory
        #[arg(long, default_value = "agent")]
        agent_dir: String,
        /// Directory the incident journal is stored in
        #[arg(long, default_value_t = default_incidents_dir())]
        incidents_dir: String,
        /// Optional path to a persistent watch.jsonl history for trend context
        #[arg(long)]
        watch_log: Option<String>,
    },
    /// Execute a fix plan an earlier `vigil investigate` proposed, after
    /// interactive per-step approval
    Fix {
        /// Path to an incident file containing a `## Proposed fix` block
        incident_file: String,
        /// Path to the vigil_agent project directory
        #[arg(long, default_value = "agent")]
        agent_dir: String,
    },
```

Add these tests to `cli.rs`'s existing `#[cfg(test)] mod tests` block:

```rust
    #[test]
    fn investigate_parses_the_alert_key_positional_argument() {
        let cli = Cli::try_parse_from(["vigil", "investigate", "cpu_hog:37489"]).unwrap();
        match cli.command {
            Commands::Investigate { alert_key, .. } => assert_eq!(alert_key, "cpu_hog:37489"),
            _ => panic!("expected Investigate"),
        }
    }

    #[test]
    fn fix_parses_the_incident_file_positional_argument() {
        let cli = Cli::try_parse_from(["vigil", "fix", "/tmp/some-incident.md"]).unwrap();
        match cli.command {
            Commands::Fix { incident_file, .. } => assert_eq!(incident_file, "/tmp/some-incident.md"),
            _ => panic!("expected Fix"),
        }
    }
```

- [ ] **Step 2: Wire dispatch in `src/main.rs`**

Insert into the `match cli.command { ... }` block, after the `Commands::Incidents
{ ... }` arm and before `Commands::Menubar { ... }`:

```rust
        Commands::Investigate { alert_key, agent_dir, incidents_dir, watch_log } => {
            std::process::exit(investigate_process::run(&alert_key, &agent_dir, &incidents_dir, watch_log.as_deref()));
        }
        Commands::Fix { incident_file, agent_dir } => {
            std::process::exit(fix_process::run(&incident_file, &agent_dir));
        }
```

- [ ] **Step 3: Run the tests**

Run: `cargo test cli::`
Expected: all `cli::tests` PASS, including the 2 new ones.

Run: `cargo build`
Expected: builds cleanly with no unused-function warnings left for
`investigate_process::run`/`fix_process::run` (both now called from `main.rs`).

- [ ] **Step 4: Commit**

```bash
git checkout -b fix-workflow-cli-wiring
git add src/cli.rs src/main.rs
git commit -m "Wire vigil investigate/fix subcommands into the CLI"
git push -u origin fix-workflow-cli-wiring
gh pr create --title "Wire investigate/fix subcommands into cli.rs + main.rs" --body "Part of the fix-execution workflow (Task 8). vigil investigate and vigil fix are now runnable; the alert-fired path still auto-diagnoses until Task 9's cutover."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 9: Cutover — remove auto-diagnosis, wire stub+notify into `watch`/`ui`

This is the one task where old and new code can't coexist: `watch.rs` and
`ui_loop.rs` both call `agent::maybe_diagnose_alert_async`, so removing it and
switching those two call sites happen together, in one commit.

**Files:**
- Modify: `src/watch.rs`
- Modify: `src/ui_loop.rs`
- Modify: `src/agent.rs` (delete `is_auto_diagnose_worthy` + its tests; update the
  `agent_process` re-export; update stale doc-comment references)
- Modify: `src/agent_process.rs` (delete `maybe_diagnose_alert_async`; update the
  module doc comment)
- Modify: `src/alerts.rs` (delete `RecentAlerts` + its tests + the two
  `RECENT_ALERTS_*` constants; update `IncidentTracker`'s doc comment)
- Modify: `src/incidents.rs` (delete `Incident` + `record()` + their 4 tests — fully
  superseded by `write_stub`/`append_diagnosis` from Task 2)

**Interfaces:**
- Consumes everything added in Tasks 1-4 (`incidents::write_stub`,
  `incidents::IncidentStub`, `agent::augment_with_investigate_hint`).
- Removes: `agent::is_auto_diagnose_worthy`, `agent_process::maybe_diagnose_alert_async`,
  `alerts::RecentAlerts`, `incidents::Incident`, `incidents::record`.

- [ ] **Step 1: Replace `watch.rs`'s alert-fired block**

In `src/watch.rs`, remove the line `let mut recent_alerts =
crate::alerts::RecentAlerts::new();` (near the top of `run()`, alongside the other
`let mut ...` state declarations).

Replace this block (inside `if !args.no_notify { ... }`):

```rust
            let mut fired = crate::alerts::evaluate(&snap, cpu_count, &mut alert_state, cooldown, now);
            fired.extend(crate::alerts::evaluate_battery(&snap, battery_eta, &mut alert_state, cooldown, now));
            for alert in &fired {
                recent_alerts.record(&alert.key, &alert.message, now);
            }
            for alert in fired {
                eprintln!("[vigil] ALERT [{}] {}", alert.key, alert.message);
                if incident_tracker.is_new_incident(alert.target.as_deref(), incident_timeout, now) {
                    crate::alerts::notify(&alert);
                    let context = recent_alerts.context_excluding(&alert.key, now);
                    crate::agent::maybe_diagnose_alert_async(
                        &alert,
                        &line,
                        &args.agent_dir,
                        &args.incidents_dir,
                        context.as_deref(),
                        watch_log_path.as_deref(),
                    );
                } else {
                    eprintln!(
                        "[vigil] [{}] continuing open incident for {:?} — notification/diagnosis suppressed",
                        alert.key, alert.target
                    );
                }
            }
```

with:

```rust
            let mut fired = crate::alerts::evaluate(&snap, cpu_count, &mut alert_state, cooldown, now);
            fired.extend(crate::alerts::evaluate_battery(&snap, battery_eta, &mut alert_state, cooldown, now));
            for alert in fired {
                eprintln!("[vigil] ALERT [{}] {}", alert.key, alert.message);
                if incident_tracker.is_new_incident(alert.target.as_deref(), incident_timeout, now) {
                    let incidents_dir = std::path::Path::new(&args.incidents_dir);
                    let stub = crate::incidents::IncidentStub {
                        alert_key: &alert.key,
                        alert_title: &alert.title,
                        alert_message: &alert.message,
                    };
                    if let Err(e) = crate::incidents::write_stub(incidents_dir, &stub) {
                        eprintln!("[vigil] failed to write incident stub: {e}");
                    }
                    crate::alerts::notify(&crate::agent::augment_with_investigate_hint(&alert, watch_log_path.as_deref()));
                } else {
                    eprintln!(
                        "[vigil] [{}] continuing open incident for {:?} — notification suppressed",
                        alert.key, alert.target
                    );
                }
            }
```

- [ ] **Step 2: Replace `ui_loop.rs`'s alert-fired block**

In `src/ui_loop.rs`, remove the line `let mut recent_alerts =
crate::alerts::RecentAlerts::new();` (near the top of `run()`).

Replace this block:

```rust
                let snap = crate::take_snapshot(&mut sys, opts.top_n);
                let snapshot_json = serde_json::to_string(&snap).unwrap_or_default();

                battery_trend.record(
                    snap.battery.as_ref().and_then(|b| b.charging),
                    snap.battery.as_ref().and_then(|b| b.percentage),
                    now,
                );
                let battery_eta = battery_trend.eta();
                app.battery_pct = snap.battery.as_ref().and_then(|b| b.percentage);
                app.battery_charging = snap.battery.as_ref().and_then(|b| b.charging);
                app.battery_eta_secs = battery_eta.map(|d| d.as_secs());

                let mut fired = crate::alerts::evaluate(&snap, cpu_count, &mut alert_state, opts.cooldown, now);
                fired.extend(crate::alerts::evaluate_battery(&snap, battery_eta, &mut alert_state, opts.cooldown, now));
                for alert in &fired {
                    recent_alerts.record(&alert.key, &alert.message, now);
                }
                for alert in fired {
                    app.push_alert(format!("[{}] {}", alert.key, alert.message));
                    if incident_tracker.is_new_incident(alert.target.as_deref(), incident_timeout, now) {
                        crate::alerts::notify(&alert);
                        let context = recent_alerts.context_excluding(&alert.key, now);
                        crate::agent::maybe_diagnose_alert_async(
                            &alert,
                            &snapshot_json,
                            &opts.agent_dir,
                            &opts.incidents_dir,
                            context.as_deref(),
                            None, // vigil ui's own snapshot loop doesn't write a persistent JSONL log
                        );
                    }
                }
```

with:

```rust
                let snap = crate::take_snapshot(&mut sys, opts.top_n);

                battery_trend.record(
                    snap.battery.as_ref().and_then(|b| b.charging),
                    snap.battery.as_ref().and_then(|b| b.percentage),
                    now,
                );
                let battery_eta = battery_trend.eta();
                app.battery_pct = snap.battery.as_ref().and_then(|b| b.percentage);
                app.battery_charging = snap.battery.as_ref().and_then(|b| b.charging);
                app.battery_eta_secs = battery_eta.map(|d| d.as_secs());

                let mut fired = crate::alerts::evaluate(&snap, cpu_count, &mut alert_state, opts.cooldown, now);
                fired.extend(crate::alerts::evaluate_battery(&snap, battery_eta, &mut alert_state, opts.cooldown, now));
                for alert in fired {
                    app.push_alert(format!("[{}] {}", alert.key, alert.message));
                    if incident_tracker.is_new_incident(alert.target.as_deref(), incident_timeout, now) {
                        let incidents_dir = std::path::Path::new(&opts.incidents_dir);
                        let stub = crate::incidents::IncidentStub {
                            alert_key: &alert.key,
                            alert_title: &alert.title,
                            alert_message: &alert.message,
                        };
                        if let Err(e) = crate::incidents::write_stub(incidents_dir, &stub) {
                            eprintln!("[vigil] failed to write incident stub: {e}");
                        }
                        // vigil ui's own snapshot loop doesn't write a persistent JSONL log
                        crate::alerts::notify(&crate::agent::augment_with_investigate_hint(&alert, None));
                    }
                }
```

- [ ] **Step 3: Delete `is_auto_diagnose_worthy` and update stale references in
  `src/agent.rs`**

Delete the function:

```rust
/// Alert keys worth an automatic agent diagnosis: ...
pub(crate) fn is_auto_diagnose_worthy(alert_key: &str) -> bool {
    alert_key == "high_load"
        || alert_key.starts_with("cpu_hog:")
        || alert_key == "battery_low"
        || alert_key.starts_with("high_process_count:")
}
```

Delete its two tests, `auto_diagnose_worthy_for_cpu_and_battery_alerts` and
`auto_diagnose_not_worthy_for_disk_and_memory_alerts`.

Change the re-export line:

```rust
pub use crate::agent_process::{ask, maybe_diagnose_alert_async};
```

to:

```rust
pub use crate::agent_process::{ask, execute_fix};
```

Update `build_diagnosis_question`'s doc comment — change "Pure — kept separate from
`maybe_diagnose_alert_async`'s side effects so" to "Pure — kept separate from
`investigate_process::run`'s side effects so".

Update `temp_snapshot_path`'s doc comment — change:

```rust
/// Includes a nanosecond timestamp, not just the PID, because background
/// diagnosis threads (see `maybe_diagnose_alert_async`) can call `ask()`
/// concurrently within the same process — a PID-only name would race.
```

to:

```rust
/// Includes a nanosecond timestamp, not just the PID, as a defensive
/// uniqueness guarantee in case multiple invocations ever overlap — cheap
/// to keep even though nothing in the current codebase actually calls
/// `ask()` concurrently within one process.
```

Update the `temp_snapshot_path_is_unique_across_concurrent_calls` test's comment —
change:

```rust
    #[test]
    fn temp_snapshot_path_is_unique_across_concurrent_calls() {
        // Guards against the race a PID-only filename would have with
        // `maybe_diagnose_alert_async` spawning concurrent `ask()` calls.
```

to:

```rust
    #[test]
    fn temp_snapshot_path_is_unique_across_concurrent_calls() {
        // Guards against a PID-only filename colliding if `ask()` is ever
        // called twice in quick succession within one process (nothing
        // currently does this, but the guarantee is cheap to keep).
```

Update the file's own top doc comment — change "The actual process-spawning/
thread-spawning glue lives in `agent_process.rs`" to "The actual process-spawning
glue lives in `agent_process.rs`" (no more background-thread spawning after this
task).

- [ ] **Step 4: Delete `maybe_diagnose_alert_async` and update the module doc
  comment in `src/agent_process.rs`**

Delete the whole function:

```rust
/// If `alert` is worth it, ask the agent to investigate in a background
/// thread and fire a follow-up notification with the answer once ready.
/// ...
pub fn maybe_diagnose_alert_async(
    alert: &crate::alerts::Alert,
    snapshot_json: &str,
    agent_dir: &str,
    incidents_dir: &str,
    recent_context: Option<&str>,
    watch_log_path: Option<&str>,
) {
    ...
}
```

Replace the module doc comment at the top of the file with:

```rust
//! The actual OS-boundary glue for talking to `vigil_agent`: writing the
//! snapshot to a temp file and spawning `uv run vigil-agent ask`
//! (investigation, via `ask`) or `uv run vigil-agent execute` (an approved
//! fix plan, via `execute_fix`). Kept in its own file, excluded from the
//! coverage gate (see AGENTS.md's testing section and
//! `--ignore-filename-regex` in the test command) — every branch here
//! either spawns a real process that costs real tokens or blocks on I/O,
//! neither of which a unit test should trigger. The actual decision logic
//! (question wording, arg construction, output interpretation) lives in
//! `agent.rs` and is fully unit-tested there; this file is intentionally
//! as thin as possible around it.

use crate::agent::{build_args, interpret_output, temp_snapshot_path};
use std::path::PathBuf;
```

(The `use` line drops `build_diagnosis_question` and `is_auto_diagnose_worthy` —
neither is referenced in this file anymore — and drops `teaser`, which was also
only used by the now-deleted function; the `std::path::PathBuf` import may also
become unused — remove it too if `cargo build` warns it's unused after this edit.)

- [ ] **Step 5: Delete `RecentAlerts` and update `IncidentTracker`'s doc comment in
  `src/alerts.rs`**

Delete the two constants:

```rust
const RECENT_ALERTS_WINDOW: Duration = Duration::from_secs(5 * 60);
const RECENT_ALERTS_CAPACITY: usize = 10;
```

Delete the whole `RecentAlerts` struct, its doc comment, and its `impl` block (from
the `/// A short rolling log ...` doc comment through the closing `}` of `impl
RecentAlerts`).

Delete its four tests: `recent_alerts_context_excludes_self_and_includes_others`,
`recent_alerts_context_is_none_when_nothing_else_fired`,
`recent_alerts_context_drops_entries_outside_the_window`,
`recent_alerts_caps_capacity_dropping_oldest_first`.

Update `IncidentTracker`'s doc comment — change:

```rust
/// Tracks whether an alert firing for a given target process is the start
/// of a new incident or a continuation of one still open, so a process
/// pegged at high CPU for an hour produces one notification, one
/// background diagnosis, and one journal entry — not a fresh one on every
/// re-fire of the underlying rule (which, per-rule, only needs its own
/// `cooldown` to elapse to fire again). An incident is "still open" as
/// long as its target keeps firing within `timeout` of its last firing;
/// once a target goes quiet for `timeout`, the next firing starts a new
/// incident. Targetless alerts (`target: None` — e.g. `low_disk:<mount>`,
/// `high_connection_count`) have nothing to key on and are always "new".
///
/// Replaces an earlier, narrower `agent::DiagnosisCoalescer` that only
/// deduped near-simultaneous (120s) diagnoses. Real repeats of the same
/// incident in the field arrived 5-13 minutes apart — gated by each rule's
/// own re-fire cooldown, not by anything a 120s window could ever catch —
/// so that mechanism never actually engaged. This subsumes it and also
/// covers the native notification and the journal write, not just the
/// diagnosis, since all three were spamming for the same reason.
```

to:

```rust
/// Tracks whether an alert firing for a given target process is the start
/// of a new incident or a continuation of one still open, so a process
/// pegged at high CPU for an hour produces one notification and one
/// incident stub — not a fresh one on every re-fire of the underlying rule
/// (which, per-rule, only needs its own `cooldown` to elapse to fire
/// again). An incident is "still open" as long as its target keeps firing
/// within `timeout` of its last firing; once a target goes quiet for
/// `timeout`, the next firing starts a new incident. Targetless alerts
/// (`target: None` — e.g. `low_disk:<mount>`, `high_connection_count`)
/// have nothing to key on and are always "new".
///
/// Replaces an earlier, narrower `agent::DiagnosisCoalescer` that only
/// deduped near-simultaneous (120s) diagnoses. Real repeats of the same
/// incident in the field arrived 5-13 minutes apart — gated by each rule's
/// own re-fire cooldown, not by anything a 120s window could ever catch —
/// so that mechanism never actually engaged. This subsumes it and also
/// covers the native notification and the incident stub write together,
/// not just one of them, since both were spamming for the same reason.
/// (Investigation itself is opt-in now, via `vigil investigate` — this
/// tracker has no opinion on when or whether that happens.)
```

- [ ] **Step 6: Delete `Incident`/`record` and their tests in `src/incidents.rs`**

Delete the whole `pub struct Incident<'a> { ... }` block and the whole `pub fn
record(dir: &Path, incident: &Incident) -> Result<PathBuf, String> { ... }`
function (fully superseded by `IncidentStub`/`write_stub` from Task 2).

Delete `render_markdown` (only ever called by `record`):

```rust
fn render_markdown(incident: &Incident) -> String {
    format!(
        "# {}\n\n**Alert key:** `{}`\n\n**Rule message:** {}\n\n## Agent diagnosis\n\n{}\n",
        incident.alert_title, incident.alert_key, incident.alert_message, incident.diagnosis
    )
}
```

Delete its four tests: `record_writes_markdown_with_expected_content_and_filename`,
`record_creates_missing_directory`,
`record_fails_when_the_incidents_dir_cannot_be_created`,
`record_fails_when_the_file_cannot_be_created`.

- [ ] **Step 7: Run the full test suite**

Run: `cargo build 2>&1 | grep -i warning` — expect no output (no unused-import or
dead-code warnings left from the deletions above; if there are, remove the
now-unused import/binding they point at).

Run: `cargo test`
Expected: every remaining test PASSES. The removed tests (4 `record_*`, 2
`auto_diagnose_worthy_*`, 4 `recent_alerts_*`) are gone, not failing.

- [ ] **Step 8: Run the coverage gate with the not-yet-updated regex plus the two
  new glue files**

Run: `cargo llvm-cov --workspace --ignore-filename-regex
'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs'
--fail-under-lines 99.5 --fail-under-regions 98`
Expected: PASSES. (AGENTS.md itself is updated to state this exact command in Task
13 — running it here first confirms the code is actually ready for that doc
change, not just hoped to be.)

- [ ] **Step 9: Commit**

```bash
git checkout -b fix-workflow-cutover
git add src/watch.rs src/ui_loop.rs src/agent.rs src/agent_process.rs src/alerts.rs src/incidents.rs
git commit -m "Cut over: alerts write a stub + investigate hint, not an auto-diagnosis"
git push -u origin fix-workflow-cutover
gh pr create --title "Cut over to opt-in investigate (remove auto-diagnosis)" --body "Part of the fix-execution workflow (Task 9). watch.rs and ui_loop.rs now write an incident stub and notify with the vigil investigate command instead of auto-triggering a diagnosis. Removes agent::is_auto_diagnose_worthy, agent_process::maybe_diagnose_alert_async, alerts::RecentAlerts, and incidents::record/Incident (all fully superseded)."
gh pr merge --squash --delete-branch
git checkout master && git pull
cargo build --release
```

After merging, restart any running `vigil watch`/`vigil menubar` background
processes from the rebuilt `master` (kill the old PIDs, relaunch with `nohup`, per
AGENTS.md's Git workflow section) — this task changes their runtime behavior.

---

## Task 10: Python — `## Proposed fix` prompt instructions + `EXECUTE_SYSTEM_PROMPT`

**Files:**
- Modify: `agent/src/vigil_agent/prompts.py`
- Modify: `agent/tests/test_prompts.py`

**Interfaces:**
- Produces: `EXECUTE_SYSTEM_PROMPT: str` (new module-level constant in `prompts.py`).
  `SYSTEM_PROMPT` gains an additional hard-rule bullet describing the `## Proposed
  fix` schema.
- Consumes: nothing new.

- [ ] **Step 1: Add the `## Proposed fix` instruction to `SYSTEM_PROMPT`**

In `agent/src/vigil_agent/prompts.py`, insert a new bullet into `SYSTEM_PROMPT`'s
hard-rules list, immediately after the existing "You can only advise, never fix
anything yourself..." bullet and before the closing `"""`:

```python
- You can only advise, never fix anything yourself. If a suggestion implies \
  a potentially risky action (killing a process, freeing up space, deleting \
  files), say explicitly that it needs the user's confirmation first.
- If, and only if, you're confident about a specific, narrowly-scoped, low-risk \
  fix for what you diagnosed, append a `## Proposed fix` section after your \
  diagnosis, containing a fenced ```json code block with this exact shape: \
  {"plan": [{"category": "kill_process", "description": "...", "target_hint": \
  "..."}]}. Valid `category` values: `kill_process` (killing one confirmed-stale \
  process), `delete_path` (deleting/moving one specific orphaned file or \
  directory), `system_setting` (one `defaults write/delete` or `launchctl \
  unload/bootout/remove` change). `description` is shown to the user for approval \
  and later becomes the execute-agent's literal instruction for that step — be \
  specific about what and why. `target_hint` is the pid/path/setting key you \
  observed right now; a later execute-agent will re-verify it before acting, \
  since it may be stale by then. Most diagnoses should NOT include this section — \
  a fix spanning multiple root causes, or one you're not fully confident \
  identifies the actual culprit, doesn't belong here; say so in your suggestions \
  instead and leave this section out entirely.
"""
```

(Only the two bullets and the closing `"""` are shown above for placement context
— the existing bullets before "You can only advise..." are unchanged.)

- [ ] **Step 2: Add `EXECUTE_SYSTEM_PROMPT`**

Append to the end of `agent/src/vigil_agent/prompts.py`, after `build_prompt`:

```python
EXECUTE_SYSTEM_PROMPT = """\
You are vigil's fix-execution agent. You are given a short, pre-approved list of \
steps a user has already explicitly approved, each with a category, a description \
of what to do, and a target_hint identifying what to act on (a pid, a path, a \
setting key) captured when the plan was proposed — not necessarily still accurate \
now.

For each step, in order:
1. Re-verify target_hint's current state before acting (e.g. re-run `ps -p <pid>` \
   and compare the full command line, or check a path still exists and still looks \
   like what the description says). If it doesn't match what the step expects, \
   STOP — do not guess or improvise a substitute target.
2. If verification passes, carry out exactly what the step's category and \
   description say, nothing more. Only the Bash patterns this session's tool \
   config unlocks are available to you — if something you'd want to run is \
   blocked, that means it's out of scope for this step, not a tool failure to work \
   around.
3. If a step fails verification or fails to execute, STOP — do not attempt any \
   remaining steps. A plan is a sequence that assumed a certain system state; once \
   that assumption breaks, continuing on stale assumptions is worse than stopping.

Report back numbered to match the plan, one line per step: `done` with a short \
confirmation of what you verified, or `aborted` with why. Be concise — this becomes \
part of a permanent incident log, not a conversation.
"""
```

- [ ] **Step 3: Add tests to `agent/tests/test_prompts.py`**

```python
def test_system_prompt_documents_the_proposed_fix_json_schema():
    assert "## Proposed fix" in SYSTEM_PROMPT
    for category in ("kill_process", "delete_path", "system_setting"):
        assert category in SYSTEM_PROMPT
    assert '"plan"' in SYSTEM_PROMPT


def test_execute_system_prompt_requires_reverification_and_abort_on_failure():
    lowered = EXECUTE_SYSTEM_PROMPT.lower()
    assert "re-verify" in lowered
    assert "stop" in lowered
    assert "target_hint" in EXECUTE_SYSTEM_PROMPT
```

Update the test file's import line at the top:

```python
from vigil_agent.prompts import EXECUTE_SYSTEM_PROMPT, SYSTEM_PROMPT, build_prompt
```

- [ ] **Step 4: Run the tests**

Run: `cd agent && uv run pytest tests/test_prompts.py -v`
Expected: all tests PASS, including the 2 new ones.

Run: `cd agent && uv run pytest`
Expected: full suite PASSES, coverage still ≥99.9%.

- [ ] **Step 5: Commit**

```bash
git checkout -b fix-workflow-prompts
git add agent/src/vigil_agent/prompts.py agent/tests/test_prompts.py
git commit -m "Add Proposed-fix plan instructions and EXECUTE_SYSTEM_PROMPT"
git push -u origin fix-workflow-prompts
gh pr create --title "Add Proposed-fix instructions + EXECUTE_SYSTEM_PROMPT" --body "Part of the fix-execution workflow (Task 10). Nothing calls EXECUTE_SYSTEM_PROMPT yet — Task 11."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 11: Python — `execute.py` and its safety-rail tests

**Files:**
- Create: `agent/src/vigil_agent/execute.py`
- Create: `agent/tests/test_execute_config.py`
- Modify: `agent/pyproject.toml` (add `async def execute\\(` to `exclude_also`)

**Interfaces:**
- Produces: `ALLOWED_TOOLS: list[str]`, `HARD_FLOOR_DISALLOWED_TOOLS: list[str]`,
  `CATEGORY_UNLOCKS: dict[str, list[str]]`, `def build_categories(plan: list[dict])
  -> set[str]`, `def disallowed_tools_for(categories: set[str]) -> list[str]`, `def
  build_instruction(plan: list[dict]) -> str`, `async def execute(plan: list[dict])
  -> str`. Consumed by `cli.py` (Task 12).
- Consumes: `vigil_agent.diagnose.format_usage_footer` (existing),
  `vigil_agent.prompts.EXECUTE_SYSTEM_PROMPT` (Task 10).

- [ ] **Step 1: Write `agent/src/vigil_agent/execute.py`**

```python
"""Executes a small, pre-approved fix plan with narrowly, per-plan-scoped tool access.

Deliberately a separate module from `diagnose.py` rather than a shared config
with extra parameters -- the two configs' allowed blast radius is fundamentally
different (read-only investigation vs. a handful of unlocked destructive Bash
patterns), and conflating them risks a future edit to one accidentally loosening
the other. See docs/superpowers/specs/2026-08-09-fix-execution-workflow-design.md.
"""

from __future__ import annotations

from typing import Any

from claude_agent_sdk import (
    AssistantMessage,
    ClaudeAgentOptions,
    ResultMessage,
    TextBlock,
    query,
)

from .diagnose import format_usage_footer
from .prompts import EXECUTE_SYSTEM_PROMPT

# Same allowed set as investigation (diagnose.ALLOWED_TOOLS) -- the
# execute-agent re-verifies a step's target the same way an investigation
# checks a hypothesis (Bash/Read/Grep/Glob), before acting through whichever
# Bash patterns its plan's categories unlocked.
ALLOWED_TOOLS = ["Bash", "Read", "Grep", "Glob"]

# Blocked unconditionally, regardless of what's approved -- these fall
# outside all three fix categories below and there is no path to unlocking
# them via a plan. Write/Edit/NotebookEdit too: the execute-agent acts only
# through the unlocked Bash patterns, never by writing/editing files
# directly.
HARD_FLOOR_DISALLOWED_TOOLS = [
    "Write",
    "Edit",
    "NotebookEdit",
    "Bash(sudo *)",
    "Bash(su *)",
    "Bash(dd *)",
    "Bash(diskutil erase*)",
    "Bash(diskutil partition*)",
    "Bash(diskutil eraseVolume*)",
    "Bash(shutdown *)",
    "Bash(reboot *)",
    "Bash(halt *)",
    "Bash(chmod *)",
    "Bash(chown *)",
]

# Bash patterns each fix category unlocks -- only the categories actually
# present in the approved plan get removed from DISALLOWED_TOOLS; every
# other category's patterns stay blocked for this session even though
# they're not on the hard floor above.
CATEGORY_UNLOCKS: dict[str, list[str]] = {
    "kill_process": ["Bash(kill *)", "Bash(killall *)", "Bash(pkill *)"],
    "delete_path": ["Bash(rm *)", "Bash(rmdir *)", "Bash(mv *)"],
    "system_setting": [
        "Bash(defaults write*)",
        "Bash(defaults delete*)",
        "Bash(launchctl unload*)",
        "Bash(launchctl bootout*)",
        "Bash(launchctl remove*)",
    ],
}

MAX_EXECUTION_TURNS = 10


def build_categories(plan: list[dict[str, Any]]) -> set[str]:
    """The distinct `category` values present in an approved plan."""
    return {step["category"] for step in plan}


def disallowed_tools_for(categories: set[str]) -> list[str]:
    """The hard floor, plus every category-specific pattern for a category
    NOT in `categories` -- so a plan is scoped to exactly what it approved,
    nothing wider. Raises `ValueError` on an unrecognized category rather
    than silently ignoring it: silently ignoring here would either mean
    silently leaving that category's patterns blocked (safe, but hides a
    bug) or, worse, silently accepting a typo'd category that was meant to
    unlock something -- fail loud instead.
    """
    unknown = categories - CATEGORY_UNLOCKS.keys()
    if unknown:
        raise ValueError(f"unknown fix category/categories: {sorted(unknown)}")
    locked_patterns = [
        pattern
        for category, patterns in CATEGORY_UNLOCKS.items()
        if category not in categories
        for pattern in patterns
    ]
    return HARD_FLOOR_DISALLOWED_TOOLS + locked_patterns


def build_instruction(plan: list[dict[str, Any]]) -> str:
    """The literal prompt handed to the execute-agent: the approved steps,
    numbered, with nothing about any rejected step it should never see.
    """
    lines = [
        f"{i}. [{step['category']}] {step['description']} (target_hint: {step['target_hint']})"
        for i, step in enumerate(plan, start=1)
    ]
    return "Carry out this pre-approved plan:\n" + "\n".join(lines)


async def execute(plan: list[dict[str, Any]]) -> str:
    """Run the approved plan through a dedicated, narrowly-scoped agent
    session and return its report (steps done/aborted + a token footer).
    """
    categories = build_categories(plan)
    options = ClaudeAgentOptions(
        system_prompt=EXECUTE_SYSTEM_PROMPT,
        allowed_tools=ALLOWED_TOOLS,
        disallowed_tools=disallowed_tools_for(categories),
        max_turns=MAX_EXECUTION_TURNS,
    )

    chunks: list[str] = []
    usage: dict[str, Any] | None = None
    cost_usd: float | None = None
    async for message in query(prompt=build_instruction(plan), options=options):
        if isinstance(message, AssistantMessage):
            for block in message.content:
                if isinstance(block, TextBlock):
                    chunks.append(block.text)
        elif isinstance(message, ResultMessage):
            usage = message.usage
            cost_usd = message.total_cost_usd
            if message.subtype == "success" and message.result:
                return message.result.strip() + format_usage_footer(usage, cost_usd)

    answer = "".join(chunks).strip() or "The execute-agent returned no report."
    return answer + format_usage_footer(usage, cost_usd)
```

- [ ] **Step 2: Write `agent/tests/test_execute_config.py`**

```python
"""Guards the safety rails around the execute-agent's tool access -- same
spirit as test_diagnose_config.py, but for the narrower, per-plan-scoped
execute path.
"""

import pytest

from vigil_agent.execute import (
    ALLOWED_TOOLS,
    CATEGORY_UNLOCKS,
    HARD_FLOOR_DISALLOWED_TOOLS,
    build_categories,
    build_instruction,
    disallowed_tools_for,
)


def test_write_and_edit_are_always_blocked():
    assert "Write" in HARD_FLOOR_DISALLOWED_TOOLS
    assert "Edit" in HARD_FLOOR_DISALLOWED_TOOLS
    assert "NotebookEdit" in HARD_FLOOR_DISALLOWED_TOOLS


def test_hard_floor_is_present_regardless_of_approved_categories():
    disallowed = disallowed_tools_for({"kill_process", "delete_path", "system_setting"})
    for pattern in HARD_FLOOR_DISALLOWED_TOOLS:
        assert pattern in disallowed


def test_hard_floor_patterns_cannot_be_unlocked_by_any_category():
    hard_floor_set = set(HARD_FLOOR_DISALLOWED_TOOLS)
    for patterns in CATEGORY_UNLOCKS.values():
        assert hard_floor_set.isdisjoint(patterns)


def test_approving_kill_process_only_unlocks_kill_patterns():
    disallowed = disallowed_tools_for({"kill_process"})
    for pattern in CATEGORY_UNLOCKS["kill_process"]:
        assert pattern not in disallowed
    for pattern in CATEGORY_UNLOCKS["delete_path"]:
        assert pattern in disallowed
    for pattern in CATEGORY_UNLOCKS["system_setting"]:
        assert pattern in disallowed


def test_approving_no_categories_blocks_all_category_patterns():
    disallowed = disallowed_tools_for(set())
    for patterns in CATEGORY_UNLOCKS.values():
        for pattern in patterns:
            assert pattern in disallowed


def test_unknown_category_raises():
    with pytest.raises(ValueError):
        disallowed_tools_for({"reboot_machine"})


def test_build_categories_collects_distinct_categories():
    plan = [
        {"category": "kill_process", "description": "d1", "target_hint": "h1"},
        {"category": "delete_path", "description": "d2", "target_hint": "h2"},
        {"category": "kill_process", "description": "d3", "target_hint": "h3"},
    ]
    assert build_categories(plan) == {"kill_process", "delete_path"}


def test_build_instruction_numbers_steps_and_includes_target_hints():
    plan = [{"category": "kill_process", "description": "Kill the stale session", "target_hint": "pid 72837"}]
    instruction = build_instruction(plan)
    assert "1. [kill_process] Kill the stale session" in instruction
    assert "pid 72837" in instruction


def test_investigation_tools_are_allowed_for_reverification():
    for tool in ("Bash", "Read", "Grep", "Glob"):
        assert tool in ALLOWED_TOOLS
```

- [ ] **Step 3: Exclude `execute()`'s body from the coverage gate**

In `agent/pyproject.toml`, update `[tool.coverage.report]`'s `exclude_also` list:

```toml
[tool.coverage.report]
exclude_also = [
    # `ask()`'s body is the actual `query()` call into the Claude Agent
    # SDK -- a real, costly (tokens, a live Claude Code session) network
    # call. Its only non-trivial logic (`build_prompt`, `format_usage_footer`)
    # is already extracted into separately-tested functions above it.
    "async def ask\\(",
    # Same reasoning as `ask()` above, for the execute-agent's own query()
    # call -- `build_categories`/`disallowed_tools_for`/`build_instruction`
    # are the non-trivial logic, and are separately tested in
    # test_execute_config.py.
    "async def execute\\(",
]
```

- [ ] **Step 4: Run the tests**

Run: `cd agent && uv run pytest tests/test_execute_config.py -v`
Expected: all 8 tests PASS.

Run: `cd agent && uv run pytest`
Expected: full suite PASSES, coverage still ≥99.9%.

- [ ] **Step 5: Commit**

```bash
git checkout -b fix-workflow-execute-module
git add agent/src/vigil_agent/execute.py agent/tests/test_execute_config.py agent/pyproject.toml
git commit -m "Add execute.py: per-plan-scoped tool config for approved fix plans"
git push -u origin fix-workflow-execute-module
gh pr create --title "Add vigil_agent/execute.py" --body "Part of the fix-execution workflow (Task 11). Not yet wired to the CLI — Task 12."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 12: Python — wire `execute` into `cli.py`

**Files:**
- Modify: `agent/src/vigil_agent/cli.py`

**Interfaces:**
- Produces: `vigil-agent execute --plan-json <json>` CLI subcommand.
- Consumes: `execute.execute` (Task 11).

`cli.py` is entirely `omit`-ed from the coverage gate (see `agent/pyproject.toml`'s
`[tool.coverage.run]`) — no unit tests required or expected here, per that file's
own comment. Exercised by the manual smoke test in Task 16.

- [ ] **Step 1: Add the `execute` subcommand**

In `agent/src/vigil_agent/cli.py`, change the import line:

```python
from .diagnose import ask
```

to:

```python
from .diagnose import ask
from .execute import execute
```

Insert, after the existing `ask_parser` block and before `args =
parser.parse_args()`:

```python
    execute_parser = sub.add_parser("execute", help="Execute an approved fix plan")
    execute_parser.add_argument("--plan-json", required=True, help="JSON array of approved plan steps")
```

Insert, after the existing `if args.command == "ask": ...` block:

```python
    elif args.command == "execute":
        try:
            plan = json.loads(args.plan_json)
        except json.JSONDecodeError as e:
            print(f"failed to parse --plan-json: {e}", file=sys.stderr)
            sys.exit(1)

        answer = asyncio.run(execute(plan))
        print(answer)
```

- [ ] **Step 2: Confirm the package still imports cleanly**

Run: `cd agent && uv run python -c "from vigil_agent.cli import main; print('ok')"`
Expected: prints `ok`.

Run: `cd agent && uv run vigil-agent execute --help`
Expected: prints usage help for the `execute` subcommand, including `--plan-json`.

- [ ] **Step 3: Run the full Python suite**

Run: `cd agent && uv run pytest`
Expected: PASSES, coverage still ≥99.9% (cli.py stays omitted, unaffected).

- [ ] **Step 4: Commit**

```bash
git checkout -b fix-workflow-cli-execute-subcommand
git add agent/src/vigil_agent/cli.py
git commit -m "Wire vigil-agent execute subcommand into cli.py"
git push -u origin fix-workflow-cli-execute-subcommand
gh pr create --title "Wire vigil-agent execute into cli.py" --body "Part of the fix-execution workflow (Task 12). vigil fix (Rust side, already merged in Task 7-8) can now actually invoke this end to end."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 13: `AGENTS.md` — document the new architecture

**Files:**
- Modify: `AGENTS.md`

No automated test — this is a documentation-only task, verified by re-reading the
final file for internal consistency and by re-running the coverage command it now
documents.

- [ ] **Step 1: Rewrite the "Core architectural rule" section**

Replace the full section (from `## Core architectural rule: Rust never decides or
fixes anything` through the line ending "...not a default to slide into." right
before `## Language`) with:

```markdown
## Core architectural rule: Rust never decides or fixes anything

- `src/*.rs` only collects metrics (no network calls, no LLM) and, in `alerts.rs`,
  evaluates fixed cheap thresholds — it never reasons about *why* something is
  wrong, and never decides *whether* a fix is safe. Diagnosis, fix proposals, and
  fix execution all live only in `agent/` (Python, Claude Agent SDK), reached via
  `src/agent_process.rs` shelling out to `uv run vigil-agent ask` / `uv run
  vigil-agent execute`.
- Investigation is opt-in, not automatic: an alert firing only writes a stub
  incident file (`incidents::write_stub` — title, alert key, rule message, nothing
  else) and notifies with the command to run, `vigil investigate <alert-key>`. No
  agent process spawns until the user explicitly runs that.
- `vigil investigate` runs the same read-only investigation agent as the
  interactive `a`-key ask in `vigil ui` — identical contract either way, same
  `agent/src/vigil_agent/diagnose.py` config:
  - `ALLOWED_TOOLS`: `Bash`, `Read`, `Grep`, `Glob` only.
  - `DISALLOWED_TOOLS` always includes `Write`, `Edit`, `NotebookEdit`, plus the
    full destructive/privilege-escalating Bash denylist: `sudo *`, `su *`, `rm *`,
    `rmdir *`, `mv *`, `dd *`, `kill *`, `killall *`, `pkill *`, `diskutil
    erase*`/`partition*`/`eraseVolume*`, `launchctl unload*`/`bootout*`/`remove*`,
    `chmod *`, `chown *`, `shutdown *`, `reboot *`, `halt *`, `defaults
    write*`/`delete*`.
  - It only ever produces text — nothing on the machine changes from this path. If
    it identifies a specific, narrowly-scoped, low-risk fix, it may additionally
    append a `## Proposed fix` JSON block (schema in `prompts.py`'s
    `SYSTEM_PROMPT`) — a proposal, not an action.
- A `## Proposed fix` only ever executes through `vigil fix <incident-file>`, and
  only after per-step interactive approval in the terminal (`fix_process::run`).
  Approved steps — and *only* the approved ones; a rejected step is never even
  mentioned to the execute-agent — are handed to a **separate**, narrowly-scoped
  execute-agent session (`agent/src/vigil_agent/execute.py`), whose tool config is
  built fresh per invocation from exactly the fix categories present in what was
  approved:
  - `kill_process` unlocks `Bash(kill *)`/`Bash(killall *)`/`Bash(pkill *)`.
  - `delete_path` unlocks `Bash(rm *)`/`Bash(rmdir *)`/`Bash(mv *)`.
  - `system_setting` unlocks `Bash(defaults write*)`/`Bash(defaults delete*)`/
    `Bash(launchctl unload*)`/`Bash(launchctl bootout*)`/`Bash(launchctl remove*)`.
  - A **non-liftable hard floor** stays blocked regardless of what's approved —
    outside all three categories, no plan can ever unlock it:
    `execute.HARD_FLOOR_DISALLOWED_TOOLS` (`sudo`/`su`/`dd`/`diskutil
    erase`-family/`shutdown`/`reboot`/`halt`/`chmod`/`chown`, plus
    `Write`/`Edit`/`NotebookEdit` — the execute-agent acts only through unlocked
    Bash patterns, never by writing/editing files directly).
  - The execute-agent must re-verify a step's target before acting (a
    pid/path/setting captured at proposal time may be stale by execution time —
    see `agent::build_diagnosis_question`'s doc comment for a real pid-reuse race
    this project already hit) and must abort all remaining steps the moment one
    fails or its target has diverged, rather than continuing on a plan whose
    assumptions no longer hold.
  - Never widen `HARD_FLOOR_DISALLOWED_TOOLS`, never let a category unlock more
    than its own listed patterns, and never let `vigil fix` run *any* step the user
    didn't explicitly approve.
- Battery: percentage-trend ETA only (`src/battery.rs`), no `powermetrics`/sudo — a
  deliberate, explicit choice, not an oversight. If accurate per-process power
  attribution is ever wanted, that's a new decision (record it under
  `docs/decisions/`), not a default to slide into.
```

- [ ] **Step 2: Update the coverage gate's regex and file count**

In the Testing section, change:

```
- **Hard rule: `cargo llvm-cov --workspace --ignore-filename-regex
  'src/(main|watch|ui_loop|menubar_loop|agent_process|notify)\.rs' --fail-under-lines
  99.5 --fail-under-regions 98`.**
```

to:

```
- **Hard rule: `cargo llvm-cov --workspace --ignore-filename-regex
  'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs'
  --fail-under-lines 99.5 --fail-under-regions 98`.**
```

Change "The six `--ignore-filename-regex` files hold *only* irreducible
OS-boundary glue (a real terminal event loop, a real macOS tray event loop, a real
process spawn) —" to "The eight `--ignore-filename-regex` files hold *only*
irreducible OS-boundary glue (a real terminal event loop, a real macOS tray event
loop, three real process spawns) —".

Change "already keep their tested logic separate from
`agent_process.rs`/`ui_loop.rs`/`menubar_loop.rs`/`notify.rs`." to "already keep
their tested logic separate from
`agent_process.rs`/`ui_loop.rs`/`menubar_loop.rs`/`notify.rs`/`investigate_process.rs`/`fix_process.rs`."

Change "Because `cargo test` cannot exercise the six excluded files, a change
touching any of them needs a manual smoke run before merging: `vigil snapshot |
jq .`, `vigil watch --count 2 --out /tmp/x.jsonl` (check the JSONL line and that
the status file got written), `vigil incidents` + `vigil incidents --show <name>`
(check `echo $?` on both a match and a miss — `Commands::Incidents` goes through
`std::process::exit`), and `vigil menubar` launched briefly and killed." to:

```
Because `cargo test` cannot exercise the eight excluded files, a change touching
any of them needs a manual smoke run before merging: `vigil snapshot | jq .`,
`vigil watch --count 2 --out /tmp/x.jsonl` (check the JSONL line and that the
status file got written), `vigil incidents` + `vigil incidents --show <name>`
(check `echo $?` on both a match and a miss — `Commands::Incidents` goes through
`std::process::exit`), `vigil menubar` launched briefly and killed, `vigil
investigate <key>` against a hand-written stub incident file (spends real agent
tokens — see the Testing section's Python bullet for the equivalent
`vigil-agent execute --help` check), and `vigil fix <file>` against an incident
with a `## Proposed fix` block, approving then rejecting a step to confirm both
paths.
```

- [ ] **Step 3: Rewrite "The live incident-monitoring loop" section**

Replace the full section (from `## The live incident-monitoring loop` through the
line ending "...it stays investigate → journal → notify → (human decides). This is
the same rule as the tool-access one above, restated because it's the thing this
whole feature exists to not violate." right before `## Git workflow`) with:

```markdown
## The live incident-monitoring loop

- `vigil watch` runs continuously in the background; an alert firing writes a
  stub incident file (`incidents::write_stub`) and notifies with the exact command
  to investigate it — `vigil investigate <alert-key>` — but does not itself spawn
  an agent. Every incident file lives at
  `~/.vigil/incidents/<date>-<time>-<slug>.md` — a fixed, home-relative path (vigil
  is meant to run from anywhere, not just its own repo). The interactive `a`-key
  ask in `vigil ui` is deliberately NOT journaled — on-screen only, by design.
- `vigil incidents` reads that journal from a plain shell (list recent, or `--show
  <name>` for one in full) — this exists specifically because a push notification
  can't spontaneously open an already-running `vigil ui` session.
- When real incidents land, read them, look for a genuine pattern across more than
  one (not a single anecdote), and treat a confirmed pattern as a legitimate case
  for a targeted vigil improvement — this is the actual mechanism this project uses
  to find bugs and gaps in itself, not a hypothetical. Verify a proposed fix
  against the *actual* field data before shipping it, not just against the pattern
  that motivated it: an initial narrow fix (`agent::DiagnosisCoalescer`, a 120s
  near-simultaneous window keyed by target process) was itself later found to
  never actually engage — real repeats arrived 5-13 minutes apart, gated by each
  rule's own re-fire cooldown, not by anything a 120s window could catch. It was
  replaced by `alerts::IncidentTracker`, a longer open/close window (2x cooldown)
  keyed the same way (by target process, not by time alone — the earlier design's
  own reasoning for that part held up: the same live incident batch that motivated
  coalescing also contained a genuinely independent finding a *time-only* cooldown
  would have silently dropped) and covering notification + the incident stub
  together — diagnosis timing itself is opt-in now and outside `IncidentTracker`'s
  concern entirely. Re-verify a fix like this against fresh field data after
  shipping it, not just once at design time — this project's own history is the
  example.
- Running a fix through `vigil fix` never marks an incident resolved by itself —
  `IncidentTracker`'s open/closed state stays driven purely by whether the alert
  keeps re-firing, unrelated to whether a fix was proposed or run. "The agent
  reports it took an action" and "the underlying condition actually cleared" are
  different claims; conflating them would let a failed or partially-effective fix
  read as resolved when the next snapshot might show the same alert firing again
  minutes later.
- Nothing on the machine changes except through the explicit `vigil investigate` →
  `vigil fix` path described in the "Core architectural rule" section above, and
  never without the human approving the specific plan first. This loop stays
  investigate (opt-in) → propose (opt-in, agent's judgment) → approve (human,
  per-step) → execute (narrowly-scoped agent) → journal — never a shortcut around
  any of those steps.
```

- [ ] **Step 4: Verify the documented command actually passes**

Run: `cargo llvm-cov --workspace --ignore-filename-regex
'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs'
--fail-under-lines 99.5 --fail-under-regions 98`
Expected: PASSES (same command Task 9's Step 8 already confirmed — re-run here to
verify AGENTS.md's text now matches reality exactly, character for character).

- [ ] **Step 5: Commit**

```bash
git checkout -b fix-workflow-agents-md
git add AGENTS.md
git commit -m "Document the opt-in investigate/propose/approve/execute workflow in AGENTS.md"
git push -u origin fix-workflow-agents-md
gh pr create --title "Update AGENTS.md for the fix-execution workflow" --body "Part of the fix-execution workflow (Task 13). Documentation only — no code changes."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 14: `README.md` — document the new commands and flow

**Files:**
- Modify: `README.md`

No automated test — documentation only.

- [ ] **Step 1: Update the intro paragraph**

Change:

```markdown
The split is deliberate: **Rust never decides or fixes anything** — it only cheaply
collects metrics (no network, no LLM) and draws them. Diagnosis and recommendations are
a separate Python process. The agent can *inspect* the live system (logs, `sample`,
`vm_stat`, `du`, ...) but is blocked at the tool level from modifying it — no Write/Edit,
no kill/rm/mv/sudo/shutdown. It only ever produces text; nothing on your machine changes
without you doing it yourself.
```

to:

```markdown
The split is deliberate: **Rust never decides or fixes anything** — it only cheaply
collects metrics (no network, no LLM) and draws them. Diagnosis, fix proposals, and
fix execution are all a separate Python process. Investigation (`vigil investigate`)
is read-only, same as always — the agent can *inspect* the live system (logs,
`sample`, `vm_stat`, `du`, ...) but is blocked at the tool level from modifying it. If
it identifies a specific, low-risk fix, it may propose one; nothing runs until you
explicitly approve it step by step with `vigil fix`, which hands only what you
approved to a separate, narrowly-scoped agent session — see "Investigate, propose,
approve, execute" below.
```

- [ ] **Step 2: Replace the auto-trigger paragraph in Features**

Change:

```markdown
Every agent answer — interactive `a` or auto-triggered — ends with a token/cost
footer (`_Tokens: N in / M out (+K cache read) — ~$X_`), since the agent's own
token spend is part of the overhead this project tries to keep visible, not hide.

When `high_load`, `cpu_hog`, or `battery_low` fires, vigil also asks the agent to
investigate in a background thread — non-blocking, a follow-up notification with the
answer once it's done, and the diagnosis is saved as a markdown file in
`~/.vigil/incidents/<date>-<time>-<slug>.md` (override with `--incidents-dir`). Disk
and plain memory-pressure alerts don't auto-trigger the agent, and the interactive `a`
flow is UI-only — neither writes to the incident journal.
```

to:

```markdown
Every agent answer — interactive `a`, `vigil investigate`, or `vigil fix` — ends with
a token/cost footer (`_Tokens: N in / M out (+K cache read) — ~$X_`), since the
agent's own token spend is part of the overhead this project tries to keep visible,
not hide.

### Investigate, propose, approve, execute

An alert firing writes a stub incident file (title, alert key, rule message) to
`~/.vigil/incidents/<date>-<time>-<slug>.md` (override with `--incidents-dir`) and
notifies with the command to investigate it — nothing runs automatically:

```bash
# after a notification like "cpu_hog:37489 — investigate? vigil investigate cpu_hog:37489"
./target/release/vigil investigate cpu_hog:37489
```

This runs the same read-only agent as the interactive `a` flow and appends its
answer to the incident file. If it's confident about a specific, narrow, low-risk
fix, it also appends a `## Proposed fix` JSON plan — most diagnoses won't have one.
When there is one:

```bash
./target/release/vigil fix ~/.vigil/incidents/2026-08-09-01-09-41-cpu-hog-37489.md
```

prompts per step (`[1/2] kill_process — Kill the stale claude session ... Approve?
[y/N]`), then hands *only* the steps you approved to a separate, narrowly-scoped
agent session (`agent/src/vigil_agent/execute.py`) — its tool config is built fresh
each time from exactly the fix categories your approved steps belong to
(`kill_process`, `delete_path`, `system_setting`), with a non-liftable hard floor
(`sudo`/`dd`/`diskutil erase`-family/`shutdown`/`chmod`/`chown`/etc.) that stays
blocked no matter what's approved. The execute-agent re-verifies each step's target
before acting — a pid/path captured when the plan was proposed can be stale by
execution time — and aborts all remaining steps the moment one fails or diverges,
rather than pressing on with stale assumptions. Results append to the same incident
file as a `## Fix execution` section. See
[docs/decisions/0006-opt-in-investigate-propose-approve-execute.md](docs/decisions/0006-opt-in-investigate-propose-approve-execute.md)
for the full design rationale.
```

- [ ] **Step 3: Update the "Leaked process detection" bullet**

Change "Auto-triggers an agent investigation, same as `cpu_hog`. See" to "Investigate
with `vigil investigate high_process_count:<name>`, same as `cpu_hog`. See".

- [ ] **Step 4: Add commands to the Usage section**

In the ` ```bash ... ``` ` block under `## Usage`, after the existing `vigil
incidents --show cpu-hog-64955` line and before the `vigil menubar` comment, insert:

```bash
# investigate an alert the notification pointed at, then act on any proposed fix
./target/release/vigil investigate cpu_hog:37489
./target/release/vigil fix ~/.vigil/incidents/2026-08-09-01-09-41-cpu-hog-37489.md
```

- [ ] **Step 5: Update the Tests section**

Change:

```bash
cargo llvm-cov --workspace --ignore-filename-regex 'src/(main|watch|ui_loop|menubar_loop|agent_process|notify)\.rs' \
  --fail-under-lines 99.5 --fail-under-regions 98
```

to:

```bash
cargo llvm-cov --workspace --ignore-filename-regex 'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs' \
  --fail-under-lines 99.5 --fail-under-regions 98
```

Change "six files are genuine OS-boundary glue — a real terminal event loop, a real
macOS menu bar event loop, a real process spawn — with everything else around them
fully unit-tested" to "eight files are genuine OS-boundary glue — a real terminal
event loop, a real macOS menu bar event loop, three real process spawns — with
everything else around them fully unit-tested".

- [ ] **Step 6: Update the Architecture diagram**

Replace the ` ```  ... ``` ` block under `## Architecture` with:

```
vigil (Rust)                            agent/ (Python, Claude Agent SDK)
├── cli.rs — clap arg definitions       ├── prompts.py    — pure prompt building (tested without network)
├── snapshot.rs — snapshot collection,  ├── diagnose.py   — investigation query(): Bash/Read/Grep/Glob allowed,
│   incl. connections via `netstat`     │                    Write/Edit + destructive Bash patterns denylisted
│   (see docs/decisions/0001)           ├── execute.py    — fix-execution query(): per-plan-scoped Bash unlocks
├── alerts.rs — threshold rules         │                    on top of a non-liftable hard floor
│   (no LLM, no network)                └── cli.py        — vigil-agent ask --snapshot F --question Q
├── battery.rs — drain-rate ETA                              vigil-agent execute --plan-json J
│   (no powermetrics/sudo)
├── incidents.rs — markdown journal:
│   write_stub / append_diagnosis /
│   append_fix_execution (~/.vigil/incidents/)
├── incidents_cmd.rs — `vigil incidents`
├── fixplan.rs — proposed-fix JSON plan
│   parsing + approval formatting
├── investigate.rs — resolve an alert
│   key to its incident file
├── agent.rs — question/arg building,
│   output parsing (pure, unit-tested)
├── menubar.rs — health classification,
│   icon rendering (pure, unit-tested)
├── watch.rs — the `vigil watch` loop
│   (OS-boundary glue, see ADR-0003)
├── ui_loop.rs — the `vigil ui` terminal
│   event loop (OS-boundary glue)
├── menubar_loop.rs — the real macOS
│   tray/menu event loop (OS-boundary)
├── agent_process.rs — the actual
│   `uv run vigil-agent` spawn (OS-boundary)
├── investigate_process.rs — `vigil
│   investigate`'s spawn (OS-boundary)
├── fix_process.rs — `vigil fix`'s
│   approval prompt + spawn (OS-boundary)
├── notify.rs — the actual `osascript`
│   shell-out (OS-boundary)
└── main.rs — Cli::parse() + dispatch
    to the module owning each subcommand
```

- [ ] **Step 7: Commit**

```bash
git checkout -b fix-workflow-readme
git add README.md
git commit -m "Document vigil investigate/fix in README"
git push -u origin fix-workflow-readme
gh pr create --title "Update README for the fix-execution workflow" --body "Part of the fix-execution workflow (Task 14). Documentation only."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 15: ADR 0006

**Files:**
- Create: `docs/decisions/0006-opt-in-investigate-propose-approve-execute.md`

- [ ] **Step 1: Write the ADR**

```markdown
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
  1. **Investigation becomes opt-in.** An alert firing now only writes a stub
     incident file (`incidents::write_stub` — title, key, rule message) and
     notifies with the exact command to run, `vigil investigate <alert-key>`. No
     agent process spawns until the user explicitly runs that — in `vigil watch`
     and in `vigil ui`'s own background alert loop alike; the interactive
     `a`/`w`-key ask is unaffected.
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
```

- [ ] **Step 2: Commit**

```bash
git checkout -b fix-workflow-adr
git add docs/decisions/0006-opt-in-investigate-propose-approve-execute.md
git commit -m "Add ADR 0006: opt-in investigate/propose/approve/execute fix workflow"
git push -u origin fix-workflow-adr
gh pr create --title "Add ADR 0006 for the fix-execution workflow" --body "Part of the fix-execution workflow (Task 15). Documentation only."
gh pr merge --squash --delete-branch
git checkout master && git pull
```

---

## Task 16: Final verification and manual smoke test

**Files:** none (verification only).

- [ ] **Step 1: Full Rust suite + coverage gate**

Run:
```bash
cargo test --release
cargo llvm-cov --workspace --ignore-filename-regex 'src/(main|watch|ui_loop|menubar_loop|agent_process|notify|investigate_process|fix_process)\.rs' --fail-under-lines 99.5 --fail-under-regions 98
```
Expected: both PASS.

- [ ] **Step 2: Full Python suite**

Run: `cd agent && uv run pytest`
Expected: PASSES, coverage ≥99.9%.

- [ ] **Step 3: Manual smoke test — `vigil investigate` (spends real agent tokens)**

```bash
mkdir -p /tmp/vigil-smoke-incidents
cat > /tmp/vigil-smoke-incidents/2026-08-09-99-99-99-cpu-hog-1.md <<'EOF'
# vigil: process hogging CPU

**Alert key:** `cpu_hog:1`

**Rule message:** launchd (pid 1) has held 5% CPU for 3 consecutive samples.
EOF
./target/release/vigil investigate cpu_hog:1 --incidents-dir /tmp/vigil-smoke-incidents
cat /tmp/vigil-smoke-incidents/2026-08-09-99-99-99-cpu-hog-1.md
```
Expected: the agent runs a real (cheap — pid 1 is trivially uninteresting)
investigation and a `## Agent diagnosis` section is appended to the file.

- [ ] **Step 4: Manual smoke test — `vigil fix` (spends real agent tokens)**

```bash
cat >> /tmp/vigil-smoke-incidents/2026-08-09-99-99-99-cpu-hog-1.md <<'EOF'

## Proposed fix

```json
{"plan": [{"category": "kill_process", "description": "Harmless smoke-test step: check whether a process named definitely-does-not-exist-12345 is running, and report done regardless (there is nothing real to kill here — this is a wiring test)", "target_hint": "definitely-does-not-exist-12345"}]}
```
EOF
./target/release/vigil fix /tmp/vigil-smoke-incidents/2026-08-09-99-99-99-cpu-hog-1.md
# approve the one step when prompted (y)
cat /tmp/vigil-smoke-incidents/2026-08-09-99-99-99-cpu-hog-1.md
rm -rf /tmp/vigil-smoke-incidents
```
Expected: the execute-agent runs, reports the step (likely `done` — it can safely
verify the fake process name doesn't exist and stop there, since its own
instructions call for re-verifying before acting), and a `## Fix execution`
section is appended to the file with a token/cost footer.

- [ ] **Step 5: Manual smoke test — `vigil watch`'s new stub-and-notify path**

```bash
./target/release/vigil watch --count 1 --out /tmp/vigil-smoke.jsonl --incidents-dir /tmp/vigil-smoke-incidents-2
```
Expected: runs one sample and exits; if any alert fired, confirm a stub file
appeared under `/tmp/vigil-smoke-incidents-2/` containing only the H1/alert-key/
rule-message header (no `## Agent diagnosis` section) and that the terminal/native
notification (if `--no-notify` wasn't passed) mentions `vigil investigate`. Clean
up: `rm -rf /tmp/vigil-smoke.jsonl /tmp/vigil-smoke-incidents-2`.

- [ ] **Step 6: Report completion**

Once all six steps above pass, the fix-execution workflow is fully implemented and
verified end to end. No further commit needed for this task (verification only) —
if any step above surfaces a bug, fix it in a follow-up task/PR rather than
silently patching past this checklist.
