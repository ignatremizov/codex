use super::*;
use crate::session::tests::make_session_and_context;
use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
use crate::tools::handlers::McpHandler;
use crate::tools::registry::ToolExecutor;
use codex_features::Feature;
use codex_history::CodexHarnessMetadata;
use codex_login::CodexAuth;
use codex_protocol::AgentPath;
use codex_protocol::protocol::InterAgentCommunication;
use codex_protocol::turn_input::TurnStartOptions;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use serde_json::json;
use tokio_util::sync::CancellationToken;

fn inventory(server: &str, body: &str) -> ResponseItemEnvelope {
    ResponseItemEnvelope::new(ContextualUserFragment::into(McpServerUseInstructions::new(
        server.to_string(),
        body.to_string(),
    )))
}

fn tool(server: &str, name: &str) -> ToolInfo {
    serde_json::from_value(json!({
        "server_name": server,
        "tool_name": name,
        "tool_namespace": format!("mcp__{server}"),
        "tool": {"name": name, "description": "complete schema", "inputSchema": {"type": "object"}},
        "connector_id": null,
        "connector_name": null
    }))
    .expect("tool fixture")
}

#[test]
fn first_advertisement_freezes_only_non_apps_direct_inventory() {
    let state = McpPromptState::new(HashMap::new());
    let direct = tool("docs", "read");
    let deferred = tool("other", "search");
    let app = tool(CODEX_APPS_MCP_SERVER_NAME, "app");
    let specs = [&direct, &app]
        .into_iter()
        .map(|tool| McpHandler::new(tool.clone()).expect("MCP handler").spec())
        .collect::<Vec<_>>();
    // Planning the actual declarations does not publish the session contract.
    assert_eq!(*state.direct_tools.lock().expect("prompt state"), None);
    state.freeze(&[direct.clone(), deferred, app], &specs);
    state.freeze(&[tool("docs", "replacement")], &[]);
    assert_eq!(
        *state.direct_tools.lock().expect("prompt state"),
        Some(HashMap::from([(
            direct.canonical_tool_name().to_string(),
            direct
        )])),
    );
}

#[test]
fn explicit_inventory_preserves_full_text_and_server_identity() {
    let server = "docs ` </server_name_b64> with spaces";
    let body = serde_json::to_string_pretty(&json!({
        "schema": "complete-schema ".repeat(10_000)
    }))
    .expect("inventory JSON");
    let item = inventory(server, &body);
    let text = use_text(&item.item, server).expect("recognized inventory");
    assert!(text.contains(&body));
    assert_eq!(
        McpServerUseInstructions::parse_server_name(&text).as_deref(),
        Some(server)
    );
    assert!(McpServerUseInstructions::matches_response_item(&item.item));
}

#[tokio::test]
async fn active_boundary_preserves_annotated_input_order_without_follow_up() {
    let option_fields = |options: TurnStartOptions| {
        (
            options.turn_trigger,
            options.final_output_json_schema,
            options.service_tier,
            options.parent_turn_id,
            options.root_turn_id,
            options.cyber_access_program,
        )
    };
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    let active = ActiveTurn::default();
    let state = Arc::clone(&active.turn_state);
    *session.active_turn.lock().await = Some(active);
    let before = TurnInput::UserInput {
        content: vec![UserInput::Text {
            text: "before".to_string(),
            text_elements: Vec::new(),
        }],
        client_id: None,
        acceptance_order: Some(7),
    };
    let mut explicit = inventory("docs", "[]");
    explicit.metadata = Some(CodexHarnessMetadata {
        client_authored: true,
        ..Default::default()
    });
    let after = TurnInput::FunctionCallOutput(ResponseItemEnvelope {
        item: ResponseItem::FunctionCallOutput {
            id: None,
            call_id: Some("call".to_string()),
            name: None,
            namespace: None,
            output: codex_protocol::models::FunctionCallOutputPayload::from_text(
                "result".to_string(),
            ),
            internal_chat_message_metadata_passthrough: None,
        },
        metadata: Some(CodexHarnessMetadata {
            user_input_order: Some(9),
            ..Default::default()
        }),
    });
    session
        .input_queue
        .extend_pending_input_for_turn_state(
            &state,
            vec![
                before.clone(),
                TurnInput::ResponseItem(explicit.clone()),
                after.clone(),
            ],
        )
        .await;
    let mail = InterAgentCommunication::new(
        AgentPath::root(),
        AgentPath::root(),
        Vec::new(),
        "following mail".to_string(),
        /*trigger_turn*/ true,
    );
    let mail_options = TurnStartOptions {
        turn_trigger: Some("following-mail".to_string()),
        final_output_json_schema: Some(json!({"type": "object"})),
        service_tier: Some("priority".to_string()),
        parent_turn_id: Some("parent-turn".to_string()),
        root_turn_id: Some("root-turn".to_string()),
        ..Default::default()
    };
    session
        .input_queue
        .enqueue_mailbox_communication(mail.clone(), mail_options.clone())
        .await;
    let (accepted, options) = session
        .input_queue
        .get_pending_input(&session.active_turn)
        .await;
    assert_eq!(
        (accepted, option_fields(options)),
        (vec![before], option_fields(TurnStartOptions::default()))
    );
    assert!(
        !session
            .input_queue
            .has_pending_input(&session.active_turn)
            .await
    );
    assert_eq!(
        session
            .input_queue
            .take_pending_input_for_turn_state(&state)
            .await,
        vec![TurnInput::ResponseItem(explicit), after],
    );
    let (mailbox, options) = session.input_queue.drain_mailbox_input_items().await;
    assert_eq!(
        (mailbox, option_fields(options)),
        (
            vec![TurnInput::InterAgentCommunication(mail)],
            option_fields(mail_options)
        )
    );
    *session.active_turn.lock().await = None;
    session
        .input_queue
        .enqueue_mailbox_communication(
            InterAgentCommunication::new(
                AgentPath::root(),
                AgentPath::root(),
                Vec::new(),
                "queue only".to_string(),
                /*trigger_turn*/ false,
            ),
            TurnStartOptions::default(),
        )
        .await;
    session
        .maybe_start_turn_for_pending_work_with_sub_id("must-stay-idle".to_string())
        .await;
    assert!(session.active_turn.lock().await.is_none());
    assert!(session.input_queue.has_pending_mailbox_items().await);
}

