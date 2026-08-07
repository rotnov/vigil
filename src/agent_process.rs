//! The actual OS-boundary glue for talking to `vigil_agent`: writing the
//! snapshot to a temp file, spawning `uv run vigil-agent ask`, and (for
//! auto-triggered diagnoses) doing that in a background thread. Kept in its
//! own file, excluded from the coverage gate (see AGENTS.md's testing
//! section and `--ignore-filename-regex` in the test command) — every
//! branch here either spawns a real process that costs real tokens or
//! blocks on I/O, neither of which a unit test should trigger. The actual
//! decision logic (question wording, arg construction, output
//! interpretation) lives in `agent.rs` and is fully unit-tested there; this
//! file is intentionally as thin as possible around it.

use crate::agent::{build_args, build_diagnosis_question, interpret_output, is_auto_diagnose_worthy, teaser, temp_snapshot_path};
use std::path::PathBuf;

/// Write the snapshot to a temp file, invoke the agent CLI, and return its
/// stdout (trimmed) or an error message suitable for display in the UI.
pub fn ask(question: &str, snapshot_json: &str, agent_dir: &str) -> Result<String, String> {
    let tmp = temp_snapshot_path();
    std::fs::write(&tmp, snapshot_json).map_err(|e| format!("failed to write temp snapshot: {e}"))?;

    let args = build_args(question, &tmp, agent_dir);
    let output = std::process::Command::new(&args[0]).args(&args[1..]).output();
    let _ = std::fs::remove_file(&tmp);

    let output = output.map_err(|e| format!("failed to launch vigil-agent (is `uv` installed?): {e}"))?;
    interpret_output(&output)
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
    watch_log_path: Option<&str>,
) {
    if !is_auto_diagnose_worthy(&alert.key) {
        return;
    }

    eprintln!(
        "[vigil] diagnosing [{}] — recent_context: {}",
        alert.key,
        recent_context.unwrap_or("(none)")
    );
    let question = build_diagnosis_question(&alert.message, recent_context, watch_log_path, alert.command.as_deref());
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
                command: None,
            });

            if let Err(e) = record_result {
                eprintln!("[vigil] failed to write incident journal entry: {e}");
            }
        }
        Err(e) => eprintln!("[vigil] background agent diagnosis failed: {e}"),
    });
}
