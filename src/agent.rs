//! Bridge to the `vigil_agent` Python package (Claude Agent SDK).
//!
//! `vigil` itself never talks to the network or an LLM — it only shells
//! out to `uv run vigil-agent ask` with a snapshot + a question and prints
//! whatever text comes back. All actual reasoning happens in the Python
//! process.
//!
//! The actual process-spawning glue lives in
//! `agent_process.rs`, re-exported below — kept in its own file (and
//! excluded from the coverage gate, see AGENTS.md) because it does real,
//! costly work (spawns `uv run vigil-agent`, which runs a real Claude Agent
//! SDK session and spends real tokens) that a unit test shouldn't trigger.
//! Everything here is pure question/arg construction, testable without
//! spawning anything.

use std::path::{Path, PathBuf};

pub use crate::agent_process::ask;

/// Builds the question sent to the agent for an auto-triggered diagnosis.
/// Pure — kept separate from `investigate_process::run`'s side effects so
/// the exact wording (cross-alert context, the watch-log pointer, the
/// pid-reuse warning) is unit-testable without spawning anything.
/// `watch_log_path` is `None` when the caller has no persistent JSONL
/// history to point at (e.g. `vigil ui`'s own snapshot loop doesn't write
/// one — only `vigil watch` does). `command` is `Alert::command` — the
/// target process's full command line, captured synchronously at the
/// moment the alert fired (see that field's doc comment for why: this
/// diagnosis runs seconds to minutes later, in the background, and by then
/// the OS may have recycled the pid to a completely different process).
pub(crate) fn build_diagnosis_question(
    alert_message: &str,
    recent_context: Option<&str>,
    watch_log_path: Option<&str>,
    command: Option<&str>,
) -> String {
    let context_note = recent_context
        .map(|c| format!(" Other rules that also fired recently (possibly the same root cause): {c}."))
        .unwrap_or_default();
    let history_note = match watch_log_path {
        Some(path) => format!(
            " Don't just describe this one snapshot — check recent history in {path} (JSON Lines, \
             one snapshot per line) to say whether the flagged process/metric has been growing over \
             time or is a one-off spike, since that changes what's actually worth doing about it."
        ),
        None => String::new(),
    };
    let command_note = match command {
        Some(cmd) if !cmd.is_empty() => format!(
            " The flagged process's command line at the moment this fired was: `{cmd}`. Note this pid \
             may already refer to a different process by the time you check it (pids get reused) — if \
             `ps -p <pid>` shows something else now, trust the command line above for what was actually \
             running, not whatever the pid currently points to."
        ),
        Some(_) => String::new(), // empty cmd: the process was mid fork/exec when sampled, nothing useful to quote
        None => String::new(),
    };
    format!(
        "A monitoring rule just fired: \"{alert_message}\".{context_note}{history_note}{command_note} Investigate \
         the likely cause — check beyond the snapshot if useful (e.g. logs, `sample` a hot pid, \
         thermal state) — and suggest what to check or do next."
    )
}

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

pub(crate) fn temp_snapshot_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Includes a nanosecond timestamp, not just the PID, as a defensive
    // uniqueness guarantee in case multiple invocations ever overlap — cheap
    // to keep even though nothing in the current codebase actually calls
    // `ask()` concurrently within one process.
    p.push(format!("vigil-snapshot-{}-{nanos}.json", std::process::id()));
    p
}

/// Pure argv construction, kept separate from `Command` execution so it can
/// be unit-tested without spawning a real process.
pub(crate) fn build_args(question: &str, snapshot_path: &Path, agent_dir: &str) -> Vec<String> {
    vec![
        "uv".to_string(),
        "run".to_string(),
        "--project".to_string(),
        agent_dir.to_string(),
        "vigil-agent".to_string(),
        "ask".to_string(),
        "--snapshot".to_string(),
        snapshot_path.to_string_lossy().to_string(),
        "--question".to_string(),
        question.to_string(),
    ]
}

