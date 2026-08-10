//! The actual OS-boundary glue for `vigil fix <incident-file>`: parsing
//! the proposed plan out of an incident file, prompting for per-step
//! approval on stdin, spawning the scoped execute-agent, and appending its
//! report. Excluded from the coverage gate for the same reason
//! `agent_process.rs`/`investigate_process.rs` are — the pure plan
//! parsing/formatting this leans on is fully tested in `fixplan.rs`.

use std::io::{self, BufRead, Write};

pub fn run(incident_file: &str, agent_dir: &str) -> i32 {
    let path = std::path::Path::new(incident_file);
    let content = match std::fs::read_to_string(path) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("[vigil] failed to read {}: {e}", path.display());
            return 1;
        }
    };

    let Some(json) = crate::fixplan::extract_proposed_fix_json(&content) else {
        eprintln!(
            "[vigil] {} has no proposed fix — run `vigil investigate` first, or this incident didn't produce one",
            path.display()
        );
        return 1;
    };
    let plan = match crate::fixplan::parse_plan(json) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("[vigil] {e}");
            return 1;
        }
    };

    let stdin = io::stdin();
    let mut approved = Vec::with_capacity(plan.plan.len());
    for (i, step) in plan.plan.iter().enumerate() {
        print!(
            "[{}/{}] {} — {}\n    target: {}\nApprove? [y/N] ",
            i + 1,
            plan.plan.len(),
            step.category.as_str(),
            step.description,
            step.target_hint
        );
        io::stdout().flush().ok();
        let mut line = String::new();
        if stdin.lock().read_line(&mut line).is_err() {
            line.clear();
        }
        let answer = line.trim().eq_ignore_ascii_case("y") || line.trim().eq_ignore_ascii_case("yes");
        approved.push(answer);
    }

    if !approved.iter().any(|&a| a) {
        println!("No steps approved, nothing to execute.");
        return 0;
    }

    let plan_json = crate::fixplan::approved_steps_json(&plan.plan, &approved);
    match crate::agent_process::execute_fix(&plan_json, agent_dir) {
        Ok(report) => {
            let header = crate::fixplan::approved_header(&approved, &crate::incidents::human_timestamp());
            let body = format!("{header}\n\n{report}");
            match crate::incidents::append_fix_execution(path, &body) {
                Ok(_) => {
                    println!("{report}");
                    println!("\nFix execution appended to {}", path.display());
                    0
                }
                Err(e) => {
                    eprintln!("[vigil] fix ran but failed to save the report: {e}");
                    1
                }
            }
        }
        Err(e) => {
            eprintln!("[vigil] fix execution failed: {e}");
            1
        }
    }
}
