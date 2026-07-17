//! Verifies conditional checklist guidance preserves goal instructions and task data.
#![allow(dead_code)]

#[path = "../src/steering.rs"]
mod steering;

use codex_core::context::ContextualUserFragment;
use codex_core::context::InternalContextSource;
use codex_core::context::InternalModelContextFragment;
use codex_protocol::ThreadId;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ThreadGoal;
use codex_protocol::protocol::ThreadGoalStatus;
use codex_utils_template::Template;
use pretty_assertions::assert_eq;

#[test]
fn enabled_checklist_preserves_the_original_continuation_prompt() {
    let goal = test_goal("Finish the feature.");
    let original = Template::parse(include_str!("../templates/goals/continuation.md"))
        .expect("original continuation template")
        .render([
            ("objective", goal.objective.as_str()),
            ("tokens_used", "100"),
            ("token_budget", "10000"),
            ("remaining_tokens", "9900"),
        ])
        .expect("render original continuation prompt");
    assert!(original.contains("sources that are authoritative for the current objective"));
    assert!(original.contains("their authority depends on their relevance"));
    assert!(original.contains("call get_goal to re-ground on the active objective"));
    assert!(!original.contains("current worktree and external state as authoritative"));
    let expected: ResponseItem = ContextualUserFragment::into(InternalModelContextFragment::new(
        InternalContextSource::from_static("goal"),
        original,
    ));
    assert_eq!(
        steering::continuation_steering_item(&goal, /*update_plan_enabled*/ true),
        expected,
    );
}

#[test]
fn disabled_checklist_preserves_goal_text_that_mentions_the_tool() {
    let objective = "Inspect update_plan.\n\n## Plan tool\nThis is user task data.";
    let item = steering::continuation_steering_item(
        &test_goal(objective),
        /*update_plan_enabled*/ false,
    );
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected goal continuation message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected goal continuation text");
    };
    assert!(text.contains(objective));
    assert!(!text.contains("If update_plan is available"));
    assert!(text.contains("Completion audit:"));
}

#[test]
fn source_authority_survives_budget_and_objective_update_steering() {
    let mut budget_limited_goal = test_goal("Finish the feature.");
    budget_limited_goal.status = ThreadGoalStatus::BudgetLimited;
    budget_limited_goal.tokens_used = 10_100;
    let budget_limit = response_item_text(steering::budget_limit_steering_item(
        &budget_limited_goal,
    ));
    assert!(budget_limit.contains("without letting recent local artifacts redefine it"));

    let objective_updated =
        response_item_text(steering::objective_updated_steering_item(&test_goal(
            "Finish the revised feature.",
        )));
    assert!(objective_updated.contains("sources that are authoritative for the updated objective"));
    assert!(objective_updated.contains("their authority depends on their relevance"));
    assert!(objective_updated.contains("call get_goal to re-ground on the updated objective"));
    assert!(objective_updated.contains("without letting proximity, concreteness, or recency"));
}

fn response_item_text(item: ResponseItem) -> String {
    let ResponseItem::Message { content, .. } = item else {
        panic!("expected goal steering message");
    };
    let [ContentItem::InputText { text }] = content.as_slice() else {
        panic!("expected goal steering text");
    };
    text.clone()
}

fn test_goal(objective: &str) -> ThreadGoal {
    ThreadGoal {
        thread_id: ThreadId::new(),
        objective: objective.to_string(),
        status: ThreadGoalStatus::Active,
        token_budget: Some(10_000),
        tokens_used: 100,
        time_used_seconds: 0,
        created_at: 0,
        updated_at: 0,
    }
}
