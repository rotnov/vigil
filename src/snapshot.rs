//! Collecting one point-in-time snapshot of system state: processes, memory,
//! disks, battery, network connections. No LLM, no network calls of its own
//! — see AGENTS.md's "Core architectural rule". The handful of `Command`
//! shell-outs here (`pmset`, `netstat`) are each kept to the smallest
//! possible function, with the actual parsing split into a pure, unit-tested
//! sibling (`parse_battery_line`, `parse_netstat_output`) — the shell-out
//! itself is covered by directly calling it in a test on this machine
//! (macOS, which is what vigil targets) rather than mocking `Command`.

use serde::Serialize;
use std::process::Command;
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use sysinfo::{Disks, Pid, System};

#[derive(Serialize)]
pub struct ProcInfo {
    pub pid: u32,
    pub name: String,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub run_time_secs: u64,
    pub cmd: String,
}

#[derive(Serialize)]
pub struct Snapshot {
    pub ts_unix: u64,
    pub load_avg: LoadAvg,
    pub memory: MemoryInfo,
    pub disks: Vec<DiskInfo>,
    pub battery: Option<BatteryInfo>,
    pub connections: Option<ConnectionCounts>,
    pub top_cpu: Vec<ProcInfo>,
    pub top_mem: Vec<ProcInfo>,
    /// Processes aggregated by name (e.g. every "Google Chrome Helper
    /// (Renderer)" instance combined), sorted by combined memory
    /// descending. Individually small helper/renderer processes of a
    /// multi-process app can each sit well below `top_mem`'s per-process
    /// ranking while their *sum* is the real memory story — this is how
    /// `swap_pressure`/`low_memory` notice that case. See
    /// `group_by_name`/`alerts::format_mem_consumer`.
    pub top_mem_groups: Vec<ProcGroup>,
}

#[derive(Serialize, Clone)]
pub struct ProcGroup {
    pub name: String,
    pub count: u32,
    pub total_cpu_pct: f32,
    pub total_mem_bytes: u64,
    /// The single highest-memory PID in this group — a pointer for further
    /// investigation (e.g. `sample <pid>`), not a claim that it alone
    /// explains the group's total.
    pub top_pid: u32,
}

#[derive(Serialize, Clone)]
pub struct DiskInfo {
    pub mount_point: String,
    pub total_bytes: u64,
    pub available_bytes: u64,
    pub used_pct: f32,
}

#[derive(Serialize)]
pub struct LoadAvg {
    pub one: f64,
    pub five: f64,
    pub fifteen: f64,
}

#[derive(Serialize)]
pub struct MemoryInfo {
    pub total_bytes: u64,
    pub used_bytes: u64,
    pub free_bytes: u64,
    pub swap_total_bytes: u64,
    pub swap_used_bytes: u64,
}

#[derive(Serialize)]
pub struct BatteryInfo {
    pub percentage: Option<u8>,
    pub charging: Option<bool>,
    /// macOS's own "H:MM remaining" estimate, in seconds. `None` when
    /// pmset shows "0:00"/"(no estimate)" (i.e. not discharging, or still
    /// calibrating right after a state change).
    pub remaining_secs: Option<u64>,
    pub raw: String,
}

#[derive(Serialize, Default, Debug, PartialEq)]
pub struct ConnectionCounts {
    pub established: u32,
    pub listen: u32,
    pub time_wait: u32,
    pub close_wait: u32,
    pub other: u32,
    pub total: u32,
    /// ESTABLISHED connections whose local port matches one of our own
    /// LISTEN ports *and* whose remote peer isn't loopback — i.e. someone
    /// out on the network actually connected in to a service this machine
    /// is running, as opposed to two local processes talking over
    /// 127.0.0.1 or this machine reaching out to something else. See
    /// docs/decisions/0001-network-connection-monitoring.md for why this
    /// heuristic (not a true kernel-level "who dialed whom") is what's used.
    pub incoming: u32,
}

