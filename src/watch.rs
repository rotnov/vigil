//! `vigil watch` — the continuous sampling loop: writes JSONL, evaluates
//! alert rules, fires notifications, and (for the incident-worthy rules)
//! triggers a background agent diagnosis.
//!
//! Excluded from the coverage gate (see AGENTS.md's testing section and the
//! `--ignore-filename-regex` in the test command): this is genuinely an
//! infinite loop with real `Command`/file/notification side effects on
//! every iteration, only exitable by `count` reaching a nonzero target or
//! the process being killed. Every piece of *logic* inside it — alert
//! evaluation, incident tracking, the diagnosis question, notification
//! formatting — is already unit-tested where it's actually defined
//! (`alerts.rs`, `agent.rs`, `battery.rs`); this loop is just the thin glue
//! that calls them on a timer.

use std::io::Write;
use std::time::{Duration, Instant};
use sysinfo::System;

pub struct WatchArgs {
    pub interval: u64,
    pub count: u64,
    pub out: String,
    pub top: usize,
    pub no_notify: bool,
    pub cooldown_secs: u64,
    pub agent_dir: String,
    pub incidents_dir: String,
    pub status_file: String,
}

pub fn run(args: WatchArgs) {
    let mut sys = System::new_all();
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&args.out)
        .expect("failed to open output file");
    // Absolute so the agent (a separate `uv run` subprocess) can
    // reliably find it regardless of `--out` being relative.
    let watch_log_path = std::fs::canonicalize(&args.out).ok().map(|p| p.to_string_lossy().to_string());

    let cpu_count = sys.cpus().len();
    let mut alert_state = crate::alerts::AlertState::new();
    let mut battery_trend = crate::battery::BatteryTrend::new();
    let mut recent_alerts = crate::alerts::RecentAlerts::new();
    let mut incident_tracker = crate::alerts::IncidentTracker::new();
    let cooldown = Duration::from_secs(args.cooldown_secs);
    // Wide enough that a rule cycling in and out right at its own
    // `cooldown` boundary still reads as one ongoing incident, not
    // several — real field repeats arrived 5-13 minutes apart, gated
    // by cooldown itself, not by anything narrower.
    let incident_timeout = cooldown * 2;

    let mut n: u64 = 0;
    loop {
        let snap = crate::take_snapshot(&mut sys, args.top);
        let line = serde_json::to_string(&snap).unwrap();
        writeln!(file, "{line}").expect("failed to write snapshot");
        file.flush().ok();

        let now = Instant::now();
        battery_trend.record(
            snap.battery.as_ref().and_then(|b| b.charging),
            snap.battery.as_ref().and_then(|b| b.percentage),
            now,
        );
        let battery_eta = battery_trend.eta();

        eprintln!(
            "[vigil] sample {} @ {} — load1={:.2} mem_used={:.1}GB{}",
            n + 1,
            snap.ts_unix,
            snap.load_avg.one,
            snap.memory.used_bytes as f64 / 1e9,
            battery_eta.map(|e| format!(" battery_eta={}", crate::battery::format_eta(e))).unwrap_or_default()
        );

        if !args.no_notify {
            let mut fired = crate::alerts::evaluate(&snap, cpu_count, &mut alert_state, cooldown, now);
            fired.extend(crate::alerts::evaluate_battery(&snap, battery_eta, &mut alert_state, cooldown, now));
            for alert in &fired {
                recent_alerts.record(&alert.key, &alert.message, now);
            }
            for alert in fired {
                eprintln!("[vigil] ALERT [{}] {}", alert.key, alert.message);
                if incident_tracker.is_new_incident(alert.target.as_deref(), incident_timeout, now) {
                    crate::alerts::notify(&alert);
                    let context = recent_alerts.context_excluding(&alert.key, now);
                    crate::agent::maybe_diagnose_alert_async(
                        &alert,
                        &line,
                        &args.agent_dir,
                        &args.incidents_dir,
                        context.as_deref(),
                        watch_log_path.as_deref(),
                    );
                } else {
                    eprintln!(
                        "[vigil] [{}] continuing open incident for {:?} — notification/diagnosis suppressed",
                        alert.key, alert.target
                    );
                }
            }
        }

        crate::menubar::write_status(&args.status_file, incident_tracker.open_count(incident_timeout, now));

        n += 1;
        if args.count != 0 && n >= args.count {
            break;
        }
        std::thread::sleep(Duration::from_secs(args.interval));
    }
}
