use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::UserAgentControlAction;
use codex_app_server_protocol::UserAgentControlStatus;
use codex_app_server_protocol::UserAgentForkMode;
use codex_protocol::openai_models::ReasoningEffort;

use super::*;

#[test]
fn renders_live_subtree_default_with_future_members() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "subtree-control".into(),
        action: UserAgentControlAction::SubtreeMessaging,
        authored_selector: Some("all".into()),
        target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67268".into()),
        reply_recipient_thread_id: None,
        observer_thread_id: None,
        authored_observer_selector: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        task_path: None,
        task: None,
        task_path_mapping: Vec::new(),
        agent_ref: Some("1".into()),
        nickname: Some("Main".into()),
        role: None,
        model: None,
        reasoning_effort: None,
        prompt_preview: None,
        resumed_target: false,
        fork_mode: None,
        observe_commentary: None,
        final_response: None,
        target_messages: Some(true),
        queue_input: None,
        status: UserAgentControlStatus::Succeeded,
        error: None,
    })
    .expect("subtree audit cell");
    let text = cell
        .display_lines(/*width*/ 160)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(text, @"
    • User changed subtree messaging for Main [default] (ref 1) (enabled)
      └ Applies to this supervisor and current/future descendants; explicit pair settings take precedence.
    ");
}

#[test]
fn renders_successful_user_agent_prompt() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "control-1".to_string(),
        action: UserAgentControlAction::Prompt,
        authored_selector: Some("2".to_string()),
        target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67269".to_string()),
        reply_recipient_thread_id: None,
        observer_thread_id: None,
        authored_observer_selector: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        task_path: None,
        task: None,
        task_path_mapping: Vec::new(),
        agent_ref: Some("2".to_string()),
        nickname: Some("Anscombe".to_string()),
        role: Some("reviewer".to_string()),
        model: None,
        reasoning_effort: None,
        prompt_preview: Some("Review the latest diff.".to_string()),
        resumed_target: false,
        fork_mode: None,
        observe_commentary: Some(true),
        final_response: Some(AgentFinalResponseHandling::Wake),
        target_messages: Some(true),
        queue_input: Some(false),
        status: UserAgentControlStatus::Succeeded,
        error: None,
    })
    .expect("control item should render");

    let rendered = cell
        .display_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    • User sent to Anscombe [reviewer] (ref 2) (commentary · wake · allow replies)
      └ Review the latest diff.
    ");
}

#[test]
fn renders_child_to_main_prompt_with_main_identity() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "control-main".to_string(),
        action: UserAgentControlAction::Prompt,
        authored_selector: Some("main".to_string()),
        target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67268".to_string()),
        reply_recipient_thread_id: None,
        observer_thread_id: None,
        authored_observer_selector: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        task_path: None,
        task: None,
        task_path_mapping: Vec::new(),
        agent_ref: Some("1".to_string()),
        nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
        role: None,
        model: None,
        reasoning_effort: None,
        prompt_preview: Some("Please confirm.".to_string()),
        resumed_target: false,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Presentation),
        target_messages: Some(false),
        queue_input: Some(false),
        status: UserAgentControlStatus::Succeeded,
        error: None,
    })
    .expect("control item should render");

    let rendered = cell
        .display_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    • User sent to Main [default] (ref 1) (presentation)
      └ Please confirm.
    ");
}

#[test]
fn renders_successful_prompt_with_post_admission_warning() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "control-warning".to_string(),
        action: UserAgentControlAction::Prompt,
        authored_selector: Some("2".to_string()),
        target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67269".to_string()),
        reply_recipient_thread_id: None,
        observer_thread_id: None,
        authored_observer_selector: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        task_path: None,
        task: None,
        task_path_mapping: Vec::new(),
        agent_ref: Some("2".to_string()),
        nickname: Some("Anscombe".to_string()),
        role: Some("reviewer".to_string()),
        model: None,
        reasoning_effort: None,
        prompt_preview: Some("Review the latest diff.".to_string()),
        resumed_target: false,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Wake),
        target_messages: Some(false),
        queue_input: Some(false),
        status: UserAgentControlStatus::Succeeded,
        error: Some("target input was admitted, but response handling was rolled back".to_string()),
    })
    .expect("control item should render");

    let rendered = cell
        .display_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    • User sent to Anscombe [reviewer] (ref 2) (wake)
      └ Review the latest diff.
        Warning: target input was admitted, but response handling was rolled back
    ");
}

