//! `vigil incidents` — lists or shows saved incident stubs (and, when
//! `vigil investigate`/`vigil fix` have since run against them, their
//! appended diagnosis/fix sections) so the journal can be checked from a
//! plain shell, without an already-open TUI session (a TUI can't pop itself
//! open on a push notification).
//!
//! Returns an exit code instead of calling `std::process::exit` directly —
//! that keeps every branch (including the error paths) callable from a unit
//! test; only `main.rs`'s thin wrapper actually exits the process.

/// The same fields `vigil incidents --show <file>` prints as raw markdown,
/// pulled apart into one structured value instead — for `--json`, so a
/// caller (namely `vigil-ui`'s Tauri backend) doesn't have to re-parse
/// markdown by hand. `proposed_fix` embeds `fixplan::Plan` directly (it
/// already derives `Serialize`) rather than re-encoding its JSON as a
/// string.
#[derive(serde::Serialize)]
pub struct IncidentJson<'a> {
    pub title: &'a str,
    pub alert_key: Option<&'a str>,
    pub rule_message: Option<&'a str>,
    pub command: Option<&'a str>,
    pub diagnosis: Option<&'a str>,
    pub proposed_fix: Option<crate::fixplan::Plan>,
    pub fix_execution: Option<&'a str>,
}

/// Pure — composes `incidents::extract_*`/`fixplan::extract_proposed_fix_json`
/// into one value. A `## Proposed fix` block that fails to parse (malformed
/// JSON, an unknown category) is treated the same as "no proposed fix" here
/// — `--json`'s job is to hand back what's actually usable, not to surface
/// a parse error for a field nothing requires.
pub fn build_incident_json(content: &str) -> IncidentJson<'_> {
    IncidentJson {
        title: crate::incidents::extract_title(content),
        alert_key: crate::incidents::extract_alert_key(content),
        rule_message: crate::incidents::extract_rule_message(content),
        command: crate::incidents::extract_command(content),
        diagnosis: crate::incidents::extract_diagnosis(content),
        proposed_fix: crate::fixplan::extract_proposed_fix_json(content)
            .and_then(|j| crate::fixplan::parse_plan(j).ok()),
        fix_execution: crate::incidents::extract_fix_execution(content),
    }
}

