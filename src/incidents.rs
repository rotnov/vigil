//! Persists background agent diagnoses to a local incident journal —
//! `<dir>/<date>-<time>-<slug>.md`, one file per auto-triggered diagnosis.
//!
//! Only the automatic background diagnoses (see
//! `agent::maybe_diagnose_alert_async`) are logged here — the interactive
//! 'a' flow in the UI is deliberately not, it stays on-screen only.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `~/.vigil/incidents` — a fixed, home-relative default so the journal
/// doesn't scatter across whatever directory `vigil` happens to be run
/// from (it's meant to be launched from anywhere, not just its own repo).
pub fn default_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".vigil").join("incidents")
}

pub struct Incident<'a> {
    pub alert_key: &'a str,
    pub alert_title: &'a str,
    pub alert_message: &'a str,
    pub diagnosis: &'a str,
}

/// Write one markdown incident file into `dir` (created if missing).
/// Returns the path written.
pub fn record(dir: &Path, incident: &Incident) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("failed to create incidents dir {}: {e}", dir.display()))?;

    let filename = format!("{}-{}.md", timestamp_prefix(), slugify(incident.alert_key));
    let path = dir.join(filename);

    let body = render_markdown(incident);
    let mut f = std::fs::File::create(&path).map_err(|e| format!("failed to create {}: {e}", path.display()))?;
    f.write_all(body.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))?;

    Ok(path)
}

/// List incident files in `dir`, oldest first (filenames sort chronologically
/// since they're prefixed `YYYY-MM-DD-HH-MM-SS`). Empty (not an error) if the
/// directory doesn't exist yet — a fresh install has no incidents.
pub fn list(dir: &Path) -> Result<Vec<PathBuf>, String> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries: Vec<PathBuf> = std::fs::read_dir(dir)
        .map_err(|e| format!("failed to read {}: {e}", dir.display()))?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| p.extension().is_some_and(|ext| ext == "md"))
        .collect();
    entries.sort();
    Ok(entries)
}

/// The first non-empty line of an incident file, stripped of its leading
/// markdown `#` — i.e. the `alert_title` `record()` wrote as the H1.
pub fn extract_title(content: &str) -> &str {
    content
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim_start_matches('#').trim())
        .unwrap_or("(untitled)")
}

fn render_markdown(incident: &Incident) -> String {
    format!(
        "# {}\n\n**Alert key:** `{}`\n\n**Rule message:** {}\n\n## Agent diagnosis\n\n{}\n",
        incident.alert_title, incident.alert_key, incident.alert_message, incident.diagnosis
    )
}

/// `alert.key` values can contain `:`/other punctuation (e.g. `cpu_hog:1234`)
/// that isn't filename-safe — normalize to lowercase hyphen-separated words.
fn slugify(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

/// Shells out to `date` rather than pulling in a chrono-style dependency
/// just for this — consistent with how `pmset`/`osascript` are already
/// used elsewhere for OS-specific info.
fn timestamp_prefix() -> String {
    Command::new("date")
        .arg("+%Y-%m-%d-%H-%M-%S")
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "unknown-time".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU32, Ordering};

    static COUNTER: AtomicU32 = AtomicU32::new(0);

    fn test_dir() -> PathBuf {
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let mut p = std::env::temp_dir();
        p.push(format!("vigil-incidents-test-{}-{n}", std::process::id()));
        p
    }

    #[test]
    fn slugify_normalizes_punctuation_and_case() {
        assert_eq!(slugify("high_load"), "high-load");
        assert_eq!(slugify("cpu_hog:1234"), "cpu-hog-1234");
        assert_eq!(slugify("battery_low"), "battery-low");
    }

    #[test]
    fn record_writes_markdown_with_expected_content_and_filename() {
        let dir = test_dir();
        let incident = Incident {
            alert_key: "high_load",
            alert_title: "vigil: high load",
            alert_message: "Load average 12.0 ...",
            diagnosis: "The culprit is pycharm.",
        };

        let path = record(&dir, &incident).unwrap();
        assert!(path.exists());
        assert!(
            path.file_name().unwrap().to_string_lossy().ends_with("-high-load.md"),
            "unexpected filename: {:?}",
            path.file_name()
        );

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("vigil: high load"));
        assert!(content.contains("`high_load`"));
        assert!(content.contains("Load average 12.0"));
        assert!(content.contains("The culprit is pycharm."));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn record_creates_missing_directory() {
        let dir = test_dir();
        assert!(!dir.exists());
        let incident = Incident {
            alert_key: "battery_low",
            alert_title: "t",
            alert_message: "m",
            diagnosis: "d",
        };
        record(&dir, &incident).unwrap();
        assert!(dir.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn default_dir_is_under_home() {
        let dir = default_dir();
        assert!(dir.ends_with(".vigil/incidents"));
    }

    #[test]
    fn list_is_empty_for_missing_directory() {
        let dir = test_dir();
        assert!(!dir.exists());
        assert_eq!(list(&dir).unwrap(), Vec::<PathBuf>::new());
    }

    #[test]
    fn list_returns_md_files_oldest_first_ignoring_other_files() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("2026-08-07-12-00-44-cpu-hog.md"), "x").unwrap();
        std::fs::write(dir.join("2026-08-07-11-49-35-high-load.md"), "x").unwrap();
        std::fs::write(dir.join(".DS_Store"), "x").unwrap();

        let files = list(&dir).unwrap();
        let names: Vec<_> = files.iter().map(|p| p.file_name().unwrap().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["2026-08-07-11-49-35-high-load.md", "2026-08-07-12-00-44-cpu-hog.md"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn extract_title_strips_markdown_heading() {
        assert_eq!(extract_title("# vigil: high load\n\nbody"), "vigil: high load");
    }

    #[test]
    fn extract_title_falls_back_when_no_content() {
        assert_eq!(extract_title(""), "(untitled)");
    }
}
