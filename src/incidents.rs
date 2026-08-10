//! Persists incident data to a local markdown journal —
//! `<dir>/<date>-<time>-<slug>.md`, one file per alert-worthy incident.
//!
//! A new incident on a journal-worthy alert (see `agent::is_journal_worthy`
//! — not every firing alert key qualifies, to keep this journal bounded)
//! writes a stub immediately (`write_stub`): title, alert key, rule message,
//! and (when the alert was process-specific) the process's command line at
//! fire time — no diagnosis yet. `vigil investigate <key>` appends a `## Agent
//! diagnosis` section later (`append_diagnosis`), and `vigil fix <file>`
//! appends a `## Fix execution` section after that (`append_fix_execution`)
//! if the diagnosis proposed one. Nothing here runs automatically — see
//! `investigate_process.rs`/`fix_process.rs` for what calls these
//! functions and when.
//!
//! Only alert-fired incidents are logged here — the interactive 'a'/'w'
//! flow in the UI is deliberately not, it stays on-screen only.

use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

/// `~/.vigil/incidents` — a fixed, home-relative default so the journal
/// doesn't scatter across whatever directory `vigil` happens to be run
/// from (it's meant to be launched from anywhere, not just its own repo).
pub fn default_dir() -> PathBuf {
    let home = std::env::var_os("HOME")
        .map(PathBuf::from)
        // Coverage exemption (see AGENTS.md's testing section): a real
        // `cargo test` process always has `$HOME` set, and mutating a
        // shared env var from a test would race every other test in this
        // (multi-threaded, same-process) suite that also reads it.
        .unwrap_or_else(|| PathBuf::from("."));
    home.join(".vigil").join("incidents")
}

/// The header-only content an alert firing writes immediately — see the
/// module doc comment for the full lifecycle.
pub struct IncidentStub<'a> {
    pub alert_key: &'a str,
    pub alert_title: &'a str,
    pub alert_message: &'a str,
    pub command: Option<&'a str>,
}

/// Write a new incident file into `dir` (created if missing): title, alert
/// key, rule message, and — when the alert was process-specific and a
/// command line was captured — a `**Command:**` line (see `extract_command`
/// for why this is carried through: it's the pid-reuse defense from
/// incident `2026-08-07-14-20-56-cpu-hog-27339.md`, now surviving the gap
/// until a later `vigil investigate` run). No diagnosis section yet.
/// Returns the path written, which the caller (a notification, an
/// interactive prompt) can point back at.
pub fn write_stub(dir: &Path, stub: &IncidentStub) -> Result<PathBuf, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("failed to create incidents dir {}: {e}", dir.display()))?;

    let filename = format!("{}-{}.md", timestamp_prefix(), slugify(stub.alert_key));
    let path = dir.join(filename);

    let command_line = match stub.command {
        Some(cmd) if !cmd.is_empty() => format!("\n\n**Command:** {cmd}"),
        _ => String::new(),
    };
    let body = format!(
        "# {}\n\n**Alert key:** `{}`\n\n**Rule message:** {}{}\n",
        stub.alert_title, stub.alert_key, stub.alert_message, command_line
    );
    let f = std::fs::File::create(&path).map_err(|e| format!("failed to create {}: {e}", path.display()))?;
    write_or_err(f, &body, &path)?;

    Ok(path)
}

/// Append a `## Agent diagnosis` section to an existing incident file —
/// called once, by `vigil investigate`, after the stub was already
/// written. `diagnosis` is the agent's raw answer text, which may itself
/// contain its own `## Diagnosis`/`## Suggestions`/`## Proposed fix`
/// markdown headings nested under this one.
pub fn append_diagnosis(path: &Path, diagnosis: &str) -> Result<(), String> {
    append_section(path, "Agent diagnosis", diagnosis)
}

/// Append a `## Fix execution` section to an existing incident file —
/// called once, by `vigil fix`, after the execute-agent finished (or
/// aborted partway through) an approved plan.
pub fn append_fix_execution(path: &Path, journal: &str) -> Result<(), String> {
    append_section(path, "Fix execution", journal)
}

fn append_section(path: &Path, heading: &str, body: &str) -> Result<(), String> {
    let f = std::fs::OpenOptions::new()
        .append(true)
        .open(path)
        .map_err(|e| format!("failed to open {} for appending: {e}", path.display()))?;
    let content = format!("\n## {heading}\n\n{}\n", body.trim_end());
    write_or_err(f, &content, path)
}

