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
