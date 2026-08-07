//! Bridge to the `vigil_agent` Python package (Claude Agent SDK).
//!
//! `vigil` itself never talks to the network or an LLM — it only shells
//! out to `uv run vigil-agent ask` with a snapshot + a question and prints
//! whatever text comes back. All actual reasoning happens in the Python
//! process.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

/// If two alerts about the same process fire within this window (e.g.
/// `cpu_hog:<pid>` immediately followed by `high_load` whose top consumer
/// is that same process), only the first spawns a background diagnosis.
/// Wide enough to cover a burst of correlated rule-firings (observed
/// ~35s apart in practice), narrower than the default alert
/// re-notify cooldown (5 min) which governs repeats of one specific alert.
const COALESCE_WINDOW: Duration = Duration::from_secs(120);

/// Prevents redundant background investigations when multiple alerts name
/// the same process in a short window — each diagnosis spawns a real `uv
/// run` + Claude Agent SDK investigation (real CPU/token/wall-clock cost),
/// so three near-simultaneous alerts about one process is three times the
/// cost for one root cause. Coalescing is keyed on the *process*, not on
/// time alone: a different process firing in the same window (e.g. a
/// genuinely unrelated CPU hog) still gets investigated — a global
/// time-based cooldown would have swallowed exactly that case in the field
/// data that motivated this (2026-08-07: `cpu_hog:64955` turned out to be
/// an independent finding, not a rediscovery of the concurrent pycharm
/// alerts).
pub struct DiagnosisCoalescer {
    last_spawned: HashMap<String, Instant>,
}

impl DiagnosisCoalescer {
    pub fn new() -> Self {
        Self { last_spawned: HashMap::new() }
    }

    /// Returns true if a diagnosis should be spawned for `target` — and, if
    /// so, records `now` so a subsequent call for the same target within
    /// `window` returns false. Alerts with no identifiable target
    /// (`target: None`) always proceed, since there's nothing to coalesce
    /// against.
    fn try_claim(&mut self, target: Option<&str>, window: Duration, now: Instant) -> bool {
        let Some(target) = target else { return true };
        let ready = match self.last_spawned.get(target) {
            Some(t) => now.duration_since(*t) >= window,
            None => true,
        };
        if ready {
            self.last_spawned.insert(target.to_string(), now);
        }
        ready
    }
}

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

/// If `alert` is worth it — and no diagnosis was already spawned for the
/// same target process within `COALESCE_WINDOW` (see `coalescer`) — ask the
/// agent to investigate in a background thread and fire a follow-up
/// notification with the answer once ready. Never blocks the caller. The
/// agent has real (read-only) investigation tools here — same contract as
/// the interactive 'a' flow: it can look around (logs, `sample`, `vm_stat`,
/// ...) but never modify anything. A failed diagnosis is logged, not
/// surfaced as a notification, since the plain rule-based alert already
/// fired.
pub fn maybe_diagnose_alert_async(
    alert: &crate::alerts::Alert,
    snapshot_json: &str,
    agent_dir: &str,
    incidents_dir: &str,
    recent_context: Option<&str>,
    coalescer: &mut DiagnosisCoalescer,
    now: Instant,
) {
    if !is_auto_diagnose_worthy(&alert.key) {
        return;
    }
    if !coalescer.try_claim(alert.target.as_deref(), COALESCE_WINDOW, now) {
        eprintln!(
            "[vigil] skipping diagnosis for [{}] — already investigating {:?} within the last {}s",
            alert.key,
            alert.target,
            COALESCE_WINDOW.as_secs()
        );
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

    #[test]
    fn coalescer_claims_first_call_for_a_target() {
        let mut c = DiagnosisCoalescer::new();
        assert!(c.try_claim(Some("pycharm"), Duration::from_secs(120), Instant::now()));
    }

    #[test]
    fn coalescer_rejects_second_call_for_same_target_within_window() {
        let mut c = DiagnosisCoalescer::new();
        let t0 = Instant::now();
        assert!(c.try_claim(Some("pycharm"), Duration::from_secs(120), t0));
        // The high_load alert firing 35s after cpu_hog:<pid> for the same
        // process — the exact scenario that motivated this coalescer.
        assert!(!c.try_claim(Some("pycharm"), Duration::from_secs(120), t0 + Duration::from_secs(35)));
    }

    #[test]
    fn coalescer_allows_same_target_again_once_window_elapses() {
        let mut c = DiagnosisCoalescer::new();
        let t0 = Instant::now();
        assert!(c.try_claim(Some("pycharm"), Duration::from_secs(120), t0));
        assert!(c.try_claim(Some("pycharm"), Duration::from_secs(120), t0 + Duration::from_secs(200)));
    }

    #[test]
    fn coalescer_treats_different_targets_independently() {
        let mut c = DiagnosisCoalescer::new();
        let t0 = Instant::now();
        assert!(c.try_claim(Some("pycharm"), Duration::from_secs(120), t0));
        // A genuinely unrelated process (e.g. the Devin Helper finding from
        // 2026-08-07) must still get investigated in the same window.
        assert!(c.try_claim(Some("Devin Helper (Renderer)"), Duration::from_secs(120), t0));
    }

    #[test]
    fn coalescer_always_claims_when_target_is_unknown() {
        let mut c = DiagnosisCoalescer::new();
        let t0 = Instant::now();
        assert!(c.try_claim(None, Duration::from_secs(120), t0));
        assert!(c.try_claim(None, Duration::from_secs(120), t0));
    }
}
