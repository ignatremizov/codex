use super::*;
use codex_core::UserAgentFinalResponseHandling;
use pretty_assertions::assert_eq;
use test_case::test_case;

#[test_case(ThreadHistoryMode::Legacy; "legacy")]
#[test_case(ThreadHistoryMode::Paginated; "paginated")]
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn accepted_human_queue_survives_the_active_child_turn(
    history_mode: ThreadHistoryMode,
) -> Result<()> {
    skip_if_no_network!(Ok(()));
    const QUEUED_PROMPT: &str = "queued task accepted while the child is active";
    let (server, _) = start_streaming_sse_server(Vec::new()).await;
    let (child_gate_tx, child_gate_rx) = oneshot::channel();
    let (test, child_id) = setup_turn_one_with_custom_streamed_child(
        &server,
        json!({"message": CHILD_PROMPT, "w": "x"}),
        child_gate_rx,
        |builder| builder.with_history_mode(history_mode),
    )
    .await?;
    let source_status = test.codex.agent_status().await;
    let queued = test
        .codex
        .queue_agent_prompt(
            &child_id,
            vec![UserInput::Text {
                text: QUEUED_PROMPT.to_string(),
                text_elements: Vec::new(),
            }],
            UserAgentResponseHandling::from_parts(
                /*commentary*/ false,
                UserAgentFinalResponseHandling::Presentation,
                /*queue_input*/ true,
            ),
        )
        .await?;
    assert!(queued.queued);
    let entries = test.codex.list_user_agent_queued_turns()?;
    assert_eq!(entries.len(), 1);
    let mut queued_response = server
        .mount_response(
            |request| request.body_contains_text(QUEUED_PROMPT),
            vec![StreamingSseChunk {
                gate: None,
                body: sse(vec![
                    ev_response_created("accepted-human-queue"),
                    ev_assistant_message("accepted-human-queue-done", "queued task done"),
                    ev_completed("accepted-human-queue"),
                ]),
            }],
        )
        .await;
    let requests_before = server.requests().await.len();
    child_gate_tx
        .send(())
        .map_err(|_| anyhow::anyhow!("child response gate closed"))?;
    let request = wait_for_streaming_request(&mut queued_response).await?;
    assert!(request.body_contains_text(QUEUED_PROMPT));
    let child = test
        .thread_manager
        .get_thread(ThreadId::from_string(&child_id)?)
        .await?;
    wait_for_event_match(child.as_ref(), |event| match event {
        EventMsg::ItemCompleted(completed) => match &completed.item {
            TurnItem::UserMessage(item)
                if item.content.iter().any(
                    |input| matches!(input, UserInput::Text { text, .. } if text == QUEUED_PROMPT),
                ) =>
            {
                Some(())
            }
            _ => None,
        },
        _ => None,
    })
    .await;
    assert_eq!(server.requests().await.len(), requests_before + 1);
    assert_eq!(test.codex.agent_status().await, source_status);
    Ok(())
}
