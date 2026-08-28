//! Explicit context boundaries must not create a late-input completion loop or wake passive mail.

use super::regular::RegularTaskOutput;
use super::tests::PendingTask;
use crate::context::ContextualUserFragment;
use crate::context::McpServerUseInstructions;
use crate::session::TurnInput;
use crate::session::tests::make_session_and_context_with_rx;
use codex_history::ResponseItemEnvelope;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::EventMsg;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;
use std::sync::Arc;
use std::time::Duration;

#[test_case::test_case(false; "context_and_passive_mail")]
#[test_case::test_case(true; "eligible_input_before_context")]
#[tokio::test]
async fn finish_continues_only_for_input_before_mcp_boundary(earlier_user: bool) {
    let (session, turn, receiver) = make_session_and_context_with_rx().await;
    session
        .spawn_task(Arc::clone(&turn), Vec::new(), PendingTask)
        .await;
    while receiver.try_recv().is_ok() {}
    let (state, accepting) = {
        let active = session.active_turn.lock().await;
        let active = active.as_ref().expect("pending task registered");
        (
            Arc::clone(&active.turn_state),
            Arc::clone(
                &active
                    .task
                    .as_ref()
                    .expect("running task")
                    .accepting_pending_input,
            ),
        )
    };
    accepting.store(false, std::sync::atomic::Ordering::Release);
    let boundary =
        TurnInput::ResponseItem(ResponseItemEnvelope::new(ContextualUserFragment::into(
            McpServerUseInstructions::new("docs".to_string(), "[]".to_string()),
        )));
    let passive_mail =
        TurnInput::ResponseItem(ResponseItemEnvelope::new(ResponseItem::AgentMessage {
            id: None,
            author: "/root/worker".to_string(),
            recipient: "/root".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: "passive report after activation".to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        }));
    let mut input = vec![boundary.clone(), passive_mail.clone()];
    if earlier_user {
        input.insert(
            0,
            TurnInput::UserInput {
                content: vec![UserInput::Text {
                    text: "eligible earlier steer".into(),
                    text_elements: Vec::new(),
                }],
                client_id: Some("earlier-client".into()),
                metadata: Default::default(),
            },
        );
    }
    session
        .input_queue
        .extend_pending_input_for_turn_state(&state, input.clone())
        .await;
    let outcome = || {
        Ok(Some(RegularTaskOutput {
            last_agent_message: Some("finished before activation".into()),
            mcp_startup_requirements: Default::default(),
        }))
    };
    let continuation = tokio::time::timeout(
        Duration::from_secs(2),
        session.on_task_finished(Arc::clone(&turn), outcome()),
    )
    .await
    .expect("finishing must not wait on a context-only boundary");
    assert_eq!(continuation.is_some(), earlier_user);
    assert_eq!(
        accepting.load(std::sync::atomic::Ordering::Acquire),
        earlier_user
    );
    if earlier_user {
        assert!(session.active_turn.lock().await.is_some());
        assert_eq!(
            state.lock().await.pending_input.as_slice(),
            input.as_slice()
        );
        // The caller resumes sampling for that earlier input, not the later context or mail.
        assert_eq!(
            session
                .input_queue
                .get_pending_input(&session.active_turn)
                .await
                .0,
            vec![input[0].clone()]
        );
        assert!(
            tokio::time::timeout(
                Duration::from_secs(2),
                session.on_task_finished(Arc::clone(&turn), outcome()),
            )
            .await
            .expect("the boundary must then allow completion")
            .is_none()
        );
    }
    assert!(session.active_turn.lock().await.is_none());
    assert_eq!(
        session.input_queue.queued_turn_inputs().await,
        vec![passive_mail]
    );
    assert!(!session.input_queue.has_queued_turn_trigger().await);
    let history = session.clone_history().await;
    assert_eq!(
        history
            .raw_items()
            .filter(|item| { McpServerUseInstructions::matches_response_item(item) })
            .count(),
        1
    );
    let completed = std::iter::from_fn(|| receiver.try_recv().ok())
        .filter_map(|event| match event.msg {
            EventMsg::TurnComplete(completed) => Some(completed.turn_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(completed, vec![turn.sub_id.clone()]);
}
