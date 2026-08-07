//! Bridge to the `vigil_agent` Python package (Claude Agent SDK).
//!
//! `vigil` itself never talks to the network or an LLM — it only shells
//! out to `uv run vigil-agent ask` with a snapshot + a question and prints
//! whatever text comes back. All actual reasoning happens in the Python
//! process.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Write the snapshot to a temp file, invoke the agent CLI, and return its
/// stdout (trimmed) or an error message suitable for display in the UI.
pub fn ask(question: &str, snapshot_json: &str, agent_dir: &str) -> Result<String, String> {
    let tmp = temp_snapshot_path();
    std::fs::write(&tmp, snapshot_json).map_err(|e| format!("failed to write temp snapshot: {e}"))?;

    let args = build_args(question, &tmp, agent_dir);
    let output = Command::new(&args[0]).args(&args[1..]).output();
    let _ = std::fs::remove_file(&tmp);

    let output = output.map_err(|e| format!("failed to launch vigil-agent (is `uv` installed?): {e}"))?;

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

/// Alert keys worth an automatic agent diagnosis: CPU spikes (by the time
/// you'd type a question, the spike may already be gone) and low battery
/// (root-causing a drain benefits from the agent actually checking thermal
/// state / recent high-CPU history rather than a static rule). Disk and
/// plain memory-pressure alerts are left to the interactive 'a' flow.
fn is_auto_diagnose_worthy(alert_key: &str) -> bool {
    alert_key == "high_load" || alert_key.starts_with("cpu_hog:") || alert_key == "battery_low"
}

/// If `alert` is worth it, ask the agent to investigate in a background
/// thread and fire a follow-up notification with the answer once ready.
/// Never blocks the caller. The agent has real (read-only) investigation
/// tools here — same contract as the interactive 'a' flow: it can look
/// around (logs, `sample`, `vm_stat`, ...) but never modify anything. A
/// failed diagnosis is logged, not surfaced as a notification, since the
/// plain rule-based alert already fired.
///
/// Callers are expected to have already checked
/// `alerts::IncidentTracker::is_new_incident` before calling this — that's
/// what actually prevents redundant investigations for an already-open
/// incident on the same target process; this function only checks whether
/// the alert *key* is diagnose-worthy at all.
pub fn maybe_diagnose_alert_async(
    alert: &crate::alerts::Alert,
    snapshot_json: &str,
    agent_dir: &str,
    incidents_dir: &str,
    recent_context: Option<&str>,
) {
    if !is_auto_diagnose_worthy(&alert.key) {
        return;
    }

    eprintln!(
        "[vigil] diagnosing [{}] — recent_context: {}",
        alert.key,
        recent_context.unwrap_or("(none)")
    );
    let context_note = recent_context
        .map(|c| format!(" Other rules that also fired recently (possibly the same root cause): {c}."))
        .unwrap_or_default();
    let question = format!(
        "A monitoring rule just fired: \"{}\".{context_note} Investigate the likely cause — check \
         beyond the snapshot if useful (e.g. logs, `sample` a hot pid, thermal state) — and suggest \
         what to check or do next.",
        alert.message
    );
    let notif_title = format!("{} — agent diagnosis", alert.title);
    let alert_key = alert.key.clone();
    let alert_title = alert.title.clone();
    let alert_message = alert.message.clone();
    let snapshot_json = snapshot_json.to_string();
    let agent_dir = agent_dir.to_string();
    let incidents_dir = PathBuf::from(incidents_dir);

    std::thread::spawn(move || match ask(&question, &snapshot_json, &agent_dir) {
        Ok(answer) => {
            let incident = crate::incidents::Incident {
                alert_key: &alert_key,
                alert_title: &alert_title,
                alert_message: &alert_message,
                diagnosis: &answer,
            };
            // Record first so the notification can point at the saved
            // file — a multi-paragraph diagnosis doesn't fit in a banner
            // anyway, and osascript can only safely show one line of text.
            let record_result = crate::incidents::record(&incidents_dir, &incident);
            let pointer = match &record_result {
                Ok(path) => format!(" Full report: {}", path.display()),
                Err(_) => String::new(),
            };
            crate::alerts::notify(&crate::alerts::Alert {
                key: "agent_diagnosis".to_string(),
                title: notif_title,
                message: format!("{}{pointer}", teaser(&answer, 180)),
                target: None,
            });

            if let Err(e) = record_result {
                eprintln!("[vigil] failed to write incident journal entry: {e}");
            }
        }
        Err(e) => eprintln!("[vigil] background agent diagnosis failed: {e}"),
    });
}

/// A short, single-paragraph preview of a (possibly multi-paragraph)
/// diagnosis, for the notification banner — the full text goes to the
/// incident journal file instead, which is what `message` here points at.
fn teaser(text: &str, max_len: usize) -> String {
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

fn temp_snapshot_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    // Includes a nanosecond timestamp, not just the PID, because background
    // diagnosis threads (see `diagnose_alert_async`) can call `ask()`
    // concurrently within the same process — a PID-only name would race.
    p.push(format!("vigil-snapshot-{}-{nanos}.json", std::process::id()));
    p
}

/// Pure argv construction, kept separate from `Command` execution so it can
/// be unit-tested without spawning a real process.
fn build_args(question: &str, snapshot_path: &Path, agent_dir: &str) -> Vec<String> {
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

#[cfg(test)]
mod tests {
    use super::*;

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
        assert_eq!(teaser(&text, 200), "Swap is 91% full, pycharm is the top consumer.");
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

}