#[tokio::test]
async fn abort_preserves_complete_explicit_envelopes_before_queue_clear() {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    let active = ActiveTurn::default();
    let mut item = inventory("docs", &"full ".repeat(12_000));
    item.metadata = Some(CodexHarnessMetadata {
        client_authored: true,
        ..Default::default()
    });
    let expected = vec![item.clone()];
    session
        .input_queue
        .extend_pending_input_for_turn_state(
            &active.turn_state,
            vec![TurnInput::ResponseItem(item)],
        )
        .await;
    session.record_active_mcp_use_before_abort(&active).await;
    let stored = session.clone_history().await.annotated_items().to_vec();
    session.input_queue.clear_pending(&active).await;
    assert_eq!(
        session.clone_history().await.annotated_items(),
        stored.as_slice()
    );
    assert_eq!(stored, expected);
    assert!(session.active_turn.lock().await.is_none());
    // The recorded block is not work that can awaken an otherwise idle thread.
    assert!(!session.input_queue.has_queued_turn_trigger().await);
}

#[tokio::test]
async fn pre_first_turn_activation_is_queued_once_without_creating_a_turn() {
    let (session, _) = make_session_and_context().await;
    let session = Arc::new(session);
    session.activate_mcp_server("unavailable".to_string()).await;
    session.activate_mcp_server("unavailable".to_string()).await;
    assert_eq!(
        *session.mcp_prompt.first_turn_servers.lock().await,
        vec!["unavailable"]
    );
    assert!(session.active_turn.lock().await.is_none());
    assert!(!session.input_queue.has_queued_turn_trigger().await);
    assert_eq!(session.clone_history().await.raw_items().count(), 0);
}

#[tokio::test]
async fn cancellation_while_recording_keeps_first_turn_activation_for_retry() {
    let (session, turn) = make_session_and_context().await;
    let session = Arc::new(session);
    let step = StepContext::for_test(Arc::new(turn));
    session.activate_mcp_server("unavailable".to_string()).await;
    let history_lock = session.state.lock().await;
    let mut recording = Box::pin(session.record_queued_mcp_use(&step));
    // Poll to the blocked history lock; this is a deterministic cancellation
    // boundary, not a delay that assumes the task has begun running.
    assert!(futures::poll!(recording.as_mut()).is_pending());
    drop(recording);
    assert_eq!(
        *session.mcp_prompt.first_turn_servers.lock().await,
        vec!["unavailable"]
    );
    drop(history_lock);
    session.record_queued_mcp_use(&step).await;
    assert!(
        session
            .mcp_prompt
            .first_turn_servers
            .lock()
            .await
            .is_empty()
    );
    assert_eq!(
        session
            .clone_history()
            .await
            .raw_items()
            .filter(|item| { McpServerUseInstructions::matches_response_item(item) })
            .count(),
        1
    );
    assert!(session.active_turn.lock().await.is_none());
    assert!(!session.input_queue.has_queued_turn_trigger().await);
}

#[tokio::test]
async fn new_context_window_retains_each_explicit_envelope_once_with_either_generic_policy() {
    for retain_generic in [false, true] {
        let (session, turn, _events) = make_session_and_context_with_auth_and_config_and_rx(
            CodexAuth::from_api_key("test"),
            Vec::new(),
            move |config| {
                if retain_generic {
                    config
                        .features
                        .enable(Feature::RetainClientDeveloperMessages)
                        .expect("feature");
                } else {
                    config
                        .features
                        .disable(Feature::RetainClientDeveloperMessages)
                        .expect("feature");
                }
            },
        )
        .await;
        let first = inventory("docs", &"first ".repeat(10_000));
        let mut second = inventory("docs", &"second ".repeat(10_000));
        second.metadata = Some(CodexHarnessMetadata {
            client_authored: true,
            ..Default::default()
        });
        session
            .record_mcp_use_items(turn.model_info(), vec![first, second])
            .await;
        let expected = session.clone_history().await.annotated_items().to_vec();
        let step = session
            .capture_step_context(Arc::clone(&turn), &CancellationToken::new())
            .await
            .expect("step");
        let world = Arc::new(
            session
                .build_world_state_for_step(&step)
                .await
                .expect("world state"),
        );
        session.start_new_context_window(&step, world).await;
        let actual = session
            .clone_history()
            .await
            .annotated_items()
            .iter()
            .filter(|item| McpServerUseInstructions::matches_response_item(&item.item))
            .cloned()
            .collect::<Vec<_>>();
        assert_eq!(actual, expected);
    }
}
