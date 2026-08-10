//! Pure logic for `vigil investigate <alert-key>`: resolving the CLI's
//! alert-key argument back to the incident file `incidents::write_stub`
//! created for it. The actual snapshot-taking, agent spawn, and file
//! append happen in `investigate_process.rs` — this file has no IO beyond
//! `incidents::list` (already itself a thin, tested `read_dir` wrapper),
//! so it's the one part of the `vigil investigate` path that's fully
//! unit-tested.

use std::path::{Path, PathBuf};

/// The most recent incident file in `dir` whose filename matches
/// `alert_key`'s slug — same substring-match convention `vigil incidents
/// --show` already uses, but keyed by the alert key's normalized form
/// (`incidents::slugify`) rather than an arbitrary user-typed substring,
/// since `alert_key` comes verbatim from the CLI arg and needs the exact
/// same normalization `write_stub` applied when naming the file.
pub fn resolve_incident_file(dir: &Path, alert_key: &str) -> Result<PathBuf, String> {
    let files = crate::incidents::list(dir)?;
    let slug = crate::incidents::slugify(alert_key);
    files
        .into_iter()
        .filter(|p| p.file_name().is_some_and(|n| n.to_string_lossy().contains(&slug)))
        .last()
        .ok_or_else(|| format!("no incident found for alert key \"{alert_key}\" in {}", dir.display()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("vigil-investigate-test-{}-{n}", std::process::id()));
        p
    }

    #[test]
    fn resolve_incident_file_finds_the_most_recent_match() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-08-09-01-00-00-cpu-hog-37489.md"), "x").unwrap();
        std::fs::write(dir.join("2026-08-09-02-00-00-cpu-hog-37489.md"), "x").unwrap();
        std::fs::write(dir.join("2026-08-09-01-30-00-high-load.md"), "x").unwrap();

        let found = resolve_incident_file(&dir, "cpu_hog:37489").unwrap();
        assert_eq!(found.file_name().unwrap().to_string_lossy(), "2026-08-09-02-00-00-cpu-hog-37489.md");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_incident_file_errors_when_nothing_matches() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-08-09-01-00-00-high-load.md"), "x").unwrap();

        let result = resolve_incident_file(&dir, "cpu_hog:99999");
        assert!(result.is_err());
        assert!(result.unwrap_err().contains("cpu_hog:99999"));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn resolve_incident_file_errors_for_a_missing_directory() {
        let dir = test_dir();
        let result = resolve_incident_file(&dir, "cpu_hog:1");
        assert!(result.is_err());
    }
}