/// Turns a finished `uv run vigil-agent` process's `Output` into the same
/// `Result` shape `ask` returns — split out so the success/failure/trim
/// logic is testable against a manually constructed `Output`, without
/// actually spawning `uv`.
pub(crate) fn interpret_output(output: &std::process::Output) -> Result<String, String> {
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            "vigil-agent exited with an error and no message".to_string()
        } else {
            stderr
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_diagnosis_question_includes_the_alert_message() {
        let q = build_diagnosis_question("pycharm has held 129% CPU", None, None, None);
        assert!(q.contains("pycharm has held 129% CPU"));
    }

    #[test]
    fn build_diagnosis_question_includes_recent_context_when_present() {
        let q = build_diagnosis_question("high load", Some("swap is 91% full"), None, None);
        assert!(q.contains("swap is 91% full"));
    }

    #[test]
    fn build_diagnosis_question_omits_context_note_when_none() {
        let q = build_diagnosis_question("high load", None, None, None);
        assert!(!q.contains("also fired recently"));
    }

    #[test]
    fn build_diagnosis_question_points_at_the_watch_log_when_given_a_path() {
        let q = build_diagnosis_question("high load", None, Some("/Users/denis/.vigil/watch.jsonl"), None);
        assert!(q.contains("/Users/denis/.vigil/watch.jsonl"));
        assert!(q.contains("growing over"));
    }

    #[test]
    fn build_diagnosis_question_omits_history_note_without_a_watch_log_path() {
        let q = build_diagnosis_question("high load", None, None, None);
        assert!(!q.contains("growing over"));
    }

    #[test]
    fn build_diagnosis_question_includes_the_command_line_and_a_pid_reuse_warning() {
        let q = build_diagnosis_question("claude (pid 27339) has held 177% CPU", None, None, Some("bfs -S dfs / -path *"));
        assert!(q.contains("bfs -S dfs / -path *"));
        assert!(q.contains("pids get reused"));
    }

    #[test]
    fn build_diagnosis_question_omits_command_note_when_none() {
        let q = build_diagnosis_question("high load", None, None, None);
        assert!(!q.contains("pids get reused"));
    }

    #[test]
    fn build_diagnosis_question_omits_command_note_when_command_is_empty() {
        // An empty `cmd` happens when the process was sampled mid
        // fork/exec -- nothing useful to quote, so no note rather than a
        // confusing empty backtick-quote.
        let q = build_diagnosis_question("high load", None, None, Some(""));
        assert!(!q.contains("pids get reused"));
    }

    #[test]
    fn build_args_wires_project_dir_snapshot_and_question() {
        let args = build_args("why is disk space low?", Path::new("/tmp/snap.json"), "agent");
        assert_eq!(args[0], "uv");
        assert_eq!(args[1], "run");
        assert!(args.windows(2).any(|w| w == ["--project".to_string(), "agent".to_string()]));
        assert!(args.contains(&"/tmp/snap.json".to_string()));
        assert!(args.contains(&"why is disk space low?".to_string()));
    }

    #[test]
    fn temp_snapshot_path_is_unique_per_process() {
        let p = temp_snapshot_path();
        assert!(p.to_string_lossy().contains(&std::process::id().to_string()));
    }

    #[test]
    fn temp_snapshot_path_is_unique_across_concurrent_calls() {
        // Guards against a PID-only filename colliding if `ask()` is ever
        // called twice in quick succession within one process (nothing
        // currently does this, but the guarantee is cheap to keep).
        let a = temp_snapshot_path();
        let b = temp_snapshot_path();
        assert_ne!(a, b);
    }

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

    fn output_with(success: bool, stdout: &str, stderr: &str) -> std::process::Output {
        // `std::process::ExitStatus` has no public constructor, so build
        // one the portable way: actually run a trivial process and read
        // back its real status.
        let status = std::process::Command::new(if success { "true" } else { "false" })
            .status()
            .expect("failed to run `true`/`false` for a fabricated ExitStatus");
        std::process::Output { status, stdout: stdout.as_bytes().to_vec(), stderr: stderr.as_bytes().to_vec() }
    }

    #[test]
    fn interpret_output_trims_stdout_on_success() {
        let out = output_with(true, "  the answer is 42  \n", "");
        assert_eq!(interpret_output(&out), Ok("the answer is 42".to_string()));
    }

    #[test]
    fn interpret_output_returns_stderr_on_failure() {
        let out = output_with(false, "", "boom: connection refused\n");
        assert_eq!(interpret_output(&out), Err("boom: connection refused".to_string()));
    }

    #[test]
    fn interpret_output_falls_back_to_generic_message_when_stderr_is_empty() {
        let out = output_with(false, "", "");
        assert_eq!(interpret_output(&out), Err("vigil-agent exited with an error and no message".to_string()));
    }
}
