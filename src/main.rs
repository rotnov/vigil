mod agent;
mod agent_process;
mod alerts;
mod battery;
mod cli;
mod fix_process;
mod fixplan;
mod incidents;
mod incidents_cmd;
mod investigate;
mod investigate_process;
mod menubar;
mod menubar_loop;
mod notify;
mod snapshot;
mod ui;
mod ui_loop;
mod watch;

use clap::Parser;
use cli::{Cli, Commands};
use std::time::Duration;
use sysinfo::System;

// Re-exported at the crate root so existing `crate::Snapshot`,
// `crate::take_snapshot`, etc. references elsewhere (alerts.rs, ui.rs)
// don't need to know the collection logic lives in `snapshot.rs`.
pub use snapshot::{BatteryInfo, ConnectionCounts, DiskInfo, LoadAvg, MemoryInfo, ProcGroup, ProcInfo, ProcSample, Snapshot, take_snapshot};

/// Excluded from the coverage gate (see AGENTS.md's testing section): this
/// is just `Cli::parse()` plus a delegation to whichever module actually
/// owns that subcommand's logic, each already covered where it lives
/// (`snapshot.rs`, `watch.rs`, `ui.rs`, `incidents_cmd.rs`, `menubar.rs`).
fn main() {
    let cli = Cli::parse();

    match cli.command {
        Commands::Snapshot { top } => {
            let mut sys = System::new_all();
            let snap = take_snapshot(&mut sys, top);
            println!("{}", serde_json::to_string(&snap).unwrap());
        }
        Commands::Watch { interval, count, out, top, no_notify, cooldown_secs, incidents_dir, status_file } => {
            watch::run(watch::WatchArgs { interval, count, out, top, no_notify, cooldown_secs, incidents_dir, status_file });
        }
        Commands::Ui { interval, top, no_notify, cooldown_secs, agent_dir, incidents_dir } => {
            let opts = ui::UiOptions {
                interval: Duration::from_secs(interval),
                top_n: top,
                notify: !no_notify,
                cooldown: Duration::from_secs(cooldown_secs),
                agent_dir,
                incidents_dir,
            };
            ui_loop::run(opts).expect("ui failed");
        }
        Commands::Incidents { dir, show, limit } => {
            std::process::exit(incidents_cmd::run(&dir, show.as_deref(), limit));
        }
        Commands::Investigate { alert_key, agent_dir, incidents_dir, watch_log } => {
            std::process::exit(investigate_process::run(&alert_key, &agent_dir, &incidents_dir, watch_log.as_deref()));
        }
        Commands::Fix { incident_file, agent_dir } => {
            std::process::exit(fix_process::run(&incident_file, &agent_dir));
        }
        Commands::Menubar { status_file, incidents_dir, poll_secs } => {
            menubar_loop::run(menubar::MenubarOptions { status_file, incidents_dir, poll_interval: Duration::from_secs(poll_secs) });
        }
    }
}