#[test]
fn renders_successful_prompt_that_resumed_the_target() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "control-resumed-prompt".to_string(),
        action: UserAgentControlAction::Prompt,
        authored_selector: Some("2".to_string()),
        target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67269".to_string()),
        reply_recipient_thread_id: None,
        observer_thread_id: None,
        authored_observer_selector: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        task_path: None,
        task: None,
        task_path_mapping: Vec::new(),
        agent_ref: Some("2".to_string()),
        nickname: Some("Anscombe".to_string()),
        role: Some("reviewer".to_string()),
        model: None,
        reasoning_effort: None,
        prompt_preview: Some("Continue the review.".to_string()),
        resumed_target: true,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Presentation),
        target_messages: Some(false),
        queue_input: Some(false),
        status: UserAgentControlStatus::Succeeded,
        error: None,
    })
    .expect("control item should render");

    let rendered = cell
        .display_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    • User resumed and sent to Anscombe [reviewer] (ref 2) (presentation)
      └ Continue the review.
    ");
}

#[test]
fn renders_successful_queued_prompt_that_resumed_the_target() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "control-resumed-queued-prompt".to_string(),
        action: UserAgentControlAction::QueuedPrompt,
        authored_selector: Some("2".to_string()),
        target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67269".to_string()),
        reply_recipient_thread_id: None,
        observer_thread_id: None,
        authored_observer_selector: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        task_path: None,
        task: None,
        task_path_mapping: Vec::new(),
        agent_ref: Some("2".to_string()),
        nickname: Some("Anscombe".to_string()),
        role: Some("reviewer".to_string()),
        model: None,
        reasoning_effort: None,
        prompt_preview: Some("Run the queued review.".to_string()),
        resumed_target: true,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Presentation),
        target_messages: Some(false),
        queue_input: Some(true),
        status: UserAgentControlStatus::Succeeded,
        error: None,
    })
    .expect("control item should render");

    let rendered = cell
        .display_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    • User resumed and sent queued prompt to Anscombe [reviewer] (ref 2) (presentation · queued turn + reply)
      └ Run the queued review.
    ");
}

#[test]
fn renders_successful_close_with_queued_response_replay() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "control-close".to_string(),
        action: UserAgentControlAction::Close,
        authored_selector: Some("2".to_string()),
        target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67269".to_string()),
        reply_recipient_thread_id: None,
        observer_thread_id: None,
        authored_observer_selector: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        task_path: None,
        task: None,
        task_path_mapping: Vec::new(),
        agent_ref: Some("2".to_string()),
        nickname: Some("Anscombe".to_string()),
        role: Some("reviewer".to_string()),
        model: None,
        reasoning_effort: None,
        prompt_preview: None,
        resumed_target: false,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Passive),
        target_messages: Some(false),
        queue_input: Some(true),
        status: UserAgentControlStatus::Succeeded,
        error: None,
    })
    .expect("control item should render");

    let rendered = cell
        .display_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    • User closed Anscombe [reviewer] (ref 2) (passive · queued turn + reply)
    ");
}

