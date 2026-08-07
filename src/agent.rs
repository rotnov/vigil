//! Bridge to the `vigil_agent` Python package (Claude Agent SDK).
//!
//! `vigil` itself never talks to the network or an LLM — it only shells
//! out to `uv run vigil-agent ask` with a snapshot + a question and prints
//! whatever text comes back. All actual reasoning happens in the Python
//! process.
//!
//! The actual process-spawning/thread-spawning glue lives in
//! `agent_process.rs`, re-exported below — kept in its own file (and
//! excluded from the coverage gate, see AGENTS.md) because it does real,
//! costly work (spawns `uv run vigil-agent`, which runs a real Claude Agent
//! SDK session and spends real tokens) that a unit test shouldn't trigger.
//! Everything here is pure question/arg construction, testable without
//! spawning anything.

use std::path::{Path, PathBuf};

pub use crate::agent_process::{ask, maybe_diagnose_alert_async};

/// Builds the question sent to the agent for an auto-triggered diagnosis.
/// Pure — kept separate from `maybe_diagnose_alert_async`'s side effects so
/// the exact wording (cross-alert context, the watch-log pointer) is
/// unit-testable without spawning anything. `watch_log_path` is `None` when
/// the caller has no persistent JSONL history to point at (e.g. `vigil
/// ui`'s own snapshot loop doesn't write one — only `vigil watch` does).
pub(crate) fn build_diagnosis_question(alert_message: &str, recent_context: Option<&str>, watch_log_path: Option<&str>) -> String {
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
    format!(
        "A monitoring rule just fired: \"{alert_message}\".{context_note}{history_note} Investigate \
         the likely cause — check beyond the snapshot if useful (e.g. logs, `sample` a hot pid, \
         thermal state) — and suggest what to check or do next."
    )
}

/// Alert keys worth an automatic agent diagnosis: CPU spikes (by the time
/// you'd type a question, the spike may already be gone) and low battery
/// (root-causing a drain benefits from the agent actually checking thermal
/// state / recent high-CPU history rather than a static rule). Disk and
/// plain memory-pressure alerts are left to the interactive 'a' flow.
pub(crate) fn is_auto_diagnose_worthy(alert_key: &str) -> bool {
    alert_key == "high_load" || alert_key.starts_with("cpu_hog:") || alert_key == "battery_low"
}

/// A short, single-paragraph preview of a (possibly multi-paragraph)
/// diagnosis, for the notification banner — the full text goes to the
/// incident journal file instead, which is what `message` here points at.
pub(crate) fn teaser(text: &str, max_len: usize) -> String {
    // The agent's answers tend to open with a markdown heading (e.g.
    // "## Diagnosis") — skip those to reach the first real content line.
    let first_line = text
        .lines()
        .map(str::trim)
        .find(|l| !l.is_empty() && !l.starts_with('#'))
        .unwrap_or_else(|| text.trim());
    if first_line.chars().count() <= max_len {
        first_line.to_string()
    } else {
        let truncated: String = first_line.chars().take(max_len).collect();
        format!("{}…", truncated.trim_end())
    }
}

pub(crate) fn temp_snapshot_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Includes a nanosecond timestamp, not just the PID, because background
    // diagnosis threads (see `maybe_diagnose_alert_async`) can call `ask()`
    // concurrently within the same process — a PID-only name would race.
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
        let q = build_diagnosis_question("pycharm has held 129% CPU", None, None);
        assert!(q.contains("pycharm has held 129% CPU"));
    }

    #[test]
    fn build_diagnosis_question_includes_recent_context_when_present() {
        let q = build_diagnosis_question("high load", Some("swap is 91% full"), None);
        assert!(q.contains("swap is 91% full"));
    }

    #[test]
    fn build_diagnosis_question_omits_context_note_when_none() {
        let q = build_diagnosis_question("high load", None, None);
        assert!(!q.contains("also fired recently"));
    }

    #[test]
    fn build_diagnosis_question_points_at_the_watch_log_when_given_a_path() {
        let q = build_diagnosis_question("high load", None, Some("/Users/denis/.vigil/watch.jsonl"));
        assert!(q.contains("/Users/denis/.vigil/watch.jsonl"));
        assert!(q.contains("growing over"));
    }

    #[test]
    fn build_diagnosis_question_omits_history_note_without_a_watch_log_path() {
        let q = build_diagnosis_question("high load", None, None);
        assert!(!q.contains("growing over"));
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
    fn teaser_skips_markdown_heading_to_reach_real_content() {
        let text = "## Diagnosis\n\nSwap is 91% full, pycharm is the top consumer.\n\n## Suggestions\n1. Restart it.";
        assert_eq!(teaser(text, 200), "Swap is 91% full, pycharm is the top consumer.");
    }

    #[test]
    fn teaser_truncates_long_lines_with_ellipsis() {
        let long = "a".repeat(300);
        let t = teaser(&long, 50);
        assert_eq!(t.chars().count(), 51); // 50 chars + ellipsis
        assert!(t.ends_with('…'));
    }

    #[test]
    fn teaser_falls_back_to_whole_text_when_only_headings_present() {
        assert_eq!(teaser("## Just A Heading", 200), "## Just A Heading");
    }

    #[test]
    fn temp_snapshot_path_is_unique_per_process() {
        let p = temp_snapshot_path();
        assert!(p.to_string_lossy().contains(&std::process::id().to_string()));
    }

    #[test]
    fn temp_snapshot_path_is_unique_across_concurrent_calls() {
        // Guards against the race a PID-only filename would have with
        // `maybe_diagnose_alert_async` spawning concurrent `ask()` calls.
        let a = temp_snapshot_path();
        let b = temp_snapshot_path();
        assert_ne!(a, b);
    }

    #[test]
    fn auto_diagnose_worthy_for_cpu_and_battery_alerts() {
        assert!(is_auto_diagnose_worthy("high_load"));
        assert!(is_auto_diagnose_worthy("cpu_hog:1234"));
        assert!(is_auto_diagnose_worthy("battery_low"));
    }

    #[test]
    fn auto_diagnose_not_worthy_for_disk_and_memory_alerts() {
        assert!(!is_auto_diagnose_worthy("low_disk:/"));
        assert!(!is_auto_diagnose_worthy("low_memory"));
        assert!(!is_auto_diagnose_worthy("swap_pressure"));
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
