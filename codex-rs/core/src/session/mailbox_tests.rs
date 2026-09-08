use super::*;
use crate::session::tests::make_session_and_context;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::FunctionCallOutputPayload;
use codex_protocol::models::ResponseInputItem;
use codex_protocol::protocol::MultiAgentVersion;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_thread_store::AcceptMailboxInputParams;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::LiveThread;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn repaired_presentation_is_published_when_context_was_restored_before_retry()
-> anyhow::Result<()> {
    let (mut session, turn) = make_session_and_context().await;
    let store: Arc<dyn ThreadStore> = Arc::new(InMemoryThreadStore::default());
    let live = LiveThread::create(
        Arc::clone(&store),
        CreateThreadParams {
            session_id: session.session_id(),
            thread_id: session.thread_id,
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "mailbox-test".to_string(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: Some(MultiAgentVersion::V1),
            history_mode: ThreadHistoryMode::Legacy,
            subagent_history_start_ordinal: None,
            history_base: None,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            metadata: ThreadPersistenceMetadata {
                cwd: None,
                model_provider: "test".to_string(),
                memory_mode: ThreadMemoryMode::Disabled,
            },
        },
    )
    .await?;
    session.services.thread_store = Arc::clone(&store);
    session.services.live_thread = Some(live);
    let (tx, rx) = async_channel::unbounded();
    session.tx_event = tx;
    let input = vec![UserInput::Text {
        text: "Restored mailbox context.".to_string(),
        text_elements: Vec::new(),
    }];
    let accepted = store
        .accept_mailbox_input(AcceptMailboxInputParams {
            receiver_thread_id: session.thread_id,
            submission_key: "restored-context".to_string(),
            payload: MailboxPayload::User {
                input: input.clone(),
                client_id: Some("original-client".to_string()),
            },
        })
        .await?;
    let params = ClaimMailboxInputParams {
        invocation: MailboxInvocation {
            receiver_thread_id: session.thread_id,
            turn_id: turn.sub_id.clone(),
            tool_call_id: "repair-presentation".to_string(),
        },
        selection: MailboxSelection::All,
    };
    let claim = store.claim_mailbox_input(params.clone()).await?;
    let id = codex_protocol::mailbox_delivery_response_item_id(&claim.messages[0].delivery_id)
        .expect("reserved mailbox ID");
    let mut result = ResponseItem::from(ResponseInputItem::FunctionCallOutput {
        call_id: params.invocation.tool_call_id.clone(),
        output: FunctionCallOutputPayload::from_text(
            r#"{"status":"delivery_requested","from":null}"#.to_string(),
        ),
    });
    result.set_id(Some(ResponseItemId::new("fco")));
    Session::stamp_response_item_for_history(&mut result, &turn.sub_id);
    let result = ResponseItemEnvelope::new(result);
    let mut context = session.response_item_from_user_input(input.clone());
    context.set_id(Some(id.clone()));
    Session::stamp_response_item_for_history(&mut context, &turn.sub_id);
    let context = ResponseItemEnvelope::new(context);
    session
        .live_thread()
        .expect("attached live thread")
        .append_items_and_flush_canonical(&[
            RolloutItem::ResponseItem(result.clone()),
            RolloutItem::ResponseItem(context.clone()),
        ])
        .await?;
    session.state.lock().await.history.record_annotated_items(
        &[result.clone(), context.clone()],
        turn.model_info().truncation_policy.into(),
    );
    let session = Arc::new(session);
    let turn = Arc::new(turn);
    let operation = MailboxConsumption {
        tool_call_id: params.invocation.tool_call_id.clone(),
        selection: MailboxSelection::All,
    };
    session
        .commit_mailbox_consumption(Arc::clone(&turn), result.clone(), operation.clone())
        .await?;
    let mut presentations = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let EventMsg::ItemCompleted(event) = event.msg {
            presentations.push(event.item);
        }
    }
    assert_eq!(
        serde_json::to_value(presentations)?,
        serde_json::to_value(vec![TurnItem::UserMessage(UserMessageItem {
            id: id.to_string(),
            client_id: Some("original-client".to_string()),
            content: input,
        })])?,
    );
    assert_eq!(
        session.clone_history().await.into_annotated_items(),
        vec![result.clone(), context.clone()],
    );
    let reconciled = store.claim_mailbox_input(params).await?;
    assert_eq!(
        reconciled
            .messages
            .iter()
            .map(|member| (&member.message.id, member.message.state))
            .collect::<Vec<_>>(),
        vec![(&accepted.id, MailboxMessageState::Consumed)],
    );
    session
        .commit_mailbox_consumption(Arc::clone(&turn), result.clone(), operation)
        .await?;
    assert!(
        rx.try_recv().is_err(),
        "terminal retry must not republish historical mail"
    );
    assert_eq!(
        session.clone_history().await.into_annotated_items(),
        vec![result, context],
    );
    let history = session
        .live_thread()
        .expect("attached live thread")
        .load_rollback_history(/*include_archived*/ false)
        .await?;
    assert_eq!(history.items.iter().filter(|item| {
        matches!(item, RolloutItem::EventMsg(EventMsg::ItemCompleted(event)) if event.item.id() == id.as_str())
    }).count(), 1);
    Ok(())
}