#[test]
fn renders_user_reply_route_changes() {
    let render = |enabled| {
        let cell = new_user_agent_control(ThreadItem::UserAgentControl {
            id: format!("control-replies-{enabled}"),
            action: UserAgentControlAction::ReplyRoute,
            authored_selector: Some("2".to_string()),
            target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67269".to_string()),
            reply_recipient_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67270".to_string()),
            observer_thread_id: None,
            authored_observer_selector: None,
            previous_owner_session_id: None,
            new_owner_session_id: None,
            task_path: None,
            task: None,
            task_path_mapping: Vec::new(),
            agent_ref: Some("2".to_string()),
            nickname: Some("Anscombe".to_string()),
            role: Some("reviewer".to_string()),
            model: None,
            reasoning_effort: None,
            prompt_preview: None,
            resumed_target: false,
            fork_mode: None,
            observe_commentary: None,
            final_response: None,
            target_messages: Some(enabled),
            queue_input: None,
            status: UserAgentControlStatus::Succeeded,
            error: None,
        })
        .expect("control item should render")
        .with_direction_recipient_label(|_| Some("Franklin".to_string()));
        assert!(cell.raw_lines().iter().any(|line| {
            line.to_string()
                .contains("Recipient: 019ff050-d466-73b0-b133-72ecc7c67270")
        }));
        cell.display_lines(/*width*/ 80)
            .into_iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    };

    let enabled = render(true);
    let disabled = render(false);
    insta::assert_snapshot!(
        format!("enabled:\n{enabled}\n\ndisabled:\n{disabled}"),
        @r"
    enabled:
    • User enabled messages: Anscombe [reviewer] (ref 2) → Franklin

    disabled:
    • User disabled messages: Anscombe [reviewer] (ref 2) → Franklin
    "
    );
}

#[test]
fn renders_failed_user_agent_spawn() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "control-2".to_string(),
        action: UserAgentControlAction::Spawn,
        authored_selector: None,
        target_thread_id: None,
        reply_recipient_thread_id: None,
        observer_thread_id: None,
        authored_observer_selector: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        task_path: None,
        task: None,
        task_path_mapping: Vec::new(),
        agent_ref: None,
        nickname: None,
        role: Some("reviewer".to_string()),
        model: Some("gpt-5.6-luna".to_string()),
        reasoning_effort: Some(ReasoningEffort::High),
        prompt_preview: Some("Review the latest diff.".to_string()),
        resumed_target: false,
        fork_mode: Some(UserAgentForkMode::LastNTurns { turns: 3 }),
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Presentation),
        target_messages: Some(false),
        queue_input: Some(false),
        status: UserAgentControlStatus::Failed,
        error: Some("agent depth limit reached".to_string()),
    })
    .expect("control item should render");

    let rendered = cell
        .display_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    • User agent spawn failed [reviewer] (fork last 3) (gpt-5.6-luna high) (presentation)
      └ Review the latest diff.
        Failed: agent depth limit reached
    ");
}

#[test]
fn renders_explicit_adoption_and_preserves_owner_audit() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "control-3".to_string(),
        action: UserAgentControlAction::Resume,
        authored_selector: Some("019ff050-d466-73b0-b133-72ecc7c67269".to_string()),
        target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67269".to_string()),
        reply_recipient_thread_id: None,
        observer_thread_id: None,
        authored_observer_selector: None,
        previous_owner_session_id: None,
        new_owner_session_id: Some("019ff050-d466-73b0-b133-72ecc7c67270".to_string()),
        task_path: None,
        task: None,
        task_path_mapping: Vec::new(),
        agent_ref: Some("2".to_string()),
        nickname: Some("Noether".to_string()),
        role: None,
        model: None,
        reasoning_effort: None,
        prompt_preview: None,
        resumed_target: false,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Presentation),
        target_messages: Some(false),
        queue_input: Some(false),
        status: UserAgentControlStatus::Succeeded,
        error: None,
    })
    .expect("control item should render");

    let visible = cell
        .display_lines(/*width*/ 80)
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");
    let audit = cell
        .raw_lines()
        .into_iter()
        .map(|line| line.to_string())
        .collect::<Vec<_>>()
        .join("\n");

    insta::assert_snapshot!(format!("visible:\n{visible}\n\naudit:\n{audit}"), @r"
    visible:
    • User adopted Noether (ref 2) (presentation)

    audit:
    • User adopted Noether (ref 2) (presentation)
    Target: 019ff050-d466-73b0-b133-72ecc7c67269
    Selector: 019ff050-d466-73b0-b133-72ecc7c67269
    Ownership: unowned → 019ff050-d466-73b0-b133-72ecc7c67270
    ");
}

