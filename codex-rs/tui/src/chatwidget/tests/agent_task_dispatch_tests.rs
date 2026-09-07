use super::*;

#[tokio::test]
async fn nested_spawn_keeps_issuing_thread_label_and_all_spawn_controls() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let issuing_thread = ThreadId::new();
    chat.thread_id = Some(issuing_thread);
    chat.config.agent_roles.insert(
        "reviewer".to_string(),
        crate::legacy_core::config::AgentRoleConfig {
            description: Some("Review changes".to_string()),
            config_file: None,
            nickname_candidates: None,
        },
    );
    for selector in ["new", "role:reviewer"] {
        chat.dispatch_command_with_args(
            SlashCommand::Agent,
            format!(
                "{selector} task:review model:custom effort:high fork:all w:cmx review the API"
            ),
            Vec::new(),
        );
        let actual = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|event| match event {
            AppEvent::SpawnAgent {
                source_thread_id,
                task,
                role,
                model,
                reasoning_effort,
                prompt,
                fork_mode,
                response_handling,
                ..
            } => Some((
                source_thread_id,
                task,
                role,
                model,
                reasoning_effort,
                prompt.map(|message| message.text),
                fork_mode,
                response_handling,
            )),
            _ => None,
        });
        assert_eq!(
            actual,
            Some((
                issuing_thread,
                Some("review".to_string()),
                (selector == "role:reviewer").then(|| "reviewer".to_string()),
                Some("custom".to_string()),
                Some(ReasoningEffortConfig::High),
                Some("review the API".to_string()),
                codex_app_server_protocol::AgentForkMode::All,
                Some(codex_app_server_protocol::AgentResponseHandling::new(
                    /*commentary*/ true,
                    codex_app_server_protocol::AgentFinalResponseHandling::Presentation,
                    /*target_messages*/ true,
                    /*queue_input*/ false,
                )),
            )),
        );
    }
}

#[tokio::test]
async fn target_first_adoption_dispatch_preserves_authored_relative_label() {
    let (mut chat, mut rx, _op_rx) = make_chatwidget_manual(/*model_override*/ None).await;
    let issuing_thread = ThreadId::new();
    let imported_thread = ThreadId::new();
    chat.thread_id = Some(issuing_thread);
    chat.dispatch_command_with_args(
        SlashCommand::Agent,
        format!("{imported_thread} resume task:imported w:x"),
        Vec::new(),
    );
    let actual = std::iter::from_fn(|| rx.try_recv().ok()).find_map(|event| match event {
        AppEvent::ResumeAgent {
            source_thread_id,
            selector,
            task,
            response_handling,
            prompt,
        } => Some((
            source_thread_id,
            selector.control_target(),
            task,
            response_handling,
            prompt,
        )),
        _ => None,
    });
    assert_eq!(
        actual,
        Some((
            issuing_thread,
            Ok(imported_thread.to_string()),
            Some("imported".to_string()),
            Some(codex_app_server_protocol::AgentResponseHandling::Presentation),
            None,
        )),
    );
}
