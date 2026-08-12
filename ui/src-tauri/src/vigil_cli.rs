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
///
/// `path` is a caller-supplied *full* file path (from `incident.js`'s
/// `getIncidentPath()`, itself the `?path=` URL param `open_incident_window`
/// set from a full `PathBuf`), but `vigil incidents --show` only
/// substring-matches a candidate's bare filename
/// (`src/incidents_cmd.rs::run`: `p.file_name()...contains(query)`) — a
/// full path is always longer than the filename it's supposed to identify,
/// so passing it through unchanged as `--show`'s query can never match
/// anything. `--show` is given just `path`'s filename component instead
/// (falling back to the whole string if `path` has no `/`, matching
/// `vigil_cli`'s own convention elsewhere for a path with no parent) — the
/// stub/journal filenames `incidents::write_stub` generates are already
/// unique per incident (a `<date>-<time>-<slug>` prefix), so the basename
/// alone is exact enough to resolve back to one file.
pub fn build_show_json_args(vigil_bin: &str, incidents_dir: &str, path: &str) -> Vec<String> {
    let query = path.rsplit('/').next().unwrap_or(path);
    vec![
        vigil_bin.to_string(),
        "incidents".to_string(),
        "--dir".to_string(),
        incidents_dir.to_string(),
        "--show".to_string(),
        query.to_string(),
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
        let args = build_show_json_args("vigil", "/tmp/incidents", "x.md");
        assert_eq!(args, vec!["vigil", "incidents", "--dir", "/tmp/incidents", "--show", "x.md", "--json"]);
    }

    #[test]
    fn build_show_json_args_reduces_a_full_path_to_its_filename() {
        // The real bug this guards against: `vigil incidents --show`
        // substring-matches only a candidate's bare filename
        // (`src/incidents_cmd.rs::run`), so passing the full path straight
        // through as the query can never match -- confirmed against the
        // real `vigil` binary during this task's manual smoke test (every
        // `read_incident_json` call failed with "no incident matches").
        let args = build_show_json_args(
            "vigil",
            "/Users/denis/.vigil/incidents",
            "/Users/denis/.vigil/incidents/2026-08-12-00-00-00-cpu-hog-1.md",
        );
        assert_eq!(
            args,
            vec![
                "vigil",
                "incidents",
                "--dir",
                "/Users/denis/.vigil/incidents",
                "--show",
                "2026-08-12-00-00-00-cpu-hog-1.md",
                "--json",
            ]
        );
    }

    #[test]
    fn build_show_json_args_falls_back_to_the_whole_string_with_no_slash() {
        let args = build_show_json_args("vigil", "/tmp/incidents", "x.md");
        assert_eq!(args, vec!["vigil", "incidents", "--dir", "/tmp/incidents", "--show", "x.md", "--json"]);
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