fn observation_control_item() -> ThreadItem {
    ThreadItem::UserAgentControl {
        id: "observe-main-from-peirce".to_string(),
        action: UserAgentControlAction::Observe,
        authored_selector: Some("Main".to_string()),
        target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67269".to_string()),
        observer_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67270".to_string()),
        authored_observer_selector: Some("Peirce".to_string()),
        reply_recipient_thread_id: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        agent_ref: None,
        nickname: None,
        role: None,
        task: None,
        task_path: None,
        task_path_mapping: Vec::new(),
        model: None,
        reasoning_effort: None,
        prompt_preview: None,
        resumed_target: false,
        fork_mode: None,
        observe_commentary: None,
        final_response: Some(AgentFinalResponseHandling::Passive),
        target_messages: None,
        queue_input: None,
        status: UserAgentControlStatus::Succeeded,
        error: None,
    }
}

#[test]
fn observation_audit_snapshot_shows_target_to_resolved_observer() {
    let cell = new_user_agent_control(observation_control_item())
        .expect("observation cell")
        .with_direction_recipient_label(|_| Some("Peirce".to_string()));
    let rendered = cell
        .display_lines(/*width*/ 160)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @"• User changed observation: Main (passive) → Peirce");
}

#[test]
fn failed_observer_resolution_snapshot_retains_authored_direction() {
    let mut item = observation_control_item();
    if let ThreadItem::UserAgentControl {
        observer_thread_id,
        authored_observer_selector,
        status,
        error,
        ..
    } = &mut item
    {
        *observer_thread_id = None;
        *authored_observer_selector = Some("nick:Missing".to_string());
        *status = UserAgentControlStatus::Failed;
        *error = Some("unknown observer".to_string());
    }
    let cell = new_user_agent_control(item).expect("failed observation cell");
    let rendered = cell
        .display_lines(/*width*/ 160)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @r"
    • User observation change failed: Main (passive) → nick:Missing
      └ Failed: unknown observer
    ");
}

#[test]
fn legacy_observation_audit_snapshot_does_not_invent_an_observer() {
    let mut value = serde_json::to_value(observation_control_item()).expect("serialize audit");
    let object = value.as_object_mut().expect("audit object");
    object.remove("observerThreadId");
    object.remove("authoredObserverSelector");
    let item = serde_json::from_value(value).expect("legacy audit decodes");
    let cell = new_user_agent_control(item).expect("legacy observation cell");
    let rendered = cell
        .display_lines(/*width*/ 160)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(rendered, @"• User changed observation for Main (passive)");
}

#[test]
fn observation_audit_preserves_prefixed_and_quoted_observer_tokens() {
    let rendered = ["ref:146", "nick:\"Charles Peirce\""]
        .into_iter()
        .map(|authored| {
            let mut item = observation_control_item();
            if let ThreadItem::UserAgentControl {
                observer_thread_id,
                authored_observer_selector,
                status,
                error,
                ..
            } = &mut item
            {
                *observer_thread_id = None;
                *authored_observer_selector = Some(authored.to_string());
                *status = UserAgentControlStatus::Failed;
                *error = Some("unknown observer".to_string());
            }
            let encoded = serde_json::to_value(item).expect("serialize audit");
            let decoded = serde_json::from_value(encoded).expect("decode audit");
            let cell = new_user_agent_control(decoded).expect("observer audit");
            assert!(
                cell.raw_lines()
                    .iter()
                    .any(|line| { line.to_string() == format!("Observer selector: {authored}") })
            );
            cell.display_lines(/*width*/ 160)
                .iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join("\n")
        })
        .collect::<Vec<_>>()
        .join("\n\n");
    insta::assert_snapshot!(rendered, @r#"
    • User observation change failed: Main (passive) → ref:146
      └ Failed: unknown observer

    • User observation change failed: Main (passive) → nick:"Charles Peirce"
      └ Failed: unknown observer
    "#);
}
