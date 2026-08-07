mod agent;
mod alerts;
mod battery;
mod incidents;
mod ui;

use clap::{Parser, Subcommand};
use serde::Serialize;
use std::io::Write;
use std::process::Command;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};
use sysinfo::{Disks, Pid, System};

#[derive(Parser)]
#[command(name = "vigil", version, about = "Lightweight system metrics collector — hands snapshots to an LLM agent for diagnosis")]
struct Cli {
    #[command(subcommand)]
    command: Commands,
}

#[derive(Subcommand)]
enum Commands {
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
        /// Path to the vigil_agent project directory (for CPU-alert auto-diagnosis)
        #[arg(long, default_value = "agent")]
        agent_dir: String,
        /// Directory for the auto-diagnosis incident journal (markdown, one file per diagnosis)
        #[arg(long, default_value_t = default_incidents_dir())]
        incidents_dir: String,
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
}

fn default_incidents_dir() -> String {
    incidents::default_dir().to_string_lossy().to_string()
}

#[derive(Serialize)]
struct ProcInfo {
    pid: u32,
    name: String,
    cpu_pct: f32,
    mem_bytes: u64,
    run_time_secs: u64,
    cmd: String,
}

#[derive(Serialize)]
struct Snapshot {
    ts_unix: u64,
    load_avg: LoadAvg,
    memory: MemoryInfo,
    disks: Vec<DiskInfo>,
    battery: Option<BatteryInfo>,
    top_cpu: Vec<ProcInfo>,
    top_mem: Vec<ProcInfo>,
}

#[derive(Serialize, Clone)]
struct DiskInfo {
    mount_point: String,
    total_bytes: u64,
    available_bytes: u64,
    used_pct: f32,
}

#[derive(Serialize)]
struct LoadAvg {
    one: f64,
    five: f64,
    fifteen: f64,
}

#[derive(Serialize)]
struct MemoryInfo {
    total_bytes: u64,
    used_bytes: u64,
    free_bytes: u64,
    swap_total_bytes: u64,
    swap_used_bytes: u64,
}

#[derive(Serialize)]
struct BatteryInfo {
    percentage: Option<u8>,
    charging: Option<bool>,
    /// macOS's own "H:MM remaining" estimate, in seconds. `None` when
    /// pmset shows "0:00"/"(no estimate)" (i.e. not discharging, or still
    /// calibrating right after a state change).
    remaining_secs: Option<u64>,
    raw: String,
}

fn take_snapshot(sys: &mut System, top_n: usize) -> Snapshot {
    // Two refreshes with a short delay give sysinfo a real CPU delta to measure.
    sys.refresh_cpu_usage();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    std::thread::sleep(Duration::from_millis(200));
    sys.refresh_cpu_usage();
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
    sys.refresh_memory();

    let load = System::load_average();

    let mut procs: Vec<(&Pid, &sysinfo::Process)> = sys.processes().iter().collect();

    procs.sort_by(|a, b| b.1.cpu_usage().partial_cmp(&a.1.cpu_usage()).unwrap());
    let top_cpu = procs
        .iter()
        .take(top_n)
        .map(|(pid, p)| to_proc_info(pid, p))
        .collect();

    procs.sort_by(|a, b| b.1.memory().cmp(&a.1.memory()));
    let top_mem = procs
        .iter()
        .take(top_n)
        .map(|(pid, p)| to_proc_info(pid, p))
        .collect();

    Snapshot {
        ts_unix: SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs(),
        load_avg: LoadAvg {
            one: load.one,
            five: load.five,
            fifteen: load.fifteen,
        },
        memory: MemoryInfo {
            total_bytes: sys.total_memory(),
            used_bytes: sys.used_memory(),
            free_bytes: sys.free_memory(),
            swap_total_bytes: sys.total_swap(),
            swap_used_bytes: sys.used_swap(),
        },
        disks: collect_disks(),
        battery: read_battery(),
        top_cpu,
        top_mem,
    }
}

fn collect_disks() -> Vec<DiskInfo> {
    Disks::new_with_refreshed_list()
        .list()
        .iter()
        .filter(|d| d.total_space() > 0)
        .map(|d| {
            let total = d.total_space();
            let available = d.available_space();
            let used_pct = if total > 0 {
                (total.saturating_sub(available)) as f32 / total as f32 * 100.0
            } else {
                0.0
            };
            DiskInfo {
                mount_point: d.mount_point().to_string_lossy().to_string(),
                total_bytes: total,
                available_bytes: available,
                used_pct,
            }
        })
        .collect()
}

fn to_proc_info(pid: &Pid, p: &sysinfo::Process) -> ProcInfo {
    ProcInfo {
        pid: pid.as_u32(),
        name: p.name().to_string_lossy().to_string(),
        cpu_pct: p.cpu_usage(),
        mem_bytes: p.memory(),
        run_time_secs: p.run_time(),
        cmd: p
            .cmd()
            .iter()
            .map(|s| s.to_string_lossy().to_string())
            .collect::<Vec<_>>()
            .join(" ")
            .chars()
            .take(200)
            .collect(),
    }
}

