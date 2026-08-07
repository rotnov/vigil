//! Cheap, rule-based anomaly detection over a single `Snapshot`.
//!
//! This is intentionally dumb (fixed thresholds, no LLM). It only decides
//! *whether to notify the user* and drafts a human-readable suggestion —
//! it never takes any corrective action itself. Any actual fix requires a
//! separate, explicit user confirmation outside this tool.

use crate::{ProcGroup, ProcInfo, Snapshot};
use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

const SWAP_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB
const LOW_FREE_MEM_RATIO: f64 = 0.05;
const CPU_HOG_THRESHOLD_PCT: f32 = 90.0;
const CPU_HOG_STREAK_REQUIRED: u32 = 3;
const LOW_DISK_AVAILABLE_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GB
const LOW_BATTERY_PCT: u8 = 20;
const RECENT_ALERTS_WINDOW: Duration = Duration::from_secs(5 * 60);
const RECENT_ALERTS_CAPACITY: usize = 10;
/// First-guess thresholds (see docs/decisions/0001-network-connection-monitoring.md)
/// — not derived from a fleet baseline, just picked generously above what a
/// single busy dev workstation showed in practice (~570 total connections
/// observed live) and near-zero for genuine external inbound. Expect to
/// tune both from real field data the way the other rules' thresholds were.
const HIGH_CONNECTION_COUNT_THRESHOLD: u32 = 1500;
const INCOMING_CONNECTIONS_THRESHOLD: u32 = 10;
/// First-guess thresholds for `high_process_count`. Live incident
/// (2026-08-07): 224 "node" processes (MCP servers spawned via `npx` by
/// closed Claude Code/Codex/Devin sessions, never cleaned up, some 3-14
/// days old per `ps -eo etime`) sat at 0.0% combined CPU — swap_pressure/
/// low_memory never cited the group since its ~9.6GB never crossed
/// GROUP_VS_SINGLE_RATIO against the single top process, and no existing
/// rule looks at instance count at all. 100 is comfortably above every
/// legitimately-multi-process app observed on this machine (36 Chrome
/// renderer helpers, 80 WebKit processes) while catching the 224-instance
/// case; 5% is "basically idle" for the group's *combined* CPU, wide
/// enough that a real many-tab browser actually doing something (video,
/// scroll, decode) won't false-positive. Expect to tune both from field
/// data, same as the connection-count thresholds above.
const PROCESS_COUNT_THRESHOLD: u32 = 100;
const LEAKED_GROUP_CPU_IDLE_PCT: f32 = 5.0;
/// How much bigger a process group's combined memory needs to be than the
/// single top consumer's before a message cites the group instead — avoids
/// e.g. citing "2 × app (210 MB combined)" when the single top process
/// already dominates and the group framing would just be noise.
const GROUP_VS_SINGLE_RATIO: f64 = 1.5;

/// vigil's own compiled binary name (see `Cargo.toml`'s `[package] name`) —
/// used to flag CPU rules where vigil itself, not something it's watching,
/// is the cited top consumer. This matches AGENTS.md's governing design
/// goal directly: vigil's own overhead counts against the same goal it's
/// trying to protect, so it shouldn't be silently indistinguishable from
/// any other process in its own alerts. Observed live (a `high_load` alert
/// once cited "vigil (61% CPU)") — this doesn't suppress or exclude that
/// case, it just makes it legible instead of reading like an ordinary
/// third-party app to restart.
const SELF_PROCESS_NAME: &str = "vigil";

/// Pure — appended to a CPU-rule message when its top consumer is vigil's
/// own process, empty otherwise.
fn self_process_note(name: &str) -> &'static str {
    if name == SELF_PROCESS_NAME {
        " (this is vigil's own process, not a third-party app — if it keeps happening, \
         it's worth profiling vigil itself rather than restarting something else; see \
         AGENTS.md's governing design goal.)"
    } else {
        ""
    }
}

/// A short rolling log of every alert that fired recently (any key,
/// including ones that never trigger an agent diagnosis on their own —
/// e.g. `low_memory`/`swap_pressure`). Fed to the agent as extra context
/// alongside whichever alert *did* trigger it, so a `cpu_hog` diagnosis
/// doesn't have to independently rediscover a memory-pressure problem
/// that another rule already flagged in the same window.
pub struct RecentAlerts {
    entries: VecDeque<(Instant, String, String)>,
}

impl RecentAlerts {
    pub fn new() -> Self {
        Self { entries: VecDeque::with_capacity(RECENT_ALERTS_CAPACITY) }
    }

    pub fn record(&mut self, key: &str, message: &str, now: Instant) {
        if self.entries.len() == RECENT_ALERTS_CAPACITY {
            self.entries.pop_front();
        }
        self.entries.push_back((now, key.to_string(), message.to_string()));
    }

    /// Messages of other alerts (any key) fired within the recency window,
    /// excluding `exclude_key` itself. `None` if there's nothing else.
    pub fn context_excluding(&self, exclude_key: &str, now: Instant) -> Option<String> {
        let others: Vec<&str> = self
            .entries
            .iter()
            .filter(|(t, k, _)| k != exclude_key && now.duration_since(*t) <= RECENT_ALERTS_WINDOW)
            .map(|(_, _, m)| m.as_str())
            .collect();
        if others.is_empty() {
            None
        } else {
            Some(others.join(" | "))
        }
    }
}

pub struct Alert {
    pub key: String,
    pub title: String,
    pub message: String,
    /// The process this alert is about, if any (e.g. `pycharm`) — set from
    /// the same `ProcInfo` the message text was built from, rather than
    /// parsed back out of the rendered message. Lets callers (see
    /// `IncidentTracker`) recognize "different alert, same underlying
    /// process" without brittle text scraping.
    pub target: Option<String>,
    /// The full command line of `target`, captured from the same `ProcInfo`
    /// at the moment this alert fired — not re-looked-up later. A
    /// background diagnosis (see `agent_process::maybe_diagnose_alert_async`)
    /// runs seconds to minutes after firing, and by then the OS may have
    /// recycled `target`'s pid to an unrelated process (observed live: an
    /// alert named "claude" whose pid had already become a `bfs` scan by
    /// the time the agent checked `ps -p <pid>`, see incident
    /// 2026-08-07-14-20-56-cpu-hog-27339.md). Capturing the command here,
    /// synchronously, at fire time avoids that race.
    pub command: Option<String>,
}

