//! Built-in board templates shared by the daemon and fake client.

use crate::db::{ColumnTarget, ColumnWiring};
use crate::protocol::{ColumnCreateParams, Trigger};

pub const PIPELINE_TEMPLATE: &str = "pipeline";
pub const PLAN_REVIEW_TEMPLATE: &str = "plan-review";
pub const DEFAULT_TEMPLATE_ALIAS: &str = "default";

const PLAN_PROMPT: &str = "You are in Research & Plan. Inspect the current code, history, references, ownership, and runtime path before proposing work. Do not implement. Identify scope, risks, decisions, and focused validation. Add a concise plan to the card; do not create a separate plan document unless the card asks for one.";

const IMPLEMENT_PROMPT: &str = "You are in Implementation. Implement only the human-approved plan recorded on the card. Re-read the current repository state and follow its AGENTS.md instructions. Preserve unrelated changes, keep edits scoped, and run focused validation. Stop and report failure if the work now requires an unapproved architecture, public API, data contract, dependency, or broad maintenance change. Do not commit or push unless the card explicitly asks.";

const REVIEW_PROMPT: &str = "You are the AI Review orchestrator. Read the latest human-authored `Reviewers:` directive in the card description or comments. It must name exactly two reviewers, each `codex` or `claude`; duplicates are allowed. If it is absent or invalid, do not guess: report failure for human correction. Use the Herdr skill to start both reviewers in parallel in the same repository without taking focus. Give each the card goal, approved plan, current diff, and validation evidence. Reviewers must be independent, read-only, adversarial, and check that findings are current and not already fixed. Wait for both, verify and deduplicate their findings, then add one synthesis to the card. Report success only when no actionable findings remain; otherwise report failure so the human can veto, accept, or reroute the findings.";

const POLISH_PROMPT: &str = "You are in Polish. Handle only a small, local follow-up recorded on the card. Preserve unrelated changes, avoid scope expansion, and run focused validation. If the request is no longer small and local, report failure so a human can reroute it to planning or implementation.";

const PIPELINE_PLAN_PROMPT: &str = "You are in the PLAN stage. Produce a written implementation plan and save it under docs/plans/ (or .plans/). Do not write code.";
const PIPELINE_EXECUTE_PROMPT: &str = "You are in the EXECUTE stage. Implement the plan referenced in the card comments and run tests.";
const PIPELINE_REVIEW_PROMPT: &str = "You are in the REVIEW stage. Review the diff against the card description and the plan/execution comments. Be adversarial.";

pub struct TemplateDefinition {
    pub seed_name: Option<&'static str>,
    pub columns: Vec<ColumnCreateParams>,
    pub wiring: Vec<ColumnWiring>,
}

pub fn definition(name: &str, board_id: i64, seed_column_id: i64) -> Option<TemplateDefinition> {
    match name {
        PIPELINE_TEMPLATE => Some(pipeline(board_id, seed_column_id)),
        PLAN_REVIEW_TEMPLATE => Some(plan_review(board_id)),
        _ => None,
    }
}

fn column(board_id: i64, name: &str, trigger: Trigger) -> ColumnCreateParams {
    ColumnCreateParams {
        name: name.into(),
        board_id: Some(board_id),
        trigger: Some(trigger),
        ..Default::default()
    }
}

fn pipeline(board_id: i64, todo: i64) -> TemplateDefinition {
    let mut plan = column(board_id, "Plan", Trigger::Auto);
    plan.system_prompt = Some(PIPELINE_PLAN_PROMPT.into());
    let mut execute = column(board_id, "Execute", Trigger::Auto);
    execute.system_prompt = Some(PIPELINE_EXECUTE_PROMPT.into());
    let mut review = column(board_id, "Review", Trigger::Auto);
    review.system_prompt = Some(PIPELINE_REVIEW_PROMPT.into());
    review.model_override = Some("opus".into());
    TemplateDefinition {
        seed_name: None,
        columns: vec![
            plan,
            execute,
            review,
            column(board_id, "Human Review", Trigger::Manual),
            column(board_id, "Done", Trigger::Manual),
        ],
        wiring: vec![
            ColumnWiring {
                column_index: 0,
                on_success: Some(ColumnTarget::Created(1)),
                on_fail: Some(ColumnTarget::Existing(todo)),
            },
            ColumnWiring {
                column_index: 1,
                on_success: Some(ColumnTarget::Created(2)),
                on_fail: None,
            },
            ColumnWiring {
                column_index: 2,
                on_success: Some(ColumnTarget::Created(3)),
                on_fail: Some(ColumnTarget::Created(1)),
            },
        ],
    }
}

fn plan_review(board_id: i64) -> TemplateDefinition {
    let auto = |name: &str, prompt: &str, fresh_session: bool| {
        let mut value = column(board_id, name, Trigger::Auto);
        value.system_prompt = Some(prompt.into());
        value.harness_override = Some("codex".into());
        value.fresh_session = Some(fresh_session);
        value
    };
    TemplateDefinition {
        seed_name: Some("Backlog"),
        columns: vec![
            auto("Research & Plan", PLAN_PROMPT, true),
            column(board_id, "Plan Approval", Trigger::Manual),
            auto("Implementation", IMPLEMENT_PROMPT, false),
            auto("AI Review", REVIEW_PROMPT, true),
            auto("Polish", POLISH_PROMPT, false),
            column(board_id, "Human Review", Trigger::Manual),
            column(board_id, "Done", Trigger::Manual),
        ],
        wiring: vec![
            ColumnWiring {
                column_index: 0,
                on_success: Some(ColumnTarget::Created(1)),
                on_fail: Some(ColumnTarget::Created(1)),
            },
            ColumnWiring {
                column_index: 2,
                on_success: Some(ColumnTarget::Created(3)),
                on_fail: Some(ColumnTarget::Created(5)),
            },
            ColumnWiring {
                column_index: 3,
                on_success: Some(ColumnTarget::Created(5)),
                on_fail: Some(ColumnTarget::Created(5)),
            },
            ColumnWiring {
                column_index: 4,
                on_success: Some(ColumnTarget::Created(5)),
                on_fail: Some(ColumnTarget::Created(5)),
            },
        ],
    }
}
