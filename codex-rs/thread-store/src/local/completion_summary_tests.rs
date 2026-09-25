use codex_protocol::items::CollabAgentTool;
use codex_protocol::items::CollabAgentToolCallItem;
use codex_protocol::items::CollabAgentToolCallStatus;
use codex_protocol::protocol::AgentStatus;
use codex_protocol::protocol::ThreadRolledBackEvent;
use codex_protocol::protocol::sub_agent_completion_item;

use super::*;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn cold_summary_keeps_completion_only_and_owned_orphan_wait_rows() {
    for exact_rollback in [false, true] {
        let home = TempDir::new().expect("temp dir");
        let store = projection_store(home.path()).await;
        let thread_id = ThreadId::new();
        let worker_id = ThreadId::new();
        create_paginated_thread(&store, thread_id).await;
        let completion = sub_agent_completion_item(
            "/root/worker",
            &AgentStatus::Completed(Some("done".to_string())),
        )
        .expect("terminal completion");
        let canonical_id = completion.id.clone();
        let mut forged = sub_agent_completion_item(
            "/root/forged",
            &AgentStatus::Completed(Some("untrusted".to_string())),
        )
        .expect("terminal shape");
        forged.sub_agent_completion = None;
        let wait = CollabAgentToolCallItem {
            id: "owned-wait".to_string(),
            tool: CollabAgentTool::Wait,
            status: CollabAgentToolCallStatus::Completed,
            deadline_at_ms: None,
            observe_commentary: None,
            wake_on_completion: None,
            target_messages: None,
            queue_input: None,
            input_batch: None,
            mailbox_input: None,
            sender_thread_id: thread_id,
            receiver_thread_ids: vec![worker_id],
            receiver_agents: Vec::new(),
            prompt: None,
            model: None,
            reasoning_effort: None,
            agents_states: [(worker_id, AgentStatus::Completed(Some("done".to_string())))]
                .into_iter()
                .collect(),
            completion_presentation_agent_ids: Some(vec![worker_id]),
        };
        let unowned_wait = CollabAgentToolCallItem {
            id: "ordinary-wait".to_string(),
            completion_presentation_agent_ids: None,
            ..wait.clone()
        };
        let mut items = vec![
            turn_started("background"),
            completed_item(thread_id, "background", TurnItem::AgentMessage(completion)),
            completed_item(thread_id, "background", TurnItem::AgentMessage(forged)),
            completed_item(thread_id, "background", TurnItem::CollabAgentToolCall(wait)),
            completed_item(
                thread_id,
                "background",
                TurnItem::CollabAgentToolCall(unowned_wait),
            ),
            turn_completed("background"),
        ];
        if exact_rollback {
            items.push(RolloutItem::EventMsg(EventMsg::ThreadRolledBack(
                ThreadRolledBackEvent {
                    num_turns: 1,
                    materialized_turns: None,
                    rollback_start_index: Some(1),
                },
            )));
        }
        store
            .append_completion_items_and_flush(AppendThreadItemsParams { thread_id, items })
            .await
            .expect("canonical batch");
        store.shutdown_thread(thread_id).await.expect("shutdown");
        drop(store);
        let store = projection_store(home.path()).await;
        let page = store
            .list_turns(ListTurnsParams {
                thread_id,
                include_archived: false,
                cursor: None,
                page_size: 20,
                sort_direction: SortDirection::Asc,
                items_view: StoredTurnItemsView::Summary,
            })
            .await
            .expect("cold summary");
        let actual = page
            .turns
            .into_iter()
            .flat_map(|turn| turn.items.into_iter().map(|item| item.item_id))
            .collect::<Vec<_>>();
        let mut expected = vec![canonical_id, "owned-wait".to_string()];
        if !exact_rollback {
            expected.push("ordinary-wait".to_string());
        }
        assert_eq!(actual, expected);
    }
}

#[tokio::test]
async fn fork_summary_does_not_include_completions_after_its_frozen_cutoff() {
    let home = TempDir::new().expect("temp dir");
    let store = projection_store(home.path()).await;
    let root_id = ThreadId::new();
    create_paginated_thread(&store, root_id).await;
    let first = sub_agent_completion_item("/root/first", &AgentStatus::Shutdown).expect("terminal");
    let first_id = first.id.clone();
    store
        .append_completion_items_and_flush(AppendThreadItemsParams {
            thread_id: root_id,
            items: vec![
                turn_started("background"),
                completed_item(root_id, "background", TurnItem::AgentMessage(first)),
                turn_completed("background"),
            ],
        })
        .await
        .expect("first completion");
    let prepared = prepare_paginated_fork(&store, root_id, ForkBoundary::Latest).await;
    let child_id = ThreadId::new();
    create_paginated_subagent_thread(
        &store,
        child_id,
        prepared.history_base,
        /*subagent_history_start_ordinal*/ None,
    )
    .await;
    store
        .persist_thread(child_id, PersistContext::Standard)
        .await
        .expect("persist child");
    let child_completion =
        sub_agent_completion_item("/root/child", &AgentStatus::Shutdown).expect("terminal");
    let child_completion_id = child_completion.id.clone();
    store
        .append_completion_items_and_flush(AppendThreadItemsParams {
            thread_id: child_id,
            items: vec![completed_item(
                child_id,
                "background",
                TurnItem::AgentMessage(child_completion),
            )],
        })
        .await
        .expect("child arrival in inherited turn");
    let later = sub_agent_completion_item("/root/later", &AgentStatus::Shutdown).expect("terminal");
    store
        .append_completion_items_and_flush(AppendThreadItemsParams {
            thread_id: root_id,
            items: vec![completed_item(
                root_id,
                "background",
                TurnItem::AgentMessage(later),
            )],
        })
        .await
        .expect("late completion");
    store.shutdown_thread(child_id).await.expect("close child");
    store.shutdown_thread(root_id).await.expect("close root");
    let page = store
        .list_turns(ListTurnsParams {
            thread_id: child_id,
            include_archived: false,
            cursor: None,
            page_size: 20,
            sort_direction: SortDirection::Asc,
            items_view: StoredTurnItemsView::Summary,
        })
        .await
        .expect("frozen summary");
    assert_eq!(
        page.turns
            .into_iter()
            .flat_map(|turn| turn.items.into_iter().map(|item| item.item_id))
            .collect::<Vec<_>>(),
        vec![first_id, child_completion_id],
    );
}
