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
    std::fs::write(&tmp, snapshot_json).map_err(|e| format!("не удалось записать временный снимок: {e}"))?;

    let args = build_args(question, &tmp, agent_dir);
    let output = Command::new(&args[0]).args(&args[1..]).output();
    let _ = std::fs::remove_file(&tmp);

    let output = output.map_err(|e| format!("не удалось запустить vigil-agent (установлен ли `uv`?): {e}"))?;

    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).trim().to_string())
    } else {
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        Err(if stderr.is_empty() {
            "vigil-agent завершился с ошибкой без сообщения".to_string()
        } else {
            stderr
        })
    }
}

fn temp_snapshot_path() -> PathBuf {
    let mut p = std::env::temp_dir();
    p.push(format!("vigil-snapshot-{}.json", std::process::id()));
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
        let args = build_args("почему мало места на диске?", Path::new("/tmp/snap.json"), "agent");
        assert_eq!(args[0], "uv");
        assert_eq!(args[1], "run");
        assert!(args.windows(2).any(|w| w == ["--project".to_string(), "agent".to_string()]));
        assert!(args.contains(&"/tmp/snap.json".to_string()));
        assert!(args.contains(&"почему мало места на диске?".to_string()));
    }

    #[test]
    fn temp_snapshot_path_is_unique_per_process() {
        let p = temp_snapshot_path();
        assert!(p.to_string_lossy().contains(&std::process::id().to_string()));
    }
}