/// Shells out to `pmset -g batt` — sysinfo has no battery API on macOS.
fn read_battery() -> Option<BatteryInfo> {
    let output = Command::new("pmset").args(["-g", "batt"]).output().ok()?;
    let text = String::from_utf8_lossy(&output.stdout).to_string();
    let line = text.lines().nth(1)?;
    parse_battery_line(line)
}

fn parse_battery_line(line: &str) -> Option<BatteryInfo> {
    let percentage = line
        .split_whitespace()
        .find(|tok| tok.contains('%'))
        .and_then(|tok| tok.trim_matches(|c: char| !c.is_ascii_digit()).parse::<u8>().ok());

    let charging = if line.contains("discharging") {
        Some(false)
    } else if line.contains("charging") || line.contains("AC Power") {
        Some(true)
    } else {
        None
    };

    Some(BatteryInfo {
        percentage,
        charging,
        remaining_secs: parse_remaining_secs(line),
        raw: line.trim().to_string(),
    })
}

/// Parses the "H:MM remaining" segment pmset prints when actively
/// discharging. Returns `None` for "0:00" (pmset's way of saying N/A when
/// not discharging) and for "(no estimate)" right after a state change.
fn parse_remaining_secs(line: &str) -> Option<u64> {
    let before_remaining = line.split("remaining").next()?;
    let token = before_remaining.split_whitespace().last()?;
    let (h, m) = token.split_once(':')?;
    let h: u64 = h.parse().ok()?;
    let m: u64 = m.parse().ok()?;
    if h == 0 && m == 0 {
        return None;
    }
    Some(h * 3600 + m * 60)
}

fn main() {
    let cli = Cli::parse();
    let mut sys = System::new_all();

    match cli.command {
        Commands::Snapshot { top } => {
            let snap = take_snapshot(&mut sys, top);
            println!("{}", serde_json::to_string(&snap).unwrap());
        }
        Commands::Watch {
            interval,
            count,
            out,
            top,
            no_notify,
            cooldown_secs,
            agent_dir,
            incidents_dir,
        } => {
            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&out)
                .expect("failed to open output file");

            let cpu_count = sys.cpus().len();
            let mut alert_state = alerts::AlertState::new();
            let mut battery_trend = battery::BatteryTrend::new();
            let mut recent_alerts = alerts::RecentAlerts::new();
            let cooldown = Duration::from_secs(cooldown_secs);

            let mut n: u64 = 0;
            loop {
                let snap = take_snapshot(&mut sys, top);
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
                    battery_eta.map(|e| format!(" battery_eta={}", battery::format_eta(e))).unwrap_or_default()
                );

                if !no_notify {
                    let mut fired = alerts::evaluate(&snap, cpu_count, &mut alert_state, cooldown, now);
                    fired.extend(alerts::evaluate_battery(&snap, battery_eta, &mut alert_state, cooldown, now));
                    for alert in &fired {
                        recent_alerts.record(&alert.key, &alert.message, now);
                    }
                    for alert in fired {
                        eprintln!("[vigil] ALERT [{}] {}", alert.key, alert.message);
                        alerts::notify(&alert);
                        let context = recent_alerts.context_excluding(&alert.key, now);
                        agent::maybe_diagnose_alert_async(&alert, &line, &agent_dir, &incidents_dir, context.as_deref());
                    }
                }

                n += 1;
                if count != 0 && n >= count {
                    break;
                }
                std::thread::sleep(Duration::from_secs(interval));
            }
        }
        Commands::Ui {
            interval,
            top,
            no_notify,
            cooldown_secs,
            agent_dir,
            incidents_dir,
        } => {
            let opts = ui::UiOptions {
                interval: Duration::from_secs(interval),
                top_n: top,
                notify: !no_notify,
                cooldown: Duration::from_secs(cooldown_secs),
                agent_dir,
                incidents_dir,
            };
            ui::run(opts).expect("ui failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_discharging_battery_line() {
        let line = "-InternalBattery-0 (id=36044899)\t92%; discharging; 2:26 remaining present: true";
        let b = parse_battery_line(line).unwrap();
        assert_eq!(b.percentage, Some(92));
        assert_eq!(b.charging, Some(false));
        assert_eq!(b.remaining_secs, Some(2 * 3600 + 26 * 60));
    }

    #[test]
    fn parses_charging_battery_line() {
        let line = "-InternalBattery-0 (id=36044899)\t13%; charging; 2:21 remaining present: true";
        let b = parse_battery_line(line).unwrap();
        assert_eq!(b.percentage, Some(13));
        assert_eq!(b.charging, Some(true));
    }

    #[test]
    fn charged_and_plugged_in_has_no_remaining_estimate() {
        let line = "-InternalBattery-0 (id=36044899)\t100%; charged; 0:00 remaining present: true";
        let b = parse_battery_line(line).unwrap();
        assert_eq!(b.percentage, Some(100));
        assert_eq!(b.remaining_secs, None);
    }

    #[test]
    fn no_estimate_right_after_unplugging_is_handled() {
        let line = "-InternalBattery-0 (id=36044899)\t87%; discharging; (no estimate) present: true";
        let b = parse_battery_line(line).unwrap();
        assert_eq!(b.percentage, Some(87));
        assert_eq!(b.remaining_secs, None);
    }
}
