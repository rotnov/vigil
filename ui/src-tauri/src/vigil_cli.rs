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