/// Tracks whether an alert firing for a given target process is the start
/// of a new incident or a continuation of one still open, so a process
/// pegged at high CPU for an hour produces one notification, one
/// background diagnosis, and one journal entry — not a fresh one on every
/// re-fire of the underlying rule (which, per-rule, only needs its own
/// `cooldown` to elapse to fire again). An incident is "still open" as
/// long as its target keeps firing within `timeout` of its last firing;
/// once a target goes quiet for `timeout`, the next firing starts a new
/// incident. Targetless alerts (`target: None` — e.g. `low_disk:<mount>`,
/// `high_connection_count`) have nothing to key on and are always "new".
///
/// Replaces an earlier, narrower `agent::DiagnosisCoalescer` that only
/// deduped near-simultaneous (120s) diagnoses. Real repeats of the same
/// incident in the field arrived 5-13 minutes apart — gated by each rule's
/// own re-fire cooldown, not by anything a 120s window could ever catch —
/// so that mechanism never actually engaged. This subsumes it and also
/// covers the native notification and the journal write, not just the
/// diagnosis, since all three were spamming for the same reason.
pub struct IncidentTracker {
    last_seen: HashMap<String, Instant>,
}

impl IncidentTracker {
    pub fn new() -> Self {
        Self { last_seen: HashMap::new() }
    }

    /// Returns true if this firing should be treated as a new incident —
    /// notify, diagnose, journal — and, either way, records `now` so the
    /// target's open window keeps extending while it keeps firing.
    pub fn is_new_incident(&mut self, target: Option<&str>, timeout: Duration, now: Instant) -> bool {
        let Some(target) = target else { return true };
        let is_new = match self.last_seen.get(target) {
            Some(t) => now.duration_since(*t) >= timeout,
            None => true,
        };
        self.last_seen.insert(target.to_string(), now);
        is_new
    }

    /// Number of targets currently within their open incident window as of
    /// `now` — read-only, doesn't record anything. Used by `vigil menubar`
    /// (via `vigil watch`'s status-file write) to derive a health color
    /// without re-running its own sampling/evaluation loop; see
    /// docs/decisions/0002-menu-bar-health-indicator.md.
    pub fn open_count(&self, timeout: Duration, now: Instant) -> usize {
        self.last_seen.values().filter(|t| now.duration_since(**t) < timeout).count()
    }
}

pub struct AlertState {
    last_fired: HashMap<String, Instant>,
    cpu_hog_streak: HashMap<u32, u32>,
}

