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
    connections: Option<ConnectionCounts>,
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

#[derive(Serialize, Default, Debug, PartialEq)]
struct ConnectionCounts {
    established: u32,
    listen: u32,
    time_wait: u32,
    close_wait: u32,
    other: u32,
    total: u32,
    /// ESTABLISHED connections whose local port matches one of our own
    /// LISTEN ports *and* whose remote peer isn't loopback — i.e. someone
    /// out on the network actually connected in to a service this machine
    /// is running, as opposed to two local processes talking over
    /// 127.0.0.1 or this machine reaching out to something else. See
    /// docs/decisions/0001-network-connection-monitoring.md for why this
    /// heuristic (not a true kernel-level "who dialed whom") is what's used.
    incoming: u32,
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
        connections: collect_connections(),
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

/// Shells out to `netstat` (once for IPv4, once for IPv6 — `sysinfo` has no
/// per-connection API) and classifies each TCP entry by state. See
/// `docs/decisions/0001-network-connection-monitoring.md` for why `netstat`
/// over `lsof`, and for the "incoming" heuristic.
fn collect_connections() -> Option<ConnectionCounts> {
    let inet = Command::new("netstat").args(["-an", "-f", "inet"]).output().ok()?;
    let inet6 = Command::new("netstat").args(["-an", "-f", "inet6"]).output().ok()?;
    let mut combined = String::from_utf8_lossy(&inet.stdout).into_owned();
    combined.push('\n');
    combined.push_str(&String::from_utf8_lossy(&inet6.stdout));
    Some(parse_netstat_output(&combined))
}

/// Pure parser, kept separate from the `Command` calls above so it can be
/// unit-tested against captured `netstat` output without shelling out.
fn parse_netstat_output(output: &str) -> ConnectionCounts {
    struct Row<'a> {
        local: &'a str,
        foreign: &'a str,
        state: &'a str,
    }

    let rows: Vec<Row> = output
        .lines()
        .filter_map(|line| {
            let cols: Vec<&str> = line.split_whitespace().collect();
            if cols.len() < 6 || !cols[0].starts_with("tcp") {
                return None;
            }
            Some(Row {
                local: cols[3],
                foreign: cols[4],
                state: cols[5],
            })
        })
        .collect();

    let listen_ports: std::collections::HashSet<&str> =
        rows.iter().filter(|r| r.state == "LISTEN").filter_map(|r| netstat_port(r.local)).collect();

    let mut counts = ConnectionCounts::default();
    for row in &rows {
        counts.total += 1;
        match row.state {
            "ESTABLISHED" => counts.established += 1,
            "LISTEN" => counts.listen += 1,
            "TIME_WAIT" => counts.time_wait += 1,
            "CLOSE_WAIT" => counts.close_wait += 1,
            _ => counts.other += 1,
        }
        if row.state == "ESTABLISHED" && !is_loopback(row.foreign) {
            if let Some(port) = netstat_port(row.local) {
                if listen_ports.contains(port) {
                    counts.incoming += 1;
                }
            }
        }
    }
    counts
}

/// `netstat` prints addresses as `host.port` (e.g. `127.0.0.1.64342`,
/// `*.61118`, or the IPv6 form `2001:8a0:616a:50.57419`) — the port is
/// always the component after the last `.`.
fn netstat_port(addr: &str) -> Option<&str> {
    let port = addr.rsplit('.').next()?;
    if port.is_empty() || !port.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    Some(port)
}

fn is_loopback(addr: &str) -> bool {
    let host = addr.rsplit_once('.').map(|(h, _)| h).unwrap_or(addr);
    host == "127.0.0.1" || host == "::1" || host.starts_with("127.")
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
            let mut diagnosis_coalescer = agent::DiagnosisCoalescer::new();
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
                        agent::maybe_diagnose_alert_async(
                            &alert,
                            &line,
                            &agent_dir,
                            &incidents_dir,
                            context.as_deref(),
                            &mut diagnosis_coalescer,
                            now,
                        );
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
        Commands::Incidents { dir, show, limit } => {
            run_incidents_command(&dir, show.as_deref(), limit);
        }
    }
}

