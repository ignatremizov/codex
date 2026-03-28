use super::*;
use crate::chatwidget::notifications::NotificationPreviewGraphemeLimits;
use pretty_assertions::assert_eq;

#[test]
fn plan_mode_prompt_notification_uses_dedicated_type_name() {
    let notification = Notification::PlanModePrompt {
        title: PLAN_IMPLEMENTATION_TITLE.to_string(),
    };
    assert!(notification.allowed_for(&Notifications::Custom(
        vec!["plan-mode-prompt".to_string(),]
    )));
    assert!(!notification.allowed_for(&Notifications::Custom(vec![
        "approval-requested".to_string(),
    ])));
    assert_eq!(
        notification.display(NotificationPreviewGraphemeLimits {
            agent_turn: 200,
            exec_approval: 30,
            user_input: 30,
        }),
        format!("Plan mode prompt: {PLAN_IMPLEMENTATION_TITLE}")
    );
}

#[test]
fn notification_previews_use_independent_grapheme_limits() {
    let limits = NotificationPreviewGraphemeLimits {
        agent_turn: 8,
        exec_approval: 9,
        user_input: 7,
    };
    let rendered = [
        Notification::AgentTurnComplete {
            response: "👩‍👩‍👧‍👦 Done safely".to_string(),
        }
        .display(limits),
        Notification::ExecApprovalRequested {
            command: "printf safely".to_string(),
        }
        .display(limits),
        Notification::UserInputRequested {
            question_count: 1,
            summary: Some("Choose 👩‍👩‍👧‍👦 now".to_string()),
        }
        .display(limits),
        Notification::UserInputRequested {
            question_count: 1,
            summary: None,
        }
        .display(limits),
        Notification::UserInputRequested {
            question_count: 2,
            summary: Some("ignored".to_string()),
        }
        .display(limits),
    ]
    .join("\n");
    insta::assert_snapshot!(rendered, @r###"
👩‍👩‍👧‍👦 Don...
Approval requested: printf...
Question requested: Choo...
Question requested
Questions requested: 2
"###);
}

#[test]
fn notification_preview_zero_and_small_limits_are_bounded() {
    let notification = Notification::AgentTurnComplete {
        response: "alpha beta".to_string(),
    };
    assert_eq!(
        notification.display(NotificationPreviewGraphemeLimits {
            agent_turn: 0,
            exec_approval: 30,
            user_input: 30,
        }),
        ""
    );
    assert_eq!(
        Notification::UserInputRequested {
            question_count: 1,
            summary: Some("abcdef".to_string()),
        }
        .display(NotificationPreviewGraphemeLimits {
            agent_turn: 200,
            exec_approval: 30,
            user_input: 2,
        }),
        "Question requested: ab"
    );
}

#[test]
fn notification_filters_remain_independent() {
    let notification = Notification::UserInputRequested {
        question_count: 1,
        summary: Some("Reasoning scope".to_string()),
    };
    assert!(notification.allowed_for(&Notifications::Custom(vec![
        "user-input-requested".to_string(),
    ])));
    assert!(!notification.allowed_for(&Notifications::Custom(vec![
        "plan-mode-prompt".to_string(),
    ])));
    assert!(!notification.allowed_for(&Notifications::Custom(vec!["async-question".to_string(),])));
}

#[test]
fn user_input_summary_preserves_full_text_and_handles_blank_questions() {
    let text = "Choose the environment that should receive this complete message";
    let mut questions = vec![ToolRequestUserInputQuestion {
        id: "environment".to_string(),
        header: format!(" {text} "),
        question: "fallback".to_string(),
        is_other: false,
        is_secret: false,
        options: None,
    }];
    assert_eq!(
        Notification::user_input_request_summary(&questions),
        Some(text.to_string())
    );
    questions[0].header = " \t".to_string();
    questions[0].question = format!(" {text} ");
    assert_eq!(
        Notification::user_input_request_summary(&questions),
        Some(text.to_string())
    );
    questions[0].question = " \n".to_string();
    assert_eq!(Notification::user_input_request_summary(&questions), None);
    assert_eq!(Notification::user_input_request_summary(&[]), None);
    assert_eq!(
        Notification::agent_turn_preview(" \n\t ", /*max_graphemes*/ 10),
        None
    );
}
