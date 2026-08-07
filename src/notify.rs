//! The actual native macOS notification shell-out.
//!
//! Excluded from the coverage gate (see AGENTS.md's testing section and the
//! `--ignore-filename-regex` in the test command): calling this in a unit
//! test would pop a real, visible notification on the maintainer's own Mac
//! on every `cargo test` run. The `osascript` `Command` call is the entire
//! function body — the part that actually has logic, quoting the message
//! safely for a single-line `-e` script, is `alerts::osa_quote` and is
//! fully unit-tested there.

/// Fire a native macOS notification. Purely informational — never runs any
/// remediation itself. The user acts on the suggestion manually (or, in a
/// future agent-driven mode, only after explicit confirmation).
pub fn notify(alert: &crate::alerts::Alert) {
    let script = format!(
        "display notification {} with title {} sound name \"Glass\"",
        crate::alerts::osa_quote(&alert.message),
        crate::alerts::osa_quote(&alert.title)
    );
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output();
}