pub fn take_snapshot(sys: &mut System, top_n: usize) -> Snapshot {
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

    // Over ALL processes, not just top_n — a process that individually
    // never ranks in top_mem can still be part of a group whose combined
    // memory does.
    let all_procs: Vec<ProcInfo> = procs.iter().map(|(pid, p)| to_proc_info(pid, p)).collect();
    let mut top_mem_groups = group_by_name(&all_procs);
    top_mem_groups.truncate(top_n);

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
        top_mem_groups,
    }
}

/// Aggregates by process name — every "Google Chrome Helper (Renderer)"
/// instance combined, etc. — sorted by combined memory descending. A pure
/// function over `ProcInfo`s (not `sysinfo::Process`) so it's testable
/// without a real `System`.
fn group_by_name(procs: &[ProcInfo]) -> Vec<ProcGroup> {
    struct Acc {
        count: u32,
        total_cpu_pct: f32,
        total_mem_bytes: u64,
        top_pid: u32,
        top_pid_mem: u64,
    }

    let mut groups: std::collections::HashMap<&str, Acc> = std::collections::HashMap::new();
    for p in procs {
        let acc = groups.entry(p.name.as_str()).or_insert(Acc {
            count: 0,
            total_cpu_pct: 0.0,
            total_mem_bytes: 0,
            top_pid: p.pid,
            top_pid_mem: 0,
        });
        acc.count += 1;
        acc.total_cpu_pct += p.cpu_pct;
        acc.total_mem_bytes += p.mem_bytes;
        if p.mem_bytes > acc.top_pid_mem {
            acc.top_pid_mem = p.mem_bytes;
            acc.top_pid = p.pid;
        }
    }

    let mut result: Vec<ProcGroup> = groups
        .into_iter()
        .map(|(name, acc)| ProcGroup {
            name: name.to_string(),
            count: acc.count,
            total_cpu_pct: acc.total_cpu_pct,
            total_mem_bytes: acc.total_mem_bytes,
            top_pid: acc.top_pid,
        })
        .collect();
    result.sort_by(|a, b| b.total_mem_bytes.cmp(&a.total_mem_bytes));
    result
}

