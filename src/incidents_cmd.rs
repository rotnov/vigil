//! `vigil incidents` — lists or shows saved auto-diagnosis reports so the
//! journal can be checked from a plain shell, without an already-open TUI
//! session (a TUI can't pop itself open on a push notification).
//!
//! Returns an exit code instead of calling `std::process::exit` directly —
//! that keeps every branch (including the error paths) callable from a unit
//! test; only `main.rs`'s thin wrapper actually exits the process.

pub fn run(dir: &str, show: Option<&str>, limit: usize) -> i32 {
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
                    print!("{content}");
                    0
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
        assert_eq!(run(path.to_str().unwrap(), None, 20), 1);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn show_matching_a_broken_symlink_fails_to_read_and_returns_an_error_code() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let link = dir.join("2026-08-07-11-00-00-dangling.md");
        std::os::unix::fs::symlink(dir.join("does-not-exist"), &link).unwrap();

        assert_eq!(run(dir.to_str().unwrap(), Some("dangling"), 20), 1);
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
        assert_eq!(run(dir.to_str().unwrap(), None, 20), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn missing_directory_lists_as_empty_not_an_error() {
        let dir = test_dir();
        assert_eq!(run(dir.to_str().unwrap(), None, 20), 0);
    }

    #[test]
    fn lists_recent_incidents_most_recent_first_up_to_limit() {
        let dir = test_dir();
        write_incident(&dir, "2026-08-07-11-00-00-a.md", "first");
        write_incident(&dir, "2026-08-07-12-00-00-b.md", "second");
        write_incident(&dir, "2026-08-07-13-00-00-c.md", "third");

        assert_eq!(run(dir.to_str().unwrap(), None, 20), 0);
        assert_eq!(run(dir.to_str().unwrap(), None, 1), 0);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn show_with_no_match_returns_an_error_code() {
        let dir = test_dir();
        write_incident(&dir, "2026-08-07-11-00-00-a.md", "first");
        assert_eq!(run(dir.to_str().unwrap(), Some("nonexistent"), 20), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn show_with_exactly_one_match_prints_it_and_succeeds() {
        let dir = test_dir();
        write_incident(&dir, "2026-08-07-11-00-00-cpu-hog.md", "cpu hog");
        assert_eq!(run(dir.to_str().unwrap(), Some("cpu-hog"), 20), 0);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn show_with_multiple_matches_returns_an_error_code() {
        let dir = test_dir();
        write_incident(&dir, "2026-08-07-11-00-00-cpu-hog-111.md", "a");
        write_incident(&dir, "2026-08-07-12-00-00-cpu-hog-222.md", "b");
        assert_eq!(run(dir.to_str().unwrap(), Some("cpu-hog"), 20), 1);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