impl AlertState {
    pub fn new() -> Self {
        Self {
            last_fired: HashMap::new(),
            cpu_hog_streak: HashMap::new(),
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn try_fire(
        &mut self,
        now: Instant,
        cooldown: Duration,
        key: &str,
        title: &str,
        message: String,
        target: Option<&str>,
        command: Option<&str>,
        out: &mut Vec<Alert>,
    ) {
        let ready = match self.last_fired.get(key) {
            Some(t) => now.duration_since(*t) >= cooldown,
            None => true,
        };
        if ready {
            self.last_fired.insert(key.to_string(), now);
            out.push(Alert {
                key: key.to_string(),
                title: title.to_string(),
                message,
                target: target.map(str::to_string),
                command: command.map(str::to_string),
            });
        }
    }
}

/// Describes the memory consumer worth naming in a message: the single top
/// process, unless a process *group* (see `crate::group_by_name`) is
/// materially larger — e.g. a dozen renderer helpers that individually
/// never rank at the top but collectively dwarf it. See
/// `GROUP_VS_SINGLE_RATIO` and the `AGENTS.md`/task history around process
/// grouping for why this exists.
fn format_mem_consumer(single: &ProcInfo, groups: &[ProcGroup]) -> String {
    if let Some(top_group) = groups.first() {
        if top_group.count > 1 && top_group.total_mem_bytes as f64 >= single.mem_bytes as f64 * GROUP_VS_SINGLE_RATIO {
            return format!(
                "{}× {} processes ({:.0} MB combined)",
                top_group.count,
                top_group.name,
                top_group.total_mem_bytes as f64 / 1e6
            );
        }
    }
    format!("{} ({:.0} MB)", single.name, single.mem_bytes as f64 / 1e6)
}

/// Pure — a human-readable single-unit age, coarsest unit that isn't zero
/// ("14d", "3h", "42m", "9s"). Good enough to convey "this has been running
/// for a suspiciously long time" without pulling in a duration-formatting
/// dependency for one alert message.
fn format_age(secs: u64) -> String {
    const MINUTE: u64 = 60;
    const HOUR: u64 = 60 * MINUTE;
    const DAY: u64 = 24 * HOUR;
    if secs >= DAY {
        format!("{}d", secs / DAY)
    } else if secs >= HOUR {
        format!("{}h", secs / HOUR)
    } else if secs >= MINUTE {
        format!("{}m", secs / MINUTE)
    } else {
        format!("{secs}s")
    }
}

/// Pure — renders a `ProcGroup`'s oldest-member sample as concrete,
/// loggable `pid (ppid X, age)` detail for `high_process_count`'s message,
/// e.g. "pid 41181 (ppid 40723, 14d), pid 9328 (ppid 9016, 3d)". Empty if
/// the group carried no samples (shouldn't happen for a real snapshot, but
/// `ProcGroup::oldest` isn't a non-empty-by-construction guarantee).
fn format_oldest_samples(group: &ProcGroup) -> String {
    group
        .oldest
        .iter()
        .map(|s| {
            let ppid = s.ppid.map(|p| p.to_string()).unwrap_or_else(|| "none".to_string());
            format!("pid {} (ppid {ppid}, {})", s.pid, format_age(s.run_time_secs))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// Evaluate a snapshot against fixed heuristics and return any alerts that
/// are due (i.e. not suppressed by their per-rule cooldown).
pub fn evaluate(snap: &Snapshot, cpu_count: usize, state: &mut AlertState, cooldown: Duration, now: Instant) -> Vec<Alert> {
    let mut alerts = Vec::new();

    let load_threshold = (cpu_count.max(1) as f64) * 1.5;
    if snap.load_avg.one > load_threshold {
        if let Some(top) = snap.top_cpu.first() {
            state.try_fire(
                now,
                cooldown,
                "high_load",
                "vigil: high load",
                format!(
                    "Load average {:.1} (threshold {:.1} for {} cores). Top consumer: {} ({:.0}% CPU).{} Suggestion: check the process and restart it if needed.",
                    snap.load_avg.one, load_threshold, cpu_count, top.name, top.cpu_pct, self_process_note(&top.name)
                ),
                Some(&top.name),
                Some(&top.cmd),
                &mut alerts,
            );
        }
    }

    if snap.memory.swap_used_bytes > SWAP_THRESHOLD_BYTES {
        if let Some(top) = snap.top_mem.first() {
            state.try_fire(
                now,
                cooldown,
                "swap_pressure",
                "vigil: active swap",
                format!(
                    "Swap usage {:.1} GB — memory is running out. Top consumer: {}. Suggestion: close it or restart the machine.",
                    snap.memory.swap_used_bytes as f64 / 1e9,
                    format_mem_consumer(top, &snap.top_mem_groups)
                ),
                Some(&top.name),
                Some(&top.cmd),
                &mut alerts,
            );
        }
    }

    if snap.memory.total_bytes > 0 {
        let free_ratio = snap.memory.free_bytes as f64 / snap.memory.total_bytes as f64;
        if free_ratio < LOW_FREE_MEM_RATIO {
            if let Some(top) = snap.top_mem.first() {
                state.try_fire(
                    now,
                    cooldown,
                    "low_memory",
                    "vigil: low free memory",
                    format!(
                        "Only {:.1}% memory free. Top consumer: {}. Suggestion: close unused applications.",
                        free_ratio * 100.0,
                        format_mem_consumer(top, &snap.top_mem_groups)
                    ),
                    Some(&top.name),
                    Some(&top.cmd),
                    &mut alerts,
                );
            }
        }
    }

    for disk in &snap.disks {
        if disk.available_bytes < LOW_DISK_AVAILABLE_BYTES {
            state.try_fire(
                now,
                cooldown,
                &format!("low_disk:{}", disk.mount_point),
                "vigil: low disk space",
                format!(
                    "{} has {:.1} GB free out of {:.1} GB ({:.0}% used). Press 'a' in the UI to ask the agent why, or check manually: browser caches (~/Library/Caches), Docker images (docker system df), Time Machine local snapshots (tmutil listlocalsnapshots /), Xcode DerivedData.",
                    disk.mount_point,
                    disk.available_bytes as f64 / 1e9,
                    disk.total_bytes as f64 / 1e9,
                    disk.used_pct
                ),
                None,
                None,
                &mut alerts,
            );
        }
    }

    let seen_pids: std::collections::HashSet<u32> = snap.top_cpu.iter().map(|p| p.pid).collect();
    state.cpu_hog_streak.retain(|pid, _| seen_pids.contains(pid));
    for p in &snap.top_cpu {
        if p.cpu_pct > CPU_HOG_THRESHOLD_PCT {
            *state.cpu_hog_streak.entry(p.pid).or_insert(0) += 1;
        } else {
            state.cpu_hog_streak.remove(&p.pid);
        }
    }
    if let Some((&pid, &streak)) = state
        .cpu_hog_streak
        .iter()
        .find(|(_, &s)| s >= CPU_HOG_STREAK_REQUIRED)
    {
        // `retain` above already dropped every streak entry whose pid isn't
        // in `snap.top_cpu` for this snapshot, so a surviving entry here is
        // always findable -- not a defensive `if let`, a real guarantee.
        let p = snap.top_cpu.iter().find(|p| p.pid == pid).expect("streak retain() guarantees this pid is in top_cpu");
        state.try_fire(
            now,
            cooldown,
            &format!("cpu_hog:{pid}"),
            "vigil: process hogging CPU",
            format!(
                "{} (pid {}) has held {:.0}% CPU for {} consecutive samples.{} Suggestion: check the process and decide whether to restart it.",
                p.name, p.pid, p.cpu_pct, streak, self_process_note(&p.name)
            ),
            Some(&p.name),
            Some(&p.cmd),
            &mut alerts,
        );
    }

    if let Some(conn) = &snap.connections {
        if conn.total > HIGH_CONNECTION_COUNT_THRESHOLD {
            state.try_fire(
                now,
                cooldown,
                "high_connection_count",
                "vigil: high connection count",
                format!(
                    "{} open TCP connections ({} established, {} time_wait, {} close_wait) — well above the usual baseline. Suggestion: `lsof -i -n -P | awk '{{print $1}}' | sort | uniq -c | sort -rn` to see which process holds the most.",
                    conn.total, conn.established, conn.time_wait, conn.close_wait
                ),
                None,
                None,
                &mut alerts,
            );
        }

        if conn.incoming > INCOMING_CONNECTIONS_THRESHOLD {
            state.try_fire(
                now,
                cooldown,
                "incoming_connections",
                "vigil: unusual incoming connections",
                format!(
                    "{} external connections into a service this machine is running right now. Suggestion: `lsof -i -n -P | grep LISTEN` to see what's exposed, and confirm that's expected.",
                    conn.incoming
                ),
                None,
                None,
                &mut alerts,
            );
        }
    }

    for group in &snap.top_mem_groups {
        if group.count > PROCESS_COUNT_THRESHOLD && group.total_cpu_pct < LEAKED_GROUP_CPU_IDLE_PCT {
            state.try_fire(
                now,
                cooldown,
                &format!("high_process_count:{}", group.name),
                "vigil: unusually many idle processes",
                format!(
                    "{} \"{}\" processes running, combined {:.1}% CPU (near-idle) and {:.0} MB — \
                     looks like leaked background helpers rather than active work. Oldest: {}. \
                     Suggestion: `ps -p <pid> -o ppid,etime,command` on one to confirm what spawned \
                     it and whether it's safe to kill.",
                    group.count,
                    group.name,
                    group.total_cpu_pct,
                    group.total_mem_bytes as f64 / 1e6,
                    format_oldest_samples(group)
                ),
                Some(&group.name),
                None,
                &mut alerts,
            );
        }
    }

    alerts
}

/// Separate from `evaluate` because it needs a drain-rate ETA computed
/// from history across multiple snapshots (`battery::BatteryTrend`), which
/// `evaluate` — a pure function of a single `Snapshot` — has no access to.
pub fn evaluate_battery(
    snap: &Snapshot,
    eta: Option<Duration>,
    state: &mut AlertState,
    cooldown: Duration,
    now: Instant,
) -> Option<Alert> {
    let battery = snap.battery.as_ref()?;
    if battery.charging != Some(false) {
        return None;
    }
    let pct = battery.percentage?;
    if pct > LOW_BATTERY_PCT {
        return None;
    }

    let eta_str = eta
        .map(crate::battery::format_eta)
        .or(battery.remaining_secs.map(|s| crate::battery::format_eta(Duration::from_secs(s))))
        .unwrap_or_else(|| "unknown".to_string());

    let top_hint = snap.top_cpu.first().map_or_else(String::new, |p| {
        format!(
            " Heaviest CPU consumer right now: {} ({:.0}%) — closing CPU-heavy apps is the \
             biggest lever without per-process power data.",
            p.name, p.cpu_pct
        )
    });

    let target = snap.top_cpu.first().map(|p| p.name.as_str());
    let command = snap.top_cpu.first().map(|p| p.cmd.as_str());
    let mut alerts = Vec::new();
    state.try_fire(
        now,
        cooldown,
        "battery_low",
        "vigil: low battery",
        format!("Battery at {pct}%, discharging, ~{eta_str} remaining.{top_hint}"),
        target,
        command,
        &mut alerts,
    );
    alerts.into_iter().next()
}

// `notify` (the actual `osascript` shell-out) lives in `notify.rs`, a
// dedicated, coverage-gate-excluded file — see that file's doc comment for
// why. Re-exported here so `crate::alerts::notify` keeps working everywhere
// it's already called.
pub use crate::notify::notify;

/// Quotes a string for a single-line `osascript -e` argument. Also
/// collapses newlines to spaces — a literal `\n` would terminate the `-e`
/// script mid-statement (unterminated string literal), which is a real risk
/// here since agent diagnoses are multi-paragraph text.
pub(crate) fn osa_quote(s: &str) -> String {
    let single_line = s.replace(['\n', '\r'], " ");
    format!("\"{}\"", single_line.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ConnectionCounts, DiskInfo, LoadAvg, MemoryInfo, ProcGroup, ProcInfo, ProcSample, Snapshot};

    fn proc(pid: u32, name: &str, cpu_pct: f32, mem_mb: u64) -> ProcInfo {
        ProcInfo {
            pid,
            ppid: None,
            name: name.to_string(),
            cpu_pct,
            mem_bytes: mem_mb * 1_000_000,
            run_time_secs: 0,
            cmd: name.to_string(),
        }
    }

    fn healthy_snapshot() -> Snapshot {
        Snapshot {
            ts_unix: 0,
            load_avg: LoadAvg { one: 1.0, five: 1.0, fifteen: 1.0 },
            memory: MemoryInfo {
                total_bytes: 16_000_000_000,
                used_bytes: 8_000_000_000,
                free_bytes: 8_000_000_000,
                swap_total_bytes: 0,
                swap_used_bytes: 0,
            },
            disks: vec![DiskInfo {
                mount_point: "/".to_string(),
                total_bytes: 500_000_000_000,
                available_bytes: 300_000_000_000,
                used_pct: 40.0,
            }],
            battery: None,
            connections: None,
            top_cpu: vec![proc(1, "idle_app", 5.0, 100)],
            top_mem: vec![proc(1, "idle_app", 5.0, 100)],
            top_mem_groups: vec![],
        }
    }

    #[test]
    fn osa_quote_collapses_newlines_to_keep_the_e_script_single_line() {
        let multiline = "Diagnosis: swap is full.\n\nSuggestions:\n1. Restart pycharm.";
        let quoted = osa_quote(multiline);
        assert!(!quoted.contains('\n'), "a literal newline would break `osascript -e`: {quoted:?}");
        assert!(quoted.contains("Restart pycharm"));
    }

    #[test]
    fn osa_quote_still_escapes_quotes_and_backslashes() {
        let quoted = osa_quote(r#"she said "hi" \ ok"#);
        assert_eq!(quoted, r#""she said \"hi\" \\ ok""#);
    }

    #[test]
    fn zero_total_memory_does_not_panic_or_fire_low_memory() {
        // Guards the `total_bytes > 0` check that avoids a division by zero
        // computing `free_ratio` -- a real `Snapshot` should never have a
        // zero total, but a malformed/future one shouldn't crash `evaluate`.
        let mut snap = healthy_snapshot();
        snap.memory.total_bytes = 0;
        snap.memory.free_bytes = 0;
        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert!(alerts.iter().all(|a| a.key != "low_memory"));
    }

    #[test]
    fn healthy_system_produces_no_alerts() {
        let mut state = AlertState::new();
        let alerts = evaluate(&healthy_snapshot(), 8, &mut state, Duration::from_secs(300), Instant::now());
        assert!(alerts.is_empty());
    }

    #[test]
    fn high_load_with_no_top_cpu_process_does_not_panic_or_fire() {
        // `top_cpu` isn't guaranteed non-empty by anything -- `evaluate`
        // takes an arbitrary `Snapshot` -- so a threshold breach with
        // nothing to name as the top consumer must be a no-op, not a panic.
        let mut snap = healthy_snapshot();
        snap.load_avg.one = 100.0;
        snap.top_cpu = vec![];
        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert!(alerts.iter().all(|a| a.key != "high_load"));
    }

    #[test]
    fn swap_pressure_with_no_top_mem_process_does_not_panic_or_fire() {
        let mut snap = healthy_snapshot();
        snap.memory.swap_used_bytes = SWAP_THRESHOLD_BYTES + 1;
        snap.top_mem = vec![];
        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert!(alerts.iter().all(|a| a.key != "swap_pressure"));
    }

    #[test]
    fn low_memory_with_no_top_mem_process_does_not_panic_or_fire() {
        let mut snap = healthy_snapshot();
        snap.memory.free_bytes = 1; // well under LOW_FREE_MEM_RATIO of total_bytes
        snap.top_mem = vec![];
        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert!(alerts.iter().all(|a| a.key != "low_memory"));
    }

    #[test]
    fn evaluate_battery_is_none_without_a_battery_reading() {
        let mut snap = healthy_snapshot();
        snap.battery = None;
        let mut state = AlertState::new();
        assert!(evaluate_battery(&snap, None, &mut state, Duration::from_secs(300), Instant::now()).is_none());
    }

    #[test]
    fn evaluate_battery_is_none_when_discharging_without_a_percentage() {
        let mut snap = healthy_snapshot();
        snap.battery = Some(crate::BatteryInfo { percentage: None, charging: Some(false), remaining_secs: None, raw: "test".to_string() });
        let mut state = AlertState::new();
        assert!(evaluate_battery(&snap, None, &mut state, Duration::from_secs(300), Instant::now()).is_none());
    }

    #[test]
    fn high_load_fires_and_then_respects_cooldown() {
        let mut snap = healthy_snapshot();
        snap.load_avg.one = 50.0;
        snap.top_cpu = vec![proc(42, "hog", 95.0, 500)];

        let mut state = AlertState::new();
        let now = Instant::now();
        let first = evaluate(&snap, 4, &mut state, Duration::from_secs(300), now);
        assert_eq!(first.len(), 1);
        assert_eq!(first[0].key, "high_load");
        assert!(first[0].message.contains("hog"));
        assert_eq!(first[0].command.as_deref(), Some("hog"));

        let second = evaluate(&snap, 4, &mut state, Duration::from_secs(300), now);
        assert!(second.is_empty(), "second call within cooldown must not re-fire");
    }

    #[test]
    fn high_load_refires_after_cooldown_elapses() {
        let mut snap = healthy_snapshot();
        snap.load_avg.one = 50.0;
        snap.top_cpu = vec![proc(42, "hog", 95.0, 500)];

        let mut state = AlertState::new();
        let t0 = Instant::now();
        let cooldown = Duration::from_millis(10);
        assert_eq!(evaluate(&snap, 4, &mut state, cooldown, t0).len(), 1);

        let t1 = t0 + Duration::from_millis(20);
        assert_eq!(evaluate(&snap, 4, &mut state, cooldown, t1).len(), 1);
    }

    #[test]
    fn swap_pressure_alert_names_top_memory_consumer() {
        let mut snap = healthy_snapshot();
        snap.memory.swap_used_bytes = 3 * 1024 * 1024 * 1024;
        snap.top_mem = vec![proc(7, "leaky", 2.0, 4000)];

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].key, "swap_pressure");
        assert!(alerts[0].message.contains("leaky"));
    }

    #[test]
    fn swap_pressure_cites_process_group_when_materially_larger_than_single_top() {
        let mut snap = healthy_snapshot();
        snap.memory.swap_used_bytes = 3 * 1024 * 1024 * 1024;
        snap.top_mem = vec![proc(7, "pycharm", 2.0, 4000)]; // 4000 MB single top
        snap.top_mem_groups = vec![ProcGroup {
            name: "Renderer Helper".to_string(),
            count: 12,
            total_cpu_pct: 6.0,
            total_mem_bytes: 8_000_000_000, // 2x the single top -- over GROUP_VS_SINGLE_RATIO
            top_pid: 42,
            oldest: vec![],
        }];

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("12"), "should cite the group's process count");
        assert!(alerts[0].message.contains("Renderer Helper"));
        assert!(!alerts[0].message.contains("pycharm"), "the dwarfed single process shouldn't be cited instead");
    }

    #[test]
    fn swap_pressure_ignores_group_when_not_materially_larger_than_single_top() {
        let mut snap = healthy_snapshot();
        snap.memory.swap_used_bytes = 3 * 1024 * 1024 * 1024;
        snap.top_mem = vec![proc(7, "pycharm", 2.0, 19000)]; // 19000 MB single top
        snap.top_mem_groups = vec![ProcGroup {
            name: "Renderer Helper".to_string(),
            count: 3,
            total_cpu_pct: 2.0,
            total_mem_bytes: 600_000_000, // well under the single top -- no reason to prefer it
            top_pid: 42,
            oldest: vec![],
        }];

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("pycharm"));
        assert!(!alerts[0].message.contains("Renderer Helper"));
    }

    #[test]
    fn low_memory_cites_process_group_when_materially_larger() {
        let mut snap = healthy_snapshot();
        snap.memory.free_bytes = 100_000_000; // 0.625% of 16GB total -- under LOW_FREE_MEM_RATIO
        snap.top_mem = vec![proc(7, "pycharm", 2.0, 3000)];
        snap.top_mem_groups = vec![ProcGroup {
            name: "node".to_string(),
            count: 20,
            total_cpu_pct: 10.0,
            total_mem_bytes: 6_000_000_000,
            top_pid: 99,
            oldest: vec![],
        }];

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].key, "low_memory");
        assert!(alerts[0].message.contains("20"));
        assert!(alerts[0].message.contains("node"));
    }

    #[test]
    fn low_disk_space_fires_with_mount_point_in_key() {
        let mut snap = healthy_snapshot();
        snap.disks = vec![DiskInfo {
            mount_point: "/".to_string(),
            total_bytes: 500_000_000_000,
            available_bytes: 5_000_000_000,
            used_pct: 99.0,
        }];

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].key, "low_disk:/");
        assert!(alerts[0].message.contains('%'));
        assert_eq!(alerts[0].command, None, "disk alerts aren't about a specific process");
    }

    #[test]
    fn high_connection_count_fires_with_state_breakdown_in_message() {
        let mut snap = healthy_snapshot();
        snap.connections = Some(ConnectionCounts {
            established: 1800,
            listen: 20,
            time_wait: 30,
            close_wait: 10,
            other: 0,
            total: 1860,
            incoming: 0,
        });

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].key, "high_connection_count");
        assert!(alerts[0].message.contains("1860"));
    }

    #[test]
    fn incoming_connections_fires_when_over_threshold() {
        let mut snap = healthy_snapshot();
        snap.connections = Some(ConnectionCounts {
            established: 50,
            listen: 5,
            time_wait: 0,
            close_wait: 0,
            other: 0,
            total: 55,
            incoming: 25,
        });

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].key, "incoming_connections");
        assert!(alerts[0].message.contains("25"));
    }

    #[test]
    fn normal_connection_counts_produce_no_network_alerts() {
        let mut snap = healthy_snapshot();
        snap.connections = Some(ConnectionCounts {
            established: 200,
            listen: 20,
            time_wait: 10,
            close_wait: 5,
            other: 0,
            total: 235,
            incoming: 2,
        });

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert!(alerts.is_empty());
    }

    #[test]
    fn format_age_picks_the_coarsest_nonzero_unit() {
        assert_eq!(format_age(45), "45s");
        assert_eq!(format_age(125), "2m");
        assert_eq!(format_age(3 * 3600 + 61), "3h");
        assert_eq!(format_age(14 * 86400 + 100), "14d");
    }

    #[test]
    fn format_oldest_samples_renders_pid_ppid_and_age() {
        let group = ProcGroup {
            name: "node".to_string(),
            count: 2,
            total_cpu_pct: 0.0,
            total_mem_bytes: 0,
            top_pid: 1,
            oldest: vec![
                ProcSample { pid: 41181, ppid: Some(40723), run_time_secs: 14 * 86400 },
                ProcSample { pid: 9328, ppid: None, run_time_secs: 3 * 86400 },
            ],
        };
        let rendered = format_oldest_samples(&group);
        assert_eq!(rendered, "pid 41181 (ppid 40723, 14d), pid 9328 (ppid none, 3d)");
    }

    #[test]
    fn high_process_count_fires_for_a_large_idle_group_with_pid_detail() {
        let mut snap = healthy_snapshot();
        snap.top_mem_groups = vec![ProcGroup {
            name: "node".to_string(),
            count: 224,
            total_cpu_pct: 0.0,
            total_mem_bytes: 9_600_000_000,
            top_pid: 67085,
            oldest: vec![ProcSample { pid: 41181, ppid: Some(40723), run_time_secs: 14 * 86400 }],
        }];

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert_eq!(alerts.len(), 1);
        assert_eq!(alerts[0].key, "high_process_count:node");
        assert_eq!(alerts[0].target.as_deref(), Some("node"));
        assert!(alerts[0].message.contains("224"));
        assert!(alerts[0].message.contains("pid 41181"));
        assert!(alerts[0].message.contains("ppid 40723"));
        assert!(alerts[0].message.contains("14d"));
    }

    #[test]
    fn high_process_count_ignores_a_large_group_that_is_actually_busy() {
        // Many Chrome renderer helpers actually doing work (video, decode,
        // scroll) shouldn't be flagged as leaked -- only a group that's
        // both large AND collectively near-idle should.
        let mut snap = healthy_snapshot();
        snap.top_mem_groups = vec![ProcGroup {
            name: "Google Chrome Helper (Renderer)".to_string(),
            count: 150,
            total_cpu_pct: 40.0, // well above LEAKED_GROUP_CPU_IDLE_PCT
            total_mem_bytes: 9_000_000_000,
            top_pid: 1,
            oldest: vec![],
        }];

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert!(alerts.iter().all(|a| !a.key.starts_with("high_process_count")));
    }

    #[test]
    fn high_process_count_ignores_a_small_idle_group() {
        // A handful of idle processes of the same name is completely
        // normal -- only the combination of a large count AND near-zero
        // CPU is the leak signal.
        let mut snap = healthy_snapshot();
        snap.top_mem_groups = vec![ProcGroup {
            name: "helper".to_string(),
            count: 5,
            total_cpu_pct: 0.0,
            total_mem_bytes: 500_000_000,
            top_pid: 1,
            oldest: vec![],
        }];

        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 8, &mut state, Duration::from_secs(300), Instant::now());
        assert!(alerts.iter().all(|a| !a.key.starts_with("high_process_count")));
    }

    #[test]
    fn cpu_hog_requires_sustained_streak_before_firing() {
        let mut snap = healthy_snapshot();
        snap.top_cpu = vec![proc(99, "runaway", 95.0, 200)];

        let mut state = AlertState::new();
        let now = Instant::now();
        let cooldown = Duration::from_secs(300);

        assert!(evaluate(&snap, 8, &mut state, cooldown, now).is_empty(), "1st sample: streak=1, no fire");
        assert!(evaluate(&snap, 8, &mut state, cooldown, now).is_empty(), "2nd sample: streak=2, no fire");
        let third = evaluate(&snap, 8, &mut state, cooldown, now);
        assert_eq!(third.len(), 1, "3rd sample: streak=3, should fire");
        assert_eq!(third[0].key, "cpu_hog:99");
    }

    #[test]
    fn cpu_hog_captures_the_process_command_line_at_fire_time() {
        // Guards against pid-reuse misattribution in the async diagnosis
        // (see Alert::command's doc comment and the live incident that
        // motivated it) -- the command must be captured here, synchronously,
        // not looked up again later by pid.
        let mut snap = healthy_snapshot();
        snap.top_cpu = vec![ProcInfo {
            pid: 99,
            ppid: None,
            name: "claude".to_string(),
            cpu_pct: 95.0,
            mem_bytes: 200_000_000,
            run_time_secs: 0,
            cmd: "bfs -S dfs / -path *".to_string(),
        }];
        let mut state = AlertState::new();
        let now = Instant::now();
        let cooldown = Duration::from_secs(300);
        evaluate(&snap, 8, &mut state, cooldown, now);
        evaluate(&snap, 8, &mut state, cooldown, now);
        let third = evaluate(&snap, 8, &mut state, cooldown, now);
        assert_eq!(third[0].command.as_deref(), Some("bfs -S dfs / -path *"));
    }

    #[test]
    fn self_process_note_is_empty_for_a_third_party_process() {
        assert_eq!(self_process_note("pycharm"), "");
    }

    #[test]
    fn self_process_note_flags_vigils_own_process() {
        assert!(self_process_note("vigil").contains("vigil's own process"));
    }

    #[test]
    fn high_load_annotates_the_message_when_vigil_itself_is_the_top_consumer() {
        let mut snap = healthy_snapshot();
        snap.load_avg.one = 50.0;
        snap.top_cpu = vec![proc(1, "vigil", 61.0, 40)];
        let mut state = AlertState::new();
        let alerts = evaluate(&snap, 4, &mut state, Duration::from_secs(300), Instant::now());
        assert_eq!(alerts.len(), 1);
        assert!(alerts[0].message.contains("vigil's own process"));
    }

    #[test]
    fn cpu_hog_annotates_the_message_when_vigil_itself_is_the_top_consumer() {
        let mut snap = healthy_snapshot();
        snap.top_cpu = vec![proc(1, "vigil", 95.0, 40)];
        let mut state = AlertState::new();
        let now = Instant::now();
        let cooldown = Duration::from_secs(300);
        evaluate(&snap, 8, &mut state, cooldown, now);
        evaluate(&snap, 8, &mut state, cooldown, now);
        let third = evaluate(&snap, 8, &mut state, cooldown, now);
        assert_eq!(third.len(), 1);
        assert!(third[0].message.contains("vigil's own process"));
    }

    #[test]
    fn cpu_hog_streak_resets_when_process_calms_down() {
        let mut snap = healthy_snapshot();
        snap.top_cpu = vec![proc(99, "spiky", 95.0, 200)];
        let mut state = AlertState::new();
        let now = Instant::now();
        let cooldown = Duration::from_secs(300);

        evaluate(&snap, 8, &mut state, cooldown, now);
        evaluate(&snap, 8, &mut state, cooldown, now);
        assert_eq!(state.cpu_hog_streak.get(&99), Some(&2));

        snap.top_cpu = vec![proc(99, "spiky", 10.0, 200)];
        evaluate(&snap, 8, &mut state, cooldown, now);
        assert_eq!(state.cpu_hog_streak.get(&99), None, "streak must reset once CPU usage drops");
    }

    fn battery(percentage: u8, charging: Option<bool>, remaining_secs: Option<u64>) -> crate::BatteryInfo {
        crate::BatteryInfo {
            percentage: Some(percentage),
            charging,
            remaining_secs,
            raw: "test".to_string(),
        }
    }

    #[test]
    fn no_battery_alert_when_charging_or_full() {
        let mut snap = healthy_snapshot();
        snap.battery = Some(battery(10, Some(true), None));
        let mut state = AlertState::new();
        assert!(evaluate_battery(&snap, None, &mut state, Duration::from_secs(300), Instant::now()).is_none());

        snap.battery = Some(battery(80, Some(false), None));
        assert!(evaluate_battery(&snap, None, &mut state, Duration::from_secs(300), Instant::now()).is_none());
    }

    #[test]
    fn low_battery_fires_with_top_cpu_hint() {
        let mut snap = healthy_snapshot();
        snap.battery = Some(battery(15, Some(false), Some(3600)));
        snap.top_cpu = vec![proc(7, "hungry", 88.0, 900)];

        let mut state = AlertState::new();
        let alert = evaluate_battery(&snap, None, &mut state, Duration::from_secs(300), Instant::now())
            .expect("should fire at 15% discharging");
        assert_eq!(alert.key, "battery_low");
        assert!(alert.message.contains("15%"));
        assert!(alert.message.contains("hungry"));
        assert!(alert.message.contains("1h00m"), "falls back to pmset's own remaining_secs when no trend ETA: {}", alert.message);
    }

    #[test]
    fn low_battery_prefers_trend_eta_over_pmset_estimate() {
        let mut snap = healthy_snapshot();
        snap.battery = Some(battery(15, Some(false), Some(3600)));

        let mut state = AlertState::new();
        let alert = evaluate_battery(
            &snap,
            Some(Duration::from_secs(42 * 60)),
            &mut state,
            Duration::from_secs(300),
            Instant::now(),
        )
        .unwrap();
        assert!(alert.message.contains("42m"));
    }

    #[test]
    fn low_battery_falls_back_to_unknown_eta_when_neither_source_has_one() {
        let mut snap = healthy_snapshot();
        snap.battery = Some(battery(10, Some(false), None)); // no pmset remaining_secs either
        let mut state = AlertState::new();
        let alert = evaluate_battery(&snap, None, &mut state, Duration::from_secs(300), Instant::now()).unwrap();
        assert!(alert.message.contains("unknown"));
    }

    #[test]
    fn low_battery_respects_cooldown() {
        let mut snap = healthy_snapshot();
        snap.battery = Some(battery(10, Some(false), Some(600)));
        let mut state = AlertState::new();
        let now = Instant::now();
        let cooldown = Duration::from_secs(300);

        assert!(evaluate_battery(&snap, None, &mut state, cooldown, now).is_some());
        assert!(evaluate_battery(&snap, None, &mut state, cooldown, now).is_none());
    }

    #[test]
    fn recent_alerts_context_excludes_self_and_includes_others() {
        let mut recent = RecentAlerts::new();
        let now = Instant::now();
        recent.record("swap_pressure", "Swap usage 13.7 GB", now);
        recent.record("low_memory", "Only 3.1% memory free", now);
        recent.record("cpu_hog:37489", "pycharm has held 111% CPU", now);

        let ctx = recent.context_excluding("cpu_hog:37489", now).unwrap();
        assert!(ctx.contains("Swap usage 13.7 GB"));
        assert!(ctx.contains("Only 3.1% memory free"));
        assert!(!ctx.contains("111% CPU"), "must not include the alert's own message");
    }

    #[test]
    fn recent_alerts_context_is_none_when_nothing_else_fired() {
        let mut recent = RecentAlerts::new();
        let now = Instant::now();
        recent.record("cpu_hog:1", "only this one", now);
        assert!(recent.context_excluding("cpu_hog:1", now).is_none());
        assert!(recent.context_excluding("cpu_hog:2", now).unwrap().contains("only this one"));
    }

    #[test]
    fn recent_alerts_context_drops_entries_outside_the_window() {
        let mut recent = RecentAlerts::new();
        let now = Instant::now();
        recent.record("swap_pressure", "stale", now - RECENT_ALERTS_WINDOW - Duration::from_secs(1));
        recent.record("cpu_hog:1", "current", now);

        assert!(recent.context_excluding("cpu_hog:1", now).is_none(), "the swap_pressure entry is too old to count");
    }

    #[test]
    fn recent_alerts_caps_capacity_dropping_oldest_first() {
        let mut recent = RecentAlerts::new();
        let now = Instant::now();
        for i in 0..(RECENT_ALERTS_CAPACITY + 3) {
            recent.record(&format!("key{i}"), &format!("msg{i}"), now);
        }
        let ctx = recent.context_excluding("nonexistent", now).unwrap();
        assert!(!ctx.contains("msg0"), "oldest entries should have been evicted");
        assert!(ctx.contains(&format!("msg{}", RECENT_ALERTS_CAPACITY + 2)), "most recent entry must survive");
    }

    #[test]
    fn incident_tracker_first_firing_for_a_target_is_new() {
        let mut t = IncidentTracker::new();
        assert!(t.is_new_incident(Some("pycharm"), Duration::from_secs(600), Instant::now()));
    }

    #[test]
    fn incident_tracker_repeat_firing_within_timeout_is_not_new() {
        let mut t = IncidentTracker::new();
        let t0 = Instant::now();
        assert!(t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0));
        // Real field repeats arrived 5-13 minutes apart, gated by the
        // rule's own re-fire cooldown — well inside a 600s incident window.
        assert!(!t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0 + Duration::from_secs(300)));
    }

    #[test]
    fn incident_tracker_reopens_after_silence_exceeds_timeout() {
        let mut t = IncidentTracker::new();
        let t0 = Instant::now();
        assert!(t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0));
        assert!(t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0 + Duration::from_secs(900)));
    }

    #[test]
    fn incident_tracker_extends_the_open_window_on_every_firing() {
        let mut t = IncidentTracker::new();
        let t0 = Instant::now();
        assert!(t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0));
        // Keeps firing every 5 minutes, each within the window of the last.
        assert!(!t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0 + Duration::from_secs(300)));
        assert!(!t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0 + Duration::from_secs(600)));
        assert!(!t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0 + Duration::from_secs(900)));
        // Had the window not kept extending, this point (900s after t0,
        // but only 300s after the previous firing) would have reopened.
    }

    #[test]
    fn incident_tracker_treats_different_targets_independently() {
        let mut t = IncidentTracker::new();
        let t0 = Instant::now();
        assert!(t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0));
        assert!(t.is_new_incident(Some("Devin Helper (Renderer)"), Duration::from_secs(600), t0));
    }

    #[test]
    fn incident_tracker_always_new_when_target_is_unknown() {
        let mut t = IncidentTracker::new();
        let t0 = Instant::now();
        assert!(t.is_new_incident(None, Duration::from_secs(600), t0));
        assert!(t.is_new_incident(None, Duration::from_secs(600), t0));
    }

    #[test]
    fn incident_tracker_open_count_reflects_currently_open_targets() {
        let mut t = IncidentTracker::new();
        let t0 = Instant::now();
        assert_eq!(t.open_count(Duration::from_secs(600), t0), 0);

        t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0);
        assert_eq!(t.open_count(Duration::from_secs(600), t0), 1);

        t.is_new_incident(Some("Devin Helper (Renderer)"), Duration::from_secs(600), t0);
        assert_eq!(t.open_count(Duration::from_secs(600), t0), 2);
    }

    #[test]
    fn incident_tracker_open_count_excludes_targets_past_the_timeout() {
        let mut t = IncidentTracker::new();
        let t0 = Instant::now();
        t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0);
        let later = t0 + Duration::from_secs(900);
        assert_eq!(t.open_count(Duration::from_secs(600), later), 0);
    }

    #[test]
    fn incident_tracker_open_count_does_not_mutate_state() {
        let mut t = IncidentTracker::new();
        let t0 = Instant::now();
        t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0);
        // Calling open_count repeatedly must not itself extend the window
        // the way is_new_incident does.
        let _ = t.open_count(Duration::from_secs(600), t0 + Duration::from_secs(500));
        let _ = t.open_count(Duration::from_secs(600), t0 + Duration::from_secs(500));
        assert!(!t.is_new_incident(Some("pycharm"), Duration::from_secs(600), t0 + Duration::from_secs(599)));
    }
}
