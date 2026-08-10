//! Parses and represents the `## Proposed fix` JSON plan an investigating
//! agent may append to an incident file, and the small amount of pure logic
//! around it (selecting the user-approved subset, building the report
//! header). Execution itself — actually running the plan through a
//! dedicated agent session — lives in `fix_process.rs`; this file has no
//! process-spawning or file IO at all, so it's fully unit-tested.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FixCategory {
    KillProcess,
    DeletePath,
    SystemSetting,
}

impl FixCategory {
    /// The exact string `vigil-agent execute` expects among its plan
    /// steps' `category` values — kept in sync with `serde`'s
    /// `rename_all` above via a test, since the two are independently
    /// spelled out.
    pub fn as_str(&self) -> &'static str {
        match self {
            FixCategory::KillProcess => "kill_process",
            FixCategory::DeletePath => "delete_path",
            FixCategory::SystemSetting => "system_setting",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PlanStep {
    pub category: FixCategory,
    pub description: String,
    pub target_hint: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Plan {
    pub plan: Vec<PlanStep>,
}

/// Extract the fenced ```json block that follows a `## Proposed fix`
/// heading in an incident file's markdown. `None` if there's no such
/// heading, or the heading has no well-formed fenced JSON block after it —
/// both read as "this incident has no proposed fix", not an error.
pub fn extract_proposed_fix_json(content: &str) -> Option<&str> {
    let heading_pos = content.find("## Proposed fix")?;
    let after_heading = &content[heading_pos..];
    let fence_start = after_heading.find("```json")?;
    let json_start = fence_start + "```json".len();
    let rest = &after_heading[json_start..];
    let fence_end = rest.find("```")?;
    let json = rest[..fence_end].trim();
    if json.is_empty() {
        None
    } else {
        Some(json)
    }
}

pub fn parse_plan(json: &str) -> Result<Plan, String> {
    serde_json::from_str(json).map_err(|e| format!("failed to parse proposed fix plan: {e}"))
}

/// JSON array of just the approved steps, in their original order — the
/// exact text handed to `vigil-agent execute --plan-json`. `approved` must
/// be the same length as `steps`; a mismatch is a caller bug (the
/// interactive prompt loop always builds one bool per step), not a
/// user-facing error, so this asserts rather than returning `Result`.
pub fn approved_steps_json(steps: &[PlanStep], approved: &[bool]) -> String {
    assert_eq!(steps.len(), approved.len(), "approved.len() must match steps.len()");
    let approved_steps: Vec<&PlanStep> =
        steps.iter().zip(approved.iter()).filter(|(_, &a)| a).map(|(s, _)| s).collect();
    serde_json::to_string(&approved_steps).expect("PlanStep serialization cannot fail")
}

/// The `_Approved: ..._` line prefixed to the execute-agent's own report
/// before it's appended under the incident file's `## Fix execution`
/// heading. `approved` is 0-indexed positions into the plan's steps; the
/// header numbers them 1-indexed to match what the interactive prompt
/// showed the user.
pub fn approved_header(approved: &[bool], timestamp: &str) -> String {
    let total = approved.len();
    let numbers: Vec<String> =
        approved.iter().enumerate().filter(|(_, &a)| a).map(|(i, _)| (i + 1).to_string()).collect();
    format!("_Approved: {timestamp} (steps {} of {total})_", numbers.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE_PLAN_JSON: &str = r#"{
      "plan": [
        {
          "category": "kill_process",
          "description": "Kill the stale claude session",
          "target_hint": "claude --worktree nervous-cori-c94163 --resume skill-adopt"
        },
        {
          "category": "delete_path",
          "description": "Remove the orphaned node_modules cache",
          "target_hint": "/tmp/some/node_modules/.cache"
        }
      ]
    }"#;

    #[test]
    fn fix_category_as_str_matches_its_serde_rename() {
        for cat in [FixCategory::KillProcess, FixCategory::DeletePath, FixCategory::SystemSetting] {
            let serialized = serde_json::to_string(&cat).unwrap();
            assert_eq!(serialized, format!("\"{}\"", cat.as_str()));
        }
    }

    #[test]
    fn extract_proposed_fix_json_finds_the_fenced_block_after_the_heading() {
        let content = format!("# vigil: cpu hog\n\n## Diagnosis\n\ntext\n\n## Proposed fix\n\n```json\n{SAMPLE_PLAN_JSON}\n```\n");
        let extracted = extract_proposed_fix_json(&content).unwrap();
        assert!(extracted.contains("kill_process"));
    }

    #[test]
    fn extract_proposed_fix_json_is_none_without_the_heading() {
        let content = "# vigil: cpu hog\n\n## Diagnosis\n\nno fix here.\n";
        assert_eq!(extract_proposed_fix_json(content), None);
    }

    #[test]
    fn extract_proposed_fix_json_is_none_when_heading_has_no_fenced_block() {
        let content = "# vigil\n\n## Proposed fix\n\n(none identified)\n";
        assert_eq!(extract_proposed_fix_json(content), None);
    }

    #[test]
    fn extract_proposed_fix_json_is_none_for_an_empty_fenced_block() {
        let content = "## Proposed fix\n\n```json\n\n```\n";
        assert_eq!(extract_proposed_fix_json(content), None);
    }

    #[test]
    fn parse_plan_reads_categories_descriptions_and_target_hints() {
        let plan = parse_plan(SAMPLE_PLAN_JSON).unwrap();
        assert_eq!(plan.plan.len(), 2);
        assert_eq!(plan.plan[0].category, FixCategory::KillProcess);
        assert_eq!(plan.plan[0].description, "Kill the stale claude session");
        assert_eq!(plan.plan[1].category, FixCategory::DeletePath);
        assert_eq!(plan.plan[1].target_hint, "/tmp/some/node_modules/.cache");
    }

    #[test]
    fn parse_plan_rejects_an_unknown_category() {
        let json = r#"{"plan": [{"category": "reboot_machine", "description": "d", "target_hint": "h"}]}"#;
        assert!(parse_plan(json).is_err());
    }

    #[test]
    fn parse_plan_rejects_malformed_json() {
        assert!(parse_plan("not json").is_err());
    }

    #[test]
    fn approved_steps_json_includes_only_approved_steps_in_order() {
        let plan = parse_plan(SAMPLE_PLAN_JSON).unwrap();
        let json = approved_steps_json(&plan.plan, &[false, true]);
        let reparsed: Vec<PlanStep> = serde_json::from_str(&json).unwrap();
        assert_eq!(reparsed.len(), 1);
        assert_eq!(reparsed[0].category, FixCategory::DeletePath);
    }

    #[test]
    fn approved_steps_json_is_an_empty_array_when_nothing_is_approved() {
        let plan = parse_plan(SAMPLE_PLAN_JSON).unwrap();
        assert_eq!(approved_steps_json(&plan.plan, &[false, false]), "[]");
    }

    #[test]
    #[should_panic(expected = "approved.len() must match steps.len()")]
    fn approved_steps_json_panics_on_a_length_mismatch() {
        let plan = parse_plan(SAMPLE_PLAN_JSON).unwrap();
        approved_steps_json(&plan.plan, &[true]);
    }

    #[test]
    fn approved_header_numbers_only_approved_steps_one_indexed() {
        let header = approved_header(&[false, true, true], "2026-08-09 02:30");
        assert_eq!(header, "_Approved: 2026-08-09 02:30 (steps 2, 3 of 3)_");
    }

    #[test]
    fn approved_header_handles_a_single_step_plan() {
        let header = approved_header(&[true], "2026-08-09 02:30");
        assert_eq!(header, "_Approved: 2026-08-09 02:30 (steps 1 of 1)_");
    }
}