fn collect_disks() -> Vec<DiskInfo> {
    Disks::new_with_refreshed_list()
        .list()
        .iter()
        .filter(|d| d.total_space() > 0)
        .map(|d| {
            let total = d.total_space();
            let available = d.available_space();
            // `total > 0` is guaranteed by the `filter` above, so this is a
            // plain division, not a defensive branch.
            let used_pct = (total.saturating_sub(available)) as f32 / total as f32 * 100.0;
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

#[cfg(test)]
mod tests {
    use super::*;

    fn proc(pid: u32, name: &str, cpu_pct: f32, mem_bytes: u64) -> ProcInfo {
        ProcInfo { pid, name: name.to_string(), cpu_pct, mem_bytes, run_time_secs: 0, cmd: name.to_string() }
    }

    #[test]
    fn group_by_name_aggregates_multiple_instances() {
        let procs = vec![
            proc(1, "Google Chrome Helper (Renderer)", 5.0, 200_000_000),
            proc(2, "Google Chrome Helper (Renderer)", 3.0, 250_000_000),
            proc(3, "Google Chrome Helper (Renderer)", 4.0, 180_000_000),
            proc(4, "pycharm", 90.0, 19_000_000_000),
        ];
        let groups = group_by_name(&procs);

        let chrome = groups.iter().find(|g| g.name == "Google Chrome Helper (Renderer)").unwrap();
        assert_eq!(chrome.count, 3);
        assert_eq!(chrome.total_mem_bytes, 200_000_000 + 250_000_000 + 180_000_000);
        assert!((chrome.total_cpu_pct - 12.0).abs() < 0.01);
        assert_eq!(chrome.top_pid, 2, "the highest-memory instance in the group");

        let pycharm = groups.iter().find(|g| g.name == "pycharm").unwrap();
        assert_eq!(pycharm.count, 1);
        assert_eq!(pycharm.total_mem_bytes, 19_000_000_000);
    }

    #[test]
    fn group_by_name_sorts_by_combined_memory_descending() {
        let procs = vec![
            proc(1, "solo-big", 1.0, 4_000_000_000),
            proc(2, "helper", 1.0, 1_500_000_000),
            proc(3, "helper", 1.0, 1_500_000_000),
            proc(4, "helper", 1.0, 1_500_000_000),
        ];
        let groups = group_by_name(&procs);
        // Combined, "helper" (3 x 1.5GB = 4.5GB) edges out "solo-big"
        // (4GB) even though no single "helper" instance comes close --
        // this is the whole point of grouping.
        assert_eq!(groups[0].name, "helper");
        assert_eq!(groups[0].total_mem_bytes, 4_500_000_000);
        assert_eq!(groups[1].name, "solo-big");
    }

    #[test]
    fn group_by_name_handles_empty_input() {
        assert!(group_by_name(&[]).is_empty());
    }

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

    #[test]
    fn parse_battery_line_handles_a_totally_unrecognized_line() {
        // No '%' token, no charging/discharging/AC Power keyword — pmset
        // output vigil has never actually seen, but the parser shouldn't
        // panic on it, just report "don't know".
        let b = parse_battery_line("garbage line with nothing useful").unwrap();
        assert_eq!(b.percentage, None);
        assert_eq!(b.charging, None);
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
    fn netstat_port_rejects_a_hostless_or_non_numeric_trailer() {
        assert_eq!(netstat_port(""), None);
        assert_eq!(netstat_port("just-a-word"), None);
    }

    #[test]
    fn is_loopback_recognizes_v4_and_v6() {
        assert!(is_loopback("127.0.0.1.57332"));
        assert!(is_loopback("::1.443"));
        assert!(!is_loopback("93.184.216.34.443"));
        assert!(!is_loopback("*.*"));
    }

    // The following call the real OS-shelling functions directly (`pmset`,
    // `netstat`, `sysinfo`'s disk/process APIs) rather than mocking them —
    // vigil only targets macOS, and this suite already only runs on the
    // maintainer's own Mac (see AGENTS.md's testing section), so exercising
    // the real shell-outs here is more honest than a mock that could drift
    // from the actual command output shape.

    #[test]
    fn take_snapshot_produces_a_well_formed_snapshot_on_this_machine() {
        let mut sys = System::new_all();
        let snap = take_snapshot(&mut sys, 5);
        assert!(snap.ts_unix > 0);
        assert!(snap.top_cpu.len() <= 5);
        assert!(snap.top_mem.len() <= 5);
        assert!(snap.top_mem_groups.len() <= 5);
        assert!(snap.memory.total_bytes > 0);
    }

    #[test]
    fn collect_disks_finds_at_least_the_root_volume() {
        let disks = collect_disks();
        assert!(!disks.is_empty(), "expected at least one disk with nonzero total space");
        assert!(disks.iter().any(|d| d.mount_point == "/"));
        for d in &disks {
            assert!((0.0..=100.0).contains(&d.used_pct), "used_pct out of range: {}", d.used_pct);
        }
    }

    #[test]
    fn read_battery_returns_a_plausible_percentage_when_present() {
        // This suite only runs on the maintainer's own Mac (see AGENTS.md),
        // which has a battery -- `pmset` is expected to succeed and parse.
        // A silent `if let` here would let a broken `pmset` shell-out pass
        // vacuously instead of failing loudly.
        let b = read_battery().expect("pmset -g batt should succeed and parse on this machine");
        let pct = b.percentage.expect("this machine's pmset output includes a percentage");
        assert!(pct <= 100);
    }

    #[test]
    fn collect_connections_returns_self_consistent_totals() {
        let c = collect_connections().expect("netstat should be available on macOS");
        assert_eq!(c.total, c.established + c.listen + c.time_wait + c.close_wait + c.other);
        assert!(c.incoming <= c.established);
    }
}