/// `vigil incidents` — lists or shows saved auto-diagnosis reports so the
/// journal can be checked from a plain shell, without an already-open TUI
/// session (a TUI can't pop itself open on a push notification).
fn run_incidents_command(dir: &str, show: Option<&str>, limit: usize) {
    let path = std::path::Path::new(dir);
    let files = incidents::list(path).unwrap_or_else(|e| {
        eprintln!("[vigil] {e}");
        std::process::exit(1);
    });

    if let Some(query) = show {
        let matches: Vec<_> = files
            .iter()
            .filter(|p| p.file_name().unwrap().to_string_lossy().contains(query))
            .collect();
        match matches.as_slice() {
            [] => {
                eprintln!("[vigil] no incident matches \"{query}\" in {}", path.display());
                std::process::exit(1);
            }
            [single] => match std::fs::read_to_string(single) {
                Ok(content) => print!("{content}"),
                Err(e) => {
                    eprintln!("[vigil] failed to read {}: {e}", single.display());
                    std::process::exit(1);
                }
            },
            many => {
                eprintln!("[vigil] \"{query}\" matches {} incidents, be more specific:", many.len());
                for m in many {
                    eprintln!("  {}", m.file_name().unwrap().to_string_lossy());
                }
                std::process::exit(1);
            }
        }
        return;
    }

    if files.is_empty() {
        println!("No incidents recorded yet in {}", path.display());
        return;
    }

    for f in files.iter().rev().take(limit) {
        let title = std::fs::read_to_string(f)
            .map(|c| incidents::extract_title(&c).to_string())
            .unwrap_or_else(|_| "(unreadable)".to_string());
        println!("{}  {}", f.file_name().unwrap().to_string_lossy(), title);
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

    // Captured from a real `netstat -an -f inet` / `-f inet6` run on this
    // machine (2026-08-07), trimmed to a representative sample of each
    // state plus the header lines every real invocation includes.
    const NETSTAT_SAMPLE: &str = "\
Active Internet connections (including servers)
Proto Recv-Q Send-Q  Local Address                                 Foreign Address                               (state)
tcp4       0      0  127.0.0.1.64342        127.0.0.1.57332        ESTABLISHED
tcp4       0      0  127.0.0.1.57332        127.0.0.1.64342        ESTABLISHED
tcp46      0      0  *.61118                *.*                    LISTEN
tcp4       0      0  127.0.0.1.27403        *.*                    LISTEN
tcp4       0      0  93.184.216.34.443      10.0.0.5.54321         TIME_WAIT
tcp4       0      0  10.0.0.5.54322         93.184.216.34.443      CLOSE_WAIT
udp4       0      0  *.68                   *.*
tcp6       0      0  2001:8a0:616a:50.57419 2600:1901:0:9e23.443   ESTABLISHED";

    #[test]
    fn parse_real_netstat_capture_counts_every_state() {
        let c = parse_netstat_output(NETSTAT_SAMPLE);
        assert_eq!(c.established, 3);
        assert_eq!(c.listen, 2);
        assert_eq!(c.time_wait, 1);
        assert_eq!(c.close_wait, 1);
        assert_eq!(c.other, 0);
        assert_eq!(c.total, 7); // header/title/udp lines excluded
    }

    #[test]
    fn loopback_pair_is_not_counted_as_incoming() {
        let c = parse_netstat_output(NETSTAT_SAMPLE);
        // 127.0.0.1.64342 <-> 127.0.0.1.57332 is two local processes
        // talking to each other, not anyone connecting in from outside.
        assert_eq!(c.incoming, 0);
    }

    #[test]
    fn outgoing_established_connection_is_not_counted_as_incoming() {
        // Neither TIME_WAIT/CLOSE_WAIT participate (only ESTABLISHED does),
        // and the sample's one non-loopback ESTABLISHED row (the IPv6 one)
        // has an ephemeral local port that isn't in the LISTEN set.
        let c = parse_netstat_output(NETSTAT_SAMPLE);
        assert_eq!(c.incoming, 0);
    }

    #[test]
    fn established_connection_on_a_listen_port_from_a_real_peer_is_incoming() {
        let sample = "\
Proto Recv-Q Send-Q  Local Address                                 Foreign Address                               (state)
tcp4       0      0  *.9090                 *.*                    LISTEN
tcp4       0      0  192.168.1.10.9090      198.51.100.7.51000     ESTABLISHED";
        let c = parse_netstat_output(sample);
        assert_eq!(c.incoming, 1);
    }

    #[test]
    fn established_connection_on_a_listen_port_from_loopback_is_not_incoming() {
        let sample = "\
Proto Recv-Q Send-Q  Local Address                                 Foreign Address                               (state)
tcp4       0      0  *.9090                 *.*                    LISTEN
tcp4       0      0  127.0.0.1.9090         127.0.0.1.51000        ESTABLISHED";
        let c = parse_netstat_output(sample);
        assert_eq!(c.incoming, 0);
    }

    #[test]
    fn netstat_port_extracts_the_trailing_port() {
        assert_eq!(netstat_port("127.0.0.1.64342"), Some("64342"));
        assert_eq!(netstat_port("*.61118"), Some("61118"));
        assert_eq!(netstat_port("2001:8a0:616a:50.57419"), Some("57419"));
        assert_eq!(netstat_port("*.*"), None);
    }

    #[test]
    fn is_loopback_recognizes_v4_and_v6() {
        assert!(is_loopback("127.0.0.1.57332"));
        assert!(is_loopback("::1.443"));
        assert!(!is_loopback("93.184.216.34.443"));
        assert!(!is_loopback("*.*"));
    }
}
