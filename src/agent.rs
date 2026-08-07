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
pub fn maybe_diagnose_alert_async(alert: &crate::alerts::Alert, snapshot_json: &str, agent_dir: &str) {
    if !is_auto_diagnose_worthy(&alert.key) {
        return;
    }

    let question = format!(
        "A monitoring rule just fired: \"{}\". Investigate the likely cause — check beyond the \
         snapshot if useful (e.g. logs, `sample` a hot pid, thermal state) — and suggest what to \
         check or do next.",
        alert.message
    );
    let title = format!("{} — agent diagnosis", alert.title);
    let snapshot_json = snapshot_json.to_string();
    let agent_dir = agent_dir.to_string();

    std::thread::spawn(move || match ask(&question, &snapshot_json, &agent_dir) {
        Ok(answer) => crate::alerts::notify(&crate::alerts::Alert {
            key: "agent_diagnosis".to_string(),
            title,
            message: answer,
        }),
        Err(e) => eprintln!("[vigil] background agent diagnosis failed: {e}"),
    });
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
