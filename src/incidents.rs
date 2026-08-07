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
}