/// Shared by `write_stub` (a freshly `File::create`d file) and
/// `append_section` (an existing file opened for append) — once a file
/// handle is in hand, a write failure on either is the same fault (disk
/// full, quota, revoked permissions mid-write), so it gets one
/// implementation and one exemption instead of two.
///
/// Coverage exemption (see AGENTS.md's testing section): triggering a write
/// failure on an already-successfully-opened file needs a fault that isn't
/// reasonably reproducible from a unit test.
fn write_or_err(mut f: impl Write, content: &str, path: &Path) -> Result<(), String> {
    f.write_all(content.as_bytes())
        .map_err(|e| format!("failed to write {}: {e}", path.display()))
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
/// markdown `#` — i.e. the `alert_title` `write_stub()` wrote as the H1.
pub fn extract_title(content: &str) -> &str {
    content
        .lines()
        .find(|l| !l.trim().is_empty())
        .map(|l| l.trim_start_matches('#').trim())
        .unwrap_or("(untitled)")
}

/// The text after `**Rule message:**` on its own line — what `vigil
/// investigate` hands the agent as the thing to investigate, since the
/// stub file (not a live `Alert`) is all it has to go on.
pub fn extract_rule_message(content: &str) -> Option<&str> {
    content
        .lines()
        .find_map(|l| l.trim().strip_prefix("**Rule message:**"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// The text after `**Command:**` on its own line, if the stub captured
/// one — the process's full command line at the moment the alert fired,
/// carried through so `vigil investigate` (which may run long after the
/// fact) can still warn the agent about pid reuse. Absent when the alert
/// wasn't process-specific or the command was empty at capture time (see
/// `write_stub`).
pub fn extract_command(content: &str) -> Option<&str> {
    content
        .lines()
        .find_map(|l| l.trim().strip_prefix("**Command:**"))
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// `alert.key` values can contain `:`/other punctuation (e.g. `cpu_hog:1234`)
/// that isn't filename-safe — normalize to lowercase hyphen-separated words.
/// `pub(crate)` (not private) because `investigate.rs` needs the exact same
/// normalization to resolve a CLI-supplied alert key back to its file.
pub(crate) fn slugify(key: &str) -> String {
    key.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_lowercase() } else { '-' })
        .collect::<String>()
        .split('-')
        .filter(|s| !s.is_empty())
        .collect::<Vec<_>>()
        .join("-")
}

fn date_output(format: &str) -> String {
    Command::new("date")
        .arg(format)
        .output()
        .ok()
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        // Coverage exemption (see AGENTS.md's testing section): reaching
        // this fallback needs the system `date` binary itself to be
        // missing or broken, which isn't something to fake from a test
        // without mocking `Command` (the exact thing this file's pure/IO
        // split is meant to avoid needing).
        .unwrap_or_else(|| "unknown-time".to_string())
}

/// Shells out to `date` rather than pulling in a chrono-style dependency
/// just for this — consistent with how `pmset`/`osascript` are already
/// used elsewhere for OS-specific info.
fn timestamp_prefix() -> String {
    date_output("+%Y-%m-%d-%H-%M-%S")
}

/// Same idea, human-readable (`2026-08-09 02:30`) for the `_Approved:
/// ..._` line `fixplan::approved_header` builds.
pub fn human_timestamp() -> String {
    date_output("+%Y-%m-%d %H:%M")
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
    fn write_stub_creates_markdown_with_header_only() {
        let dir = test_dir();
        let stub = IncidentStub {
            alert_key: "high_load",
            alert_title: "vigil: high load",
            alert_message: "Load average 12.0 ...",
            command: None,
        };

        let path = write_stub(&dir, &stub).unwrap();
        assert!(path.exists());
        assert!(path.file_name().unwrap().to_string_lossy().ends_with("-high-load.md"));

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("vigil: high load"));
        assert!(content.contains("`high_load`"));
        assert!(content.contains("Load average 12.0"));
        assert!(!content.contains("## Agent diagnosis"), "stub must not have a diagnosis section yet");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_stub_includes_the_command_line_when_present() {
        let dir = test_dir();
        let stub = IncidentStub {
            alert_key: "cpu_hog:1",
            alert_title: "vigil: cpu hog",
            alert_message: "m",
            command: Some("/usr/bin/pycharm --foo"),
        };
        let path = write_stub(&dir, &stub).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("**Command:** /usr/bin/pycharm --foo"));
        assert_eq!(extract_command(&content), Some("/usr/bin/pycharm --foo"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_stub_omits_the_command_line_when_none() {
        let dir = test_dir();
        let stub = IncidentStub { alert_key: "high_load", alert_title: "t", alert_message: "m", command: None };
        let path = write_stub(&dir, &stub).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("**Command:**"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_stub_omits_the_command_line_when_empty() {
        let dir = test_dir();
        let stub = IncidentStub { alert_key: "cpu_hog:1", alert_title: "t", alert_message: "m", command: Some("") };
        let path = write_stub(&dir, &stub).unwrap();
        let content = std::fs::read_to_string(&path).unwrap();
        assert!(!content.contains("**Command:**"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_stub_creates_missing_directory() {
        let dir = test_dir();
        assert!(!dir.exists());
        let stub = IncidentStub { alert_key: "battery_low", alert_title: "t", alert_message: "m", command: None };
        write_stub(&dir, &stub).unwrap();
        assert!(dir.exists());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn write_stub_fails_when_the_incidents_dir_cannot_be_created() {
        let parent = test_dir();
        std::fs::create_dir_all(&parent).unwrap();
        let mut perms = std::fs::metadata(&parent).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&parent, perms).unwrap();

        let dir = parent.join("cant-create-this");
        let stub = IncidentStub { alert_key: "k", alert_title: "t", alert_message: "m", command: None };
        let result = write_stub(&dir, &stub);

        let mut writable = std::fs::metadata(&parent).unwrap().permissions();
        writable.set_readonly(false);
        std::fs::set_permissions(&parent, writable).unwrap();
        let _ = std::fs::remove_dir_all(&parent);

        assert!(result.is_err());
    }

    #[test]
    fn write_stub_fails_when_the_file_cannot_be_created() {
        let dir = test_dir();
        std::fs::create_dir_all(&dir).unwrap();
        let mut perms = std::fs::metadata(&dir).unwrap().permissions();
        perms.set_readonly(true);
        std::fs::set_permissions(&dir, perms).unwrap();

        let stub = IncidentStub { alert_key: "k", alert_title: "t", alert_message: "m", command: None };
        let result = write_stub(&dir, &stub);

        let mut writable = std::fs::metadata(&dir).unwrap().permissions();
        writable.set_readonly(false);
        std::fs::set_permissions(&dir, writable).unwrap();
        let _ = std::fs::remove_dir_all(&dir);

        assert!(result.is_err());
    }

    #[test]
    fn append_diagnosis_adds_a_heading_and_body_after_the_stub() {
        let dir = test_dir();
        let stub = IncidentStub { alert_key: "cpu_hog:1", alert_title: "vigil: cpu hog", alert_message: "m", command: None };
        let path = write_stub(&dir, &stub).unwrap();

        append_diagnosis(&path, "## Diagnosis\n\nThe culprit is pycharm.").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Agent diagnosis"));
        assert!(content.contains("The culprit is pycharm."));
        let stub_pos = content.find("**Rule message:**").unwrap();
        let diag_pos = content.find("## Agent diagnosis").unwrap();
        assert!(stub_pos < diag_pos);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_diagnosis_fails_for_a_missing_file() {
        let dir = test_dir();
        let missing = dir.join("does-not-exist.md");
        assert!(append_diagnosis(&missing, "text").is_err());
    }

    #[test]
    fn append_fix_execution_adds_its_own_heading_after_diagnosis() {
        let dir = test_dir();
        let stub = IncidentStub { alert_key: "cpu_hog:1", alert_title: "vigil: cpu hog", alert_message: "m", command: None };
        let path = write_stub(&dir, &stub).unwrap();
        append_diagnosis(&path, "## Diagnosis\n\ntext\n\n## Proposed fix\n\n```json\n{}\n```").unwrap();

        append_fix_execution(&path, "_Approved: 2026-08-09 02:30 (steps 1 of 1)_\n\n1. done").unwrap();

        let content = std::fs::read_to_string(&path).unwrap();
        assert!(content.contains("## Fix execution"));
        assert!(content.contains("1. done"));
        let diag_pos = content.find("## Agent diagnosis").unwrap();
        let fix_pos = content.find("## Fix execution").unwrap();
        assert!(diag_pos < fix_pos);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn append_fix_execution_fails_for_a_missing_file() {
        let dir = test_dir();
        let missing = dir.join("does-not-exist.md");
        assert!(append_fix_execution(&missing, "text").is_err());
    }

    #[test]
    fn extract_rule_message_reads_the_field_line() {
        let content = "# t\n\n**Alert key:** `k`\n\n**Rule message:** Load average 12.0 (threshold 24.0).\n";
        assert_eq!(extract_rule_message(content), Some("Load average 12.0 (threshold 24.0)."));
    }

    #[test]
    fn extract_rule_message_is_none_when_the_field_is_absent() {
        assert_eq!(extract_rule_message("# t\n\nno rule message field here\n"), None);
    }

    #[test]
    fn extract_command_reads_the_field_line() {
        let content = "# t\n\n**Alert key:** `k`\n\n**Rule message:** m\n\n**Command:** /usr/bin/foo --bar\n";
        assert_eq!(extract_command(content), Some("/usr/bin/foo --bar"));
    }

    #[test]
    fn extract_command_is_none_when_the_field_is_absent() {
        assert_eq!(extract_command("# t\n\n**Rule message:** m\n"), None);
    }

    #[test]
    fn human_timestamp_matches_yyyy_mm_dd_hh_mm() {
        let ts = human_timestamp();
        assert_eq!(ts.len(), 16, "unexpected format: {ts}");
        assert_eq!(ts.chars().nth(4), Some('-'));
        assert_eq!(ts.chars().nth(7), Some('-'));
        assert_eq!(ts.chars().nth(10), Some(' '));
        assert_eq!(ts.chars().nth(13), Some(':'));
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
