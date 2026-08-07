//! Cheap, rule-based anomaly detection over a single `Snapshot`.
//!
//! This is intentionally dumb (fixed thresholds, no LLM). It only decides
//! *whether to notify the user* and drafts a human-readable suggestion —
//! it never takes any corrective action itself. Any actual fix requires a
//! separate, explicit user confirmation outside this tool.

use crate::Snapshot;
use std::collections::HashMap;
use std::time::{Duration, Instant};

const SWAP_THRESHOLD_BYTES: u64 = 2 * 1024 * 1024 * 1024; // 2 GB
const LOW_FREE_MEM_RATIO: f64 = 0.05;
const CPU_HOG_THRESHOLD_PCT: f32 = 90.0;
const CPU_HOG_STREAK_REQUIRED: u32 = 3;
const LOW_DISK_AVAILABLE_BYTES: u64 = 10 * 1024 * 1024 * 1024; // 10 GB

pub struct Alert {
    pub key: String,
    pub title: String,
    pub message: String,
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

    fn try_fire(&mut self, now: Instant, cooldown: Duration, key: &str, title: &str, message: String, out: &mut Vec<Alert>) {
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
            });
        }
    }
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
                    "Load average {:.1} (threshold {:.1} for {} cores). Top consumer: {} ({:.0}% CPU). Suggestion: check the process and restart it if needed.",
                    snap.load_avg.one, load_threshold, cpu_count, top.name, top.cpu_pct
                ),
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
                    "Swap usage {:.1} GB — memory is running out. Top consumer: {} ({:.0} MB). Suggestion: close it or restart the machine.",
                    snap.memory.swap_used_bytes as f64 / 1e9,
                    top.name,
                    top.mem_bytes as f64 / 1e6
                ),
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
                        "Only {:.1}% memory free. Top consumer: {} ({:.0} MB). Suggestion: close unused applications.",
                        free_ratio * 100.0,
                        top.name,
                        top.mem_bytes as f64 / 1e6
                    ),
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
        if let Some(p) = snap.top_cpu.iter().find(|p| p.pid == pid) {
            state.try_fire(
                now,
                cooldown,
                &format!("cpu_hog:{pid}"),
                "vigil: process hogging CPU",
                format!(
                    "{} (pid {}) has held {:.0}% CPU for {} consecutive samples. Suggestion: check the process and decide whether to restart it.",
                    p.name, p.pid, p.cpu_pct, streak
                ),
                &mut alerts,
            );
        }
    }

    alerts
}

/// Fire a native macOS notification. Purely informational — never runs any
/// remediation itself. The user acts on the suggestion manually (or, in a
/// future agent-driven mode, only after explicit confirmation).
pub fn notify(alert: &Alert) {
    let script = format!(
        "display notification {} with title {} sound name \"Glass\"",
        osa_quote(&alert.message),
        osa_quote(&alert.title)
    );
    let _ = std::process::Command::new("osascript")
        .arg("-e")
        .arg(script)
        .output();
}

fn osa_quote(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{DiskInfo, LoadAvg, MemoryInfo, ProcInfo, Snapshot};

    fn proc(pid: u32, name: &str, cpu_pct: f32, mem_mb: u64) -> ProcInfo {
        ProcInfo {
            pid,
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
            top_cpu: vec![proc(1, "idle_app", 5.0, 100)],
            top_mem: vec![proc(1, "idle_app", 5.0, 100)],
        }
    }

    #[test]
    fn healthy_system_produces_no_alerts() {
        let mut state = AlertState::new();
        let alerts = evaluate(&healthy_snapshot(), 8, &mut state, Duration::from_secs(300), Instant::now());
        assert!(alerts.is_empty());
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
}
