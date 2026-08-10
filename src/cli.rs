//! Clap argument definitions — kept separate from `main.rs` so the
//! subcommand dispatch there stays a short, obviously-correct list of
//! delegations to the actual per-command logic (each already covered where
//! it lives: `snapshot.rs`, `watch.rs`, `ui.rs`, `incidents_cmd.rs`,
//! `menubar.rs`).

use clap::{Parser, Subcommand};

#[derive(Parser)]
#[command(name = "vigil", version, about = "Lightweight system metrics collector — hands snapshots to an LLM agent for diagnosis")]
pub struct Cli {
    #[command(subcommand)]
    pub command: Commands,
}

#[derive(Subcommand)]
pub enum Commands {
    /// Print one JSON snapshot to stdout and exit
    Snapshot {
        /// Number of top processes to include (by CPU and by memory)
        #[arg(long, default_value_t = 10)]
        top: usize,
    },
    /// Continuously sample and append JSON Lines to a log file
    Watch {
        /// Seconds between samples
        #[arg(long, default_value_t = 5)]
        interval: u64,
        /// Number of samples to take (0 = run forever)
        #[arg(long, default_value_t = 0)]
        count: u64,
        /// Output file (JSONL, appended)
        #[arg(long, default_value = "vigil.jsonl")]
        out: String,
        /// Number of top processes to include (by CPU and by memory)
        #[arg(long, default_value_t = 10)]
        top: usize,
        /// Disable native macOS notifications on detected anomalies
        #[arg(long, default_value_t = false)]
        no_notify: bool,
        /// Minimum seconds between repeat notifications for the same issue
        #[arg(long, default_value_t = 300)]
        cooldown_secs: u64,
        /// Directory for the incident journal (markdown, one stub file per alert-worthy incident)
        #[arg(long, default_value_t = default_incidents_dir())]
        incidents_dir: String,
        /// Health status file, refreshed every tick, that `vigil menubar` polls
        #[arg(long, default_value_t = default_status_file())]
        status_file: String,
    },
    /// Live terminal dashboard (CPU/mem sparklines + top processes)
    Ui {
        /// Seconds between refreshes
        #[arg(long, default_value_t = 1)]
        interval: u64,
        /// Number of processes shown in the table
        #[arg(long, default_value_t = 15)]
        top: usize,
        /// Disable native macOS notifications on detected anomalies
        #[arg(long, default_value_t = false)]
        no_notify: bool,
        /// Minimum seconds between repeat notifications for the same issue
        #[arg(long, default_value_t = 300)]
        cooldown_secs: u64,
        /// Path to the vigil_agent project directory (for the in-UI "ask" feature)
        #[arg(long, default_value = "agent")]
        agent_dir: String,
        /// Directory for the auto-diagnosis incident journal (markdown, one file per diagnosis)
        #[arg(long, default_value_t = default_incidents_dir())]
        incidents_dir: String,
    },
    /// Browse the auto-diagnosis incident journal (no TUI session required)
    Incidents {
        /// Directory the incident journal is stored in
        #[arg(long, default_value_t = default_incidents_dir())]
        dir: String,
        /// Print the full contents of one incident instead of listing —
        /// accepts the filename, or any substring that matches exactly one
        #[arg(long)]
        show: Option<String>,
        /// Max incidents to list, most recent first (ignored with --show)
        #[arg(long, default_value_t = 20)]
        limit: usize,
    },
    /// Investigate an alert: runs the read-only diagnosis agent against
    /// the incident it fired, appending a `## Agent diagnosis` section
    /// (and, if the agent identifies one, a `## Proposed fix`)
    Investigate {
        /// Alert key to investigate, e.g. `cpu_hog:37489` (shown in the notification)
        alert_key: String,
        /// Path to the vigil_agent project directory
        #[arg(long, default_value = "agent")]
        agent_dir: String,
        /// Directory the incident journal is stored in
        #[arg(long, default_value_t = default_incidents_dir())]
        incidents_dir: String,
        /// Optional path to a persistent watch.jsonl history for trend context
        #[arg(long)]
        watch_log: Option<String>,
    },
    /// Execute a fix plan an earlier `vigil investigate` proposed, after
    /// interactive per-step approval
    Fix {
        /// Path to an incident file containing a `## Proposed fix` block
        incident_file: String,
        /// Path to the vigil_agent project directory
        #[arg(long, default_value = "agent")]
        agent_dir: String,
    },
    /// Menu bar health indicator — transparent when healthy, yellow/red otherwise
    Menubar {
        /// Status file written by `vigil watch` (see --status-file there)
        #[arg(long, default_value_t = default_status_file())]
        status_file: String,
        /// Directory the incident journal is stored in (for the dropdown)
        #[arg(long, default_value_t = default_incidents_dir())]
        incidents_dir: String,
        /// Seconds between polling the status file
        #[arg(long, default_value_t = 3)]
        poll_secs: u64,
    },
}

pub fn default_incidents_dir() -> String {
    crate::incidents::default_dir().to_string_lossy().to_string()
}

pub fn default_status_file() -> String {
    crate::menubar::default_status_file().to_string_lossy().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_incidents_dir_matches_the_incidents_module_default() {
        assert_eq!(default_incidents_dir(), crate::incidents::default_dir().to_string_lossy().to_string());
        assert!(default_incidents_dir().ends_with(".vigil/incidents"));
    }

    #[test]
    fn default_status_file_matches_the_menubar_module_default() {
        assert_eq!(default_status_file(), crate::menubar::default_status_file().to_string_lossy().to_string());
        assert!(default_status_file().ends_with(".vigil/status.json"));
    }

    #[test]
    fn investigate_parses_the_alert_key_positional_argument() {
        let cli = Cli::try_parse_from(["vigil", "investigate", "cpu_hog:37489"]).unwrap();
        assert!(matches!(&cli.command, Commands::Investigate { alert_key, .. } if alert_key == "cpu_hog:37489"));
    }

    #[test]
    fn fix_parses_the_incident_file_positional_argument() {
        let cli = Cli::try_parse_from(["vigil", "fix", "/tmp/some-incident.md"]).unwrap();
        assert!(matches!(&cli.command, Commands::Fix { incident_file, .. } if incident_file == "/tmp/some-incident.md"));
    }
}
