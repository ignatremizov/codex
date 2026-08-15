use codex_app_server_protocol::AgentFinalResponseHandling;
use codex_app_server_protocol::ThreadItem;
use codex_app_server_protocol::UserAgentControlAction;
use codex_app_server_protocol::UserAgentControlStatus;
use codex_app_server_protocol::UserAgentForkMode;

use super::*;

#[test]
fn renders_successful_user_agent_prompt() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "control-1".to_string(),
        action: UserAgentControlAction::Prompt,
        authored_selector: Some("2".to_string()),
        target_thread_id: Some("019ff050-d466-73b0-b133-72ecc7c67269".to_string()),
        previous_owner_session_id: None,
        new_owner_session_id: None,
        agent_ref: Some("2".to_string()),
        nickname: Some("Anscombe".to_string()),
        role: Some("reviewer".to_string()),
        prompt_preview: Some("Review the latest diff.".to_string()),
        resumed_target: false,
        fork_mode: None,
        observe_commentary: Some(true),
        final_response: Some(AgentFinalResponseHandling::Wake),
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
    • User sent to Anscombe [reviewer] (ref 2) (commentary · wake)
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
        previous_owner_session_id: None,
        new_owner_session_id: None,
        agent_ref: Some("1".to_string()),
        nickname: Some(codex_protocol::MAIN_AGENT_NICKNAME.to_string()),
        role: None,
        prompt_preview: Some("Please confirm.".to_string()),
        resumed_target: false,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Presentation),
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
        previous_owner_session_id: None,
        new_owner_session_id: None,
        agent_ref: Some("2".to_string()),
        nickname: Some("Anscombe".to_string()),
        role: Some("reviewer".to_string()),
        prompt_preview: Some("Review the latest diff.".to_string()),
        resumed_target: false,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Wake),
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
        previous_owner_session_id: None,
        new_owner_session_id: None,
        agent_ref: Some("2".to_string()),
        nickname: Some("Anscombe".to_string()),
        role: Some("reviewer".to_string()),
        prompt_preview: Some("Continue the review.".to_string()),
        resumed_target: true,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Presentation),
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
        previous_owner_session_id: None,
        new_owner_session_id: None,
        agent_ref: Some("2".to_string()),
        nickname: Some("Anscombe".to_string()),
        role: Some("reviewer".to_string()),
        prompt_preview: Some("Run the queued review.".to_string()),
        resumed_target: true,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Presentation),
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
    • User resumed and sent queued prompt to Anscombe [reviewer] (ref 2) (presentation)
      └ Run the queued review.
    ");
}

#[test]
fn renders_failed_user_agent_spawn() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "control-2".to_string(),
        action: UserAgentControlAction::Spawn,
        authored_selector: None,
        target_thread_id: None,
        previous_owner_session_id: None,
        new_owner_session_id: None,
        agent_ref: None,
        nickname: None,
        role: Some("reviewer".to_string()),
        prompt_preview: Some("Review the latest diff.".to_string()),
        resumed_target: false,
        fork_mode: Some(UserAgentForkMode::LastNTurns { turns: 3 }),
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Presentation),
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
    • User agent spawn failed [reviewer] (fork last 3) (presentation)
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
        previous_owner_session_id: None,
        new_owner_session_id: Some("019ff050-d466-73b0-b133-72ecc7c67270".to_string()),
        agent_ref: Some("2".to_string()),
        nickname: Some("Noether".to_string()),
        role: None,
        prompt_preview: None,
        resumed_target: false,
        fork_mode: None,
        observe_commentary: Some(false),
        final_response: Some(AgentFinalResponseHandling::Presentation),
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
    • User adopted Noether [default] (ref 2) (presentation)

    audit:
    • User adopted Noether [default] (ref 2) (presentation)
    Target: 019ff050-d466-73b0-b133-72ecc7c67269
    Selector: 019ff050-d466-73b0-b133-72ecc7c67269
    Ownership: unowned → 019ff050-d466-73b0-b133-72ecc7c67270
    ");
}
