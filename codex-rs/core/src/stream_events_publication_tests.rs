//! Tool dispatch requires an acknowledged canonical call, including ambiguous writer failures.

use super::output_context;
use crate::session::tests::make_session_and_context_with_auth_and_config_and_rx;
use crate::stream_events_utils::handle_output_item_done;
use codex_history::RolloutItem;
use codex_login::CodexAuth;
use codex_protocol::error::CodexErrorDetails;
use codex_protocol::models::BaseInstructions;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::protocol::SessionSource;
use codex_protocol::protocol::ThreadHistoryMode;
use codex_protocol::protocol::ThreadMemoryMode;
use codex_thread_store::CreateThreadParams;
use codex_thread_store::InMemoryThreadStore;
use codex_thread_store::InMemoryThreadStoreFailure;
use codex_thread_store::LiveThread;
use codex_thread_store::LoadThreadHistoryParams;
use codex_thread_store::ThreadPersistenceMetadata;
use codex_thread_store::ThreadStore;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use test_case::test_case;

#[derive(Clone, Copy, PartialEq, Eq)]
enum CallPublication {
    Acknowledged,
    RejectedBeforeCommit,
    LostAcknowledgement,
}

#[test_case(CallPublication::Acknowledged; "dispatch after canonical receipt")]
#[test_case(CallPublication::RejectedBeforeCommit; "no tool effect after failed append")]
#[test_case(CallPublication::LostAcknowledgement; "no tool effect after uncertain commit")]
#[tokio::test]
async fn tool_dispatch_requires_a_successful_call_publication(
    publication: CallPublication,
) -> anyhow::Result<()> {
    let (mut session, turn, events) = make_session_and_context_with_auth_and_config_and_rx(
        CodexAuth::from_api_key("Test API Key"),
        Vec::new(),
        |config| config.update_plan_enabled = true,
    )
    .await;
    let store = Arc::new(InMemoryThreadStore::default());
    let config = session.get_config().await;
    let live_thread = LiveThread::create(
        store.clone(),
        CreateThreadParams {
            creator_user_id: None,
            creator_account_id: None,
            session_id: session.session_id(),
            thread_id: session.thread_id(),
            extra_config: None,
            forked_from_id: None,
            parent_thread_id: None,
            source: SessionSource::Exec,
            thread_source: None,
            originator: "call-publication-test".into(),
            base_instructions: BaseInstructions::default(),
            dynamic_tools: Vec::new(),
            selected_capability_roots: Vec::new(),
            multi_agent_version: None,
            history_mode: ThreadHistoryMode::Legacy,
            subagent_history_start_ordinal: None,
            history_base: None,
            initial_window_id: uuid::Uuid::now_v7().to_string(),
            runtime_workspace_roots: None,
            metadata: ThreadPersistenceMetadata {
                cwd: Some(config.cwd.to_path_buf()),
                model_provider: config.model_provider_id.clone(),
                memory_mode: ThreadMemoryMode::Disabled,
            },
        },
    )
    .await?;
    Arc::get_mut(&mut session)
        .ok_or_else(|| anyhow::anyhow!("fixture session must be uniquely owned"))?
        .services
        .live_thread = Some(live_thread);
    match publication {
        CallPublication::Acknowledged => {}
        CallPublication::RejectedBeforeCommit => {
            store
                .fail_next_operation(InMemoryThreadStoreFailure::HistoryAppend)
                .await;
        }
        CallPublication::LostAcknowledgement => {
            store
                .fail_next_operation(InMemoryThreadStoreFailure::HistoryFlush)
                .await;
        }
    }
    let mut context = output_context(Arc::clone(&session), turn);
    let call = serde_json::from_value::<ResponseItem>(serde_json::json!({
        "type": "function_call",
        "call_id": "plan-effect",
        "name": "update_plan",
        "arguments": "{\"plan\":[{\"step\":\"observable effect\",\"status\":\"in_progress\"}]}",
    }))?;
    let result = handle_output_item_done(&mut context, call, /*previously_active_item*/ None).await;
    let expected_effect = publication == CallPublication::Acknowledged;
    if expected_effect {
        result?
            .tool_future
            .ok_or_else(|| anyhow::anyhow!("acknowledged call must dispatch"))?
            .await?;
    } else {
        let Err(error) = result else {
            anyhow::bail!("unacknowledged call produced an executable tool future");
        };
        assert!(matches!(error.details(), CodexErrorDetails::Fatal(_)));
        assert!(session.check_history_publication().is_err());
    }
    let mut effects = 0;
    while let Ok(event) = events.try_recv() {
        if matches!(event.msg, EventMsg::PlanUpdate(_)) {
            effects += 1;
        }
    }
    assert_eq!(effects, usize::from(expected_effect));
    let history = store
        .load_history(LoadThreadHistoryParams {
            thread_id: session.thread_id(),
            include_archived: false,
        })
        .await?;
    let recorded_calls = history
        .items
        .iter()
        .filter(|item| {
            matches!(item, RolloutItem::ResponseItem(envelope)
            if matches!(&envelope.item, ResponseItem::FunctionCall { call_id, .. }
                if call_id == "plan-effect"))
        })
        .count();
    assert_eq!(
        recorded_calls,
        usize::from(publication != CallPublication::RejectedBeforeCommit),
    );
    Ok(())
}
