//! Live, on-demand process data scoped to one incident's alert key —
//! deliberately not sourced from the agent's diagnosis prose (which is
//! written for a human to read, not a machine to parse) or from whatever
//! the snapshot looked like when the alert fired (which can be stale by
//! the time this window renders). `sysinfo` is queried fresh every time
//! `query_process_tree` is called.

use serde::Serialize;
use sysinfo::{Pid, ProcessStatus, System};

#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct ProcessNode {
    pub pid: u32,
    pub ppid: Option<u32>,
    pub name: String,
    pub cpu_pct: f32,
    pub mem_bytes: u64,
    pub run_time_secs: u64,
    pub is_zombie: bool,
}

/// What to scope a process-tree query to, derived from an alert key. An
/// alert key vigil doesn't currently name a specific process/group for
/// (e.g. `high_load`, `swap_pressure`) has nothing meaningful to scope a
/// tree to — `Scope::None` — and the caller should skip rendering a tree
/// section entirely rather than dumping every process on the machine.
#[derive(Debug, Clone, PartialEq)]
pub enum Scope {
    Pid(u32),
    Name(String),
    None,
}

/// Pure — parses vigil's alert-key conventions (`cpu_hog:<pid>`,
/// `high_process_count:<name>`) without touching the system. Mirrors the
/// same key shapes `agent::is_journal_worthy` on the main `vigil` crate
/// already gates on, kept here as an independent parse since `ui/` doesn't
/// share a crate with `vigil`.
pub fn scope_for_alert_key(alert_key: &str) -> Scope {
    if let Some(pid_str) = alert_key.strip_prefix("cpu_hog:") {
        if let Ok(pid) = pid_str.parse::<u32>() {
            return Scope::Pid(pid);
        }
    }
    if let Some(name) = alert_key.strip_prefix("high_process_count:") {
        if !name.is_empty() {
            return Scope::Name(name.to_string());
        }
    }
    Scope::None
}

/// Refreshes `sys` and returns every currently-running process matching
/// `scope`: for `Scope::Pid`, that one pid plus any process whose parent
/// chain leads back to it (its direct children — this project's incidents
/// have not needed grandchildren-of-children trees so far, and going
/// deeper risks pulling in unrelated system processes that happen to
/// share an ancestor far up the tree); for `Scope::Name`, every process
/// whose name matches exactly; for `Scope::None`, an empty list.
pub fn query_process_tree(sys: &mut System, scope: &Scope) -> Vec<ProcessNode> {
    sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);

    match scope {
        Scope::None => Vec::new(),
        Scope::Name(name) => sys
            .processes()
            .iter()
            .filter(|(_, p)| p.name().to_string_lossy() == *name)
            .map(|(pid, p)| to_node(pid, p))
            .collect(),
        Scope::Pid(target_pid) => {
            let target = Pid::from_u32(*target_pid);
            sys.processes()
                .iter()
                .filter(|(pid, p)| **pid == target || p.parent() == Some(target))
                .map(|(pid, p)| to_node(pid, p))
                .collect()
        }
    }
}

fn to_node(pid: &Pid, p: &sysinfo::Process) -> ProcessNode {
    ProcessNode {
        pid: pid.as_u32(),
        ppid: p.parent().map(|p| p.as_u32()),
        name: p.name().to_string_lossy().to_string(),
        cpu_pct: p.cpu_usage(),
        mem_bytes: p.memory(),
        run_time_secs: p.run_time(),
        is_zombie: p.status() == ProcessStatus::Zombie,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_for_alert_key_parses_cpu_hog() {
        assert_eq!(scope_for_alert_key("cpu_hog:37489"), Scope::Pid(37489));
    }

    #[test]
    fn scope_for_alert_key_parses_high_process_count() {
        assert_eq!(scope_for_alert_key("high_process_count:node"), Scope::Name("node".to_string()));
    }

    #[test]
    fn scope_for_alert_key_falls_back_to_none_for_unrecognized_keys() {
        assert_eq!(scope_for_alert_key("high_load"), Scope::None);
        assert_eq!(scope_for_alert_key("swap_pressure"), Scope::None);
        assert_eq!(scope_for_alert_key("battery_low"), Scope::None);
    }

    #[test]
    fn scope_for_alert_key_falls_back_to_none_for_a_non_numeric_cpu_hog_pid() {
        assert_eq!(scope_for_alert_key("cpu_hog:not-a-number"), Scope::None);
    }

    #[test]
    fn query_process_tree_is_empty_for_scope_none() {
        let mut sys = System::new_all();
        assert_eq!(query_process_tree(&mut sys, &Scope::None), Vec::new());
    }

    #[test]
    fn query_process_tree_by_name_finds_this_test_process_on_this_machine() {
        // Real sysinfo call against the actual running test binary, same
        // convention as the main vigil crate's own snapshot.rs tests (see
        // AGENTS.md's testing section) -- this process is guaranteed to be
        // running while the test runs.
        let mut sys = System::new_all();
        let own_pid = sysinfo::get_current_pid().expect("must be able to read our own pid");
        sys.refresh_processes(sysinfo::ProcessesToUpdate::All, true);
        let own_name = sys.process(own_pid).expect("our own process must be visible to sysinfo").name().to_string_lossy().to_string();

        let nodes = query_process_tree(&mut sys, &Scope::Name(own_name.clone()));
        assert!(nodes.iter().any(|n| n.pid == own_pid.as_u32()), "expected to find our own pid among processes named {own_name:?}");
    }

    #[test]
    fn query_process_tree_by_pid_includes_the_target_pid_itself() {
        let mut sys = System::new_all();
        let own_pid = sysinfo::get_current_pid().expect("must be able to read our own pid");
        let nodes = query_process_tree(&mut sys, &Scope::Pid(own_pid.as_u32()));
        assert!(nodes.iter().any(|n| n.pid == own_pid.as_u32()));
    }

    #[test]
    fn query_process_tree_by_unmatched_name_is_empty() {
        let mut sys = System::new_all();
        let nodes = query_process_tree(&mut sys, &Scope::Name("definitely-not-a-real-process-name-xyz".to_string()));
        assert_eq!(nodes, Vec::new());
    }
}
