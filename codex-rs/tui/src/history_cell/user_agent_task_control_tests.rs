use super::*;
use codex_app_server_protocol::AgentTaskPathMapping;

#[test]
fn adoption_snapshot_shows_resolved_path_and_unlabeled_mapping() {
    let cell = new_user_agent_control(ThreadItem::UserAgentControl {
        id: "adoption".to_string(),
        action: UserAgentControlAction::Resume,
        authored_selector: Some("id:imported".to_string()),
        target_thread_id: Some("imported".to_string()),
        reply_recipient_thread_id: None,
        previous_owner_session_id: Some("old-owner".to_string()),
        new_owner_session_id: Some("new-owner".to_string()),
        agent_ref: Some("3".to_string()),
        nickname: Some("Pascal".to_string()),
        role: Some("coder".to_string()),
        task: Some("backend".to_string()),
        task_path: Some("/root/backend".to_string()),
        task_path_mapping: vec![AgentTaskPathMapping {
            thread_id: "imported".to_string(),
            previous_task_path: None,
            task_path: Some("/root/backend".to_string()),
        }],
        model: None,
        reasoning_effort: None,
        prompt_preview: None,
        resumed_target: false,
        fork_mode: None,
        observe_commentary: None,
        final_response: None,
        target_messages: None,
        queue_input: None,
        status: UserAgentControlStatus::Succeeded,
        error: None,
    })
    .expect("adoption audit");
    let visible = cell
        .display_lines(/*width*/ 120)
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(visible, @r"
    • User adopted Pascal [coder] /root/backend (ref 3)
      └ Task path: (unlabeled) → /root/backend
    ");
    let audit = cell
        .raw_lines()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join("\n");
    insta::assert_snapshot!(audit, @r"
    • User adopted Pascal [coder] /root/backend (ref 3)
    Task path: (unlabeled) → /root/backend
    Requested task: backend
    Task path: (unlabeled) → /root/backend (imported)
    Target: imported
    Selector: id:imported
    Ownership: old-owner → new-owner
    ");
}