pub fn run(dir: &str, show: Option<&str>, limit: usize, json: bool) -> i32 {
    let path = std::path::Path::new(dir);
    let files = match crate::incidents::list(path) {
        Ok(files) => files,
        Err(e) => {
            eprintln!("[vigil] {e}");
            return 1;
        }
    };

    if let Some(query) = show {
        let matches: Vec<_> = files
            .iter()
            .filter(|p| p.file_name().unwrap().to_string_lossy().contains(query))
            .collect();
        return match matches.as_slice() {
            [] => {
                eprintln!("[vigil] no incident matches \"{query}\" in {}", path.display());
                1
            }
            [single] => match std::fs::read_to_string(single) {
                Ok(content) => {
                    if json {
                        match serde_json::to_string(&build_incident_json(&content)) {
                            Ok(j) => {
                                println!("{j}");
                                0
                            }
                            Err(e) => {
                                eprintln!("[vigil] failed to serialize {}: {e}", single.display());
                                1
                            }
                        }
                    } else {
                        print!("{content}");
                        0
                    }
                }
                Err(e) => {
                    eprintln!("[vigil] failed to read {}: {e}", single.display());
                    1
                }
            },
            many => {
                eprintln!("[vigil] \"{query}\" matches {} incidents, be more specific:", many.len());
                for m in many {
                    eprintln!("  {}", m.file_name().unwrap().to_string_lossy());
                }
                1
            }
        };
    }

    if files.is_empty() {
        println!("No incidents recorded yet in {}", path.display());
        return 0;
    }

    for f in files.iter().rev().take(limit) {
        let title = std::fs::read_to_string(f)
            .map(|c| crate::incidents::extract_title(&c).to_string())
            .unwrap_or_else(|_| "(unreadable)".to_string());
        println!("{}  {}", f.file_name().unwrap().to_string_lossy(), title);
    }
    0
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("vigil-incidents-cmd-test-{}-{n}", std::process::id()));
        p
    }

    fn write_incident(dir: &std::path::Path, filename: &str, title: &str) {
        std::fs::create_dir_all(dir).unwrap();
        std::fs::write(dir.join(filename), format!("# {title}\n\nbody")).unwrap();
    }

    #[test]
    fn a_dir_argument_that_is_actually_a_file_returns_an_error_code() {
        // `incidents::list` opens `dir` with `read_dir`, which fails when
        // the path exists but isn't a directory -- exercises the top-level
        // `Err` branch without needing to fake a permissions failure.
        let path = test_dir();
        std::fs::write(&path, "not a directory").unwrap();
        assert_eq!(run(path.to_str().unwrap(), None, 20, false), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn show_matching_a_broken_symlink_fails_to_read_and_returns_an_error_code() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("2026-08-07-11-00-00-dangling.md");
        std::os::unix::fs::symlink(dir.join("does-not-exist"), &link).unwrap();

        assert_eq!(run(dir.to_str().unwrap(), Some("dangling"), 20, false), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn listing_an_unreadable_incident_shows_a_placeholder_title_not_an_error() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("2026-08-07-11-00-00-dangling.md");
        std::os::unix::fs::symlink(dir.join("does-not-exist"), &link).unwrap();

        // Listing (unlike `--show`) never fails outright on one bad entry --
        // it prints a placeholder title and keeps going.
        assert_eq!(run(dir.to_str().unwrap(), None, 20, false), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_directory_lists_as_empty_not_an_error() {
        let dir = test_dir();
        assert_eq!(run(dir.to_str().unwrap(), None, 20, false), 0);
    }

    #[test]
    fn lists_recent_incidents_most_recent_first_up_to_limit() {
        let dir = test_dir();
        write_incident(&dir, "2026-08-07-11-00-00-a.md", "first");
        write_incident(&dir, "2026-08-07-12-00-00-b.md", "second");
        write_incident(&dir, "2026-08-07-13-00-00-c.md", "third");

        assert_eq!(run(dir.to_str().unwrap(), None, 20, false), 0);
        assert_eq!(run(dir.to_str().unwrap(), None, 1, false), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn show_with_no_match_returns_an_error_code() {
        let dir = test_dir();
        write_incident(&dir, "2026-08-07-11-00-00-a.md", "first");
        assert_eq!(run(dir.to_str().unwrap(), Some("nonexistent"), 20, false), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn show_with_exactly_one_match_prints_it_and_succeeds() {
        let dir = test_dir();
        write_incident(&dir, "2026-08-07-11-00-00-cpu-hog.md", "cpu hog");
        assert_eq!(run(dir.to_str().unwrap(), Some("cpu-hog"), 20, false), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn show_with_multiple_matches_returns_an_error_code() {
        let dir = test_dir();
        write_incident(&dir, "2026-08-07-11-00-00-cpu-hog-111.md", "a");
        write_incident(&dir, "2026-08-07-12-00-00-cpu-hog-222.md", "b");
        assert_eq!(run(dir.to_str().unwrap(), Some("cpu-hog"), 20, false), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn show_with_json_prints_structured_json_instead_of_raw_markdown() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("2026-08-09-01-00-00-cpu-hog-1.md"),
            "# vigil: cpu hog\n\n**Alert key:** `cpu_hog:1`\n\n**Rule message:** m\n\n## Agent diagnosis\n\ntext\n",
        )
        .unwrap();
        assert_eq!(run(dir.to_str().unwrap(), Some("cpu-hog-1"), 20, true), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn build_incident_json_composes_every_field() {
        let content = "# vigil: cpu hog\n\n**Alert key:** `cpu_hog:1`\n\n**Rule message:** m\n\n**Command:** /usr/bin/foo\n\n## Agent diagnosis\n\ndiag text\n\n## Proposed fix\n\n```json\n{\"plan\": [{\"category\": \"kill_process\", \"description\": \"d\", \"target_hint\": \"h\"}]}\n```\n\n## Fix execution\n\n_Approved: 2026-08-09 02:30 (steps 1 of 1)_\n\n1. done\n";
        let json = build_incident_json(content);
        assert_eq!(json.title, "vigil: cpu hog");
        assert_eq!(json.alert_key, Some("cpu_hog:1"));
        assert_eq!(json.rule_message, Some("m"));
        assert_eq!(json.command, Some("/usr/bin/foo"));
        assert_eq!(json.diagnosis, Some("diag text"));
        assert!(json.proposed_fix.is_some());
        assert_eq!(json.proposed_fix.unwrap().plan.len(), 1);
        assert!(json.fix_execution.unwrap().starts_with("_Approved:"));
    }

    #[test]
    fn build_incident_json_treats_a_malformed_proposed_fix_as_absent() {
        let content = "# t\n\n## Agent diagnosis\n\ntext\n\n## Proposed fix\n\n```json\nnot valid json\n```\n";
        let json = build_incident_json(content);
        assert!(json.proposed_fix.is_none());
    }
}
