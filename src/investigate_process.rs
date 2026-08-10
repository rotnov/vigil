//! The actual OS-boundary glue for `vigil investigate <alert-key>`:
//! resolving the incident file, taking a fresh snapshot, spawning the
//! agent, and appending its answer. Excluded from the coverage gate (see
//! AGENTS.md's testing section) for the same reason `agent_process.rs` is —
//! every branch here either spawns a real costly process or does file IO
//! whose failure modes are already covered where the pure logic lives
//! (`investigate.rs`, `incidents.rs`, `agent.rs`).

pub fn run(alert_key: &str, agent_dir: &str, incidents_dir: &str, watch_log: Option<&str>) -> i32 {
    let dir = std::path::Path::new(incidents_dir);
    let path = match crate::investigate::resolve_incident_file(dir, alert_key) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[vigil] {e}");
            return 1;
        }
    };

    let content = match std::fs::read_to_string(&path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vigil] failed to read {}: {e}", path.display());
            return 1;
        }
    };
    let rule_message = crate::incidents::extract_rule_message(&content).unwrap_or("(unknown alert)");

    let mut sys = sysinfo::System::new_all();
    let snap = crate::take_snapshot(&mut sys, 10);
    let snapshot_json = serde_json::to_string(&snap).unwrap();

    let command = crate::incidents::extract_command(&content);
    let question = crate::agent::build_diagnosis_question(rule_message, None, watch_log, command);

    match crate::agent_process::ask(&question, &snapshot_json, agent_dir) {
        Ok(answer) => match crate::incidents::append_diagnosis(&path, &answer) {
            Ok(_) => {
                println!("{answer}");
                println!("\nFull report: {}", path.display());
                0
            }
            Err(e) => {
                eprintln!("[vigil] investigation succeeded but failed to save it: {e}");
                1
            }
        },
        Err(e) => {
            eprintln!("[vigil] investigation failed: {e}");
            1
        }
    }
}
