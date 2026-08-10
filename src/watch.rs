//! `vigil watch` — the continuous sampling loop: writes JSONL, evaluates
//! alert rules, fires notifications, and writes an incident stub for each
//! new incident.
//!
//! Excluded from the coverage gate (see AGENTS.md's testing section and the
//! `--ignore-filename-regex` in the test command): this is genuinely an
//! infinite loop with real `Command`/file/notification side effects on
//! every iteration, only exitable by `count` reaching a nonzero target or
//! the process being killed. Every piece of *logic* inside it — alert
//! evaluation, incident tracking, notification formatting — is already
//! unit-tested where it's actually defined (`alerts.rs`, `agent.rs`,
//! `battery.rs`); this loop is just the thin glue that calls them on a
//! timer.

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
            for alert in fired {
                eprintln!("[vigil] ALERT [{}] {}", alert.key, alert.message);
                if incident_tracker.is_new_incident(alert.target.as_deref(), incident_timeout, now) {
                    let incidents_dir = std::path::Path::new(&args.incidents_dir);
                    let stub = crate::incidents::IncidentStub {
                        alert_key: &alert.key,
                        alert_title: &alert.title,
                        alert_message: &alert.message,
                    };
                    if let Err(e) = crate::incidents::write_stub(incidents_dir, &stub) {
                        eprintln!("[vigil] failed to write incident stub: {e}");
                    }
                    crate::alerts::notify(&crate::agent::augment_with_investigate_hint(&alert, watch_log_path.as_deref()));
                } else {
                    eprintln!(
                        "[vigil] [{}] continuing open incident for {:?} — notification suppressed",
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
