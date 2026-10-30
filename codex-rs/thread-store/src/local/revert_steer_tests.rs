//! Mid-turn cuts retain model input and cannot recover an already-excluded suffix.

use super::*;
use codex_history::CompactedItem;
use codex_history::CompactionResumeMetadata;
use codex_history::DynamicToolsSnapshot;
use codex_protocol::dynamic_tools::DynamicToolFunctionSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceSpec;
use codex_protocol::dynamic_tools::DynamicToolNamespaceTool;
use codex_protocol::dynamic_tools::DynamicToolSpec;
use codex_protocol::items::TurnItem;
use codex_protocol::items::UserMessageItem;
use codex_protocol::models::ContentItem;
use codex_protocol::models::ResponseItem;
use codex_protocol::protocol::ItemCompletedEvent;
use codex_protocol::user_input::UserInput;
use pretty_assertions::assert_eq;

#[tokio::test]
async fn steer_revert_preserves_model_prefix_after_reopen_and_enforces_inherited_caps()
-> Result<(), Box<dyn std::error::Error>> {
    for (compressed, presentation_first) in
        [(false, false), (false, true), (true, false), (true, true)]
    {
        for selected_catalog in [None, Some(revert_catalog("replacement")), Some(Vec::new())] {
            let home = TempDir::new()?;
            let config = test_config(home.path());
            let state = codex_state::StateRuntime::init(
                config.sqlite.clone(),
                config.default_model_provider_id.clone(),
            )
            .await?;
            let store = LocalThreadStore::new(config.clone(), Some(state.clone()));
            let thread_id = ThreadId::new();
            create_paginated_thread(&store, thread_id).await;
            let initial_input = response("user", "initial prompt");
            let initial_answer = response("assistant", "retained answer");
            let goal_update: RolloutItem = serde_json::from_value(serde_json::json!({
                "type": "response_item",
                "payload": {
                    "type": "message",
                    "role": "user",
                    "content": [{"type": "input_text", "text": "User set the goal: preserve this instruction"}],
                    "internal_chat_message_metadata_passthrough": {
                        "content_item_kinds": ["user.goal"],
                    },
                },
            }))?;
            let mut items = vec![
                turn_started("turn-1"),
                initial_input.clone(),
                user_item(thread_id, "initial", "initial prompt"),
                initial_answer.clone(),
                response("user", "correction"),
                user_item(thread_id, "steer", "correction"),
                response("assistant", "discarded answer"),
                turn_completed("turn-1"),
                turn_started("turn-2"),
                response("user", "later prompt"),
                turn_completed("turn-2"),
            ];
            if presentation_first {
                items.swap(/*a*/ 1, /*b*/ 2);
                items.swap(/*a*/ 4, /*b*/ 5);
            }
            items.insert(/*index*/ 4, goal_update.clone());
            let retained_catalog = revert_catalog("retained");
            if selected_catalog.is_some() {
                // This retained update would override a replacement header without a suffix carrier.
                items.insert(
                    /*index*/ 0,
                    RolloutItem::DynamicToolsUpdated {
                        thread_id,
                        tools: retained_catalog.clone(),
                    },
                );
            }
            for (owner, tools) in [
                selected_catalog
                    .as_ref()
                    .map(|tools| (thread_id, tools.clone())),
                Some((ThreadId::new(), revert_catalog("foreign"))),
            ]
            .into_iter()
            .flatten()
            {
                // The newest owned catalog is beyond the selected steer. A later foreign
                // catalog must not become this thread's catalog, including in the absent case.
                items.push(if presentation_first {
                    RolloutItem::Compacted(Box::new(CompactedItem {
                        message: "catalog checkpoint in excluded suffix".into(),
                        resume_metadata: Some(CompactionResumeMetadata {
                            dynamic_tools: Some(Box::new(DynamicToolsSnapshot {
                                thread_id: owner,
                                tools,
                            })),
                            multi_agent_version: None,
                            last_started_turn_id: None,
                            turn_attribution: None,
                            previous_turn_settings: None,
                        }),
                        ..Default::default()
                    }))
                } else {
                    RolloutItem::DynamicToolsUpdated {
                        thread_id: owner,
                        tools,
                    }
                });
            }
            store
                .append_items(AppendThreadItemsParams { thread_id, items })
                .await?;
            let original_path = store
                .live_rollout_path(thread_id)
                .await
                .expect("source path");
            store.shutdown_thread(thread_id).await?;
            codex_rollout::state_db::reconcile_rollout(
                Some(state.as_ref()),
                original_path.as_path(),
                "test-provider",
                /*builder*/ None,
                &[],
                /*archived_only*/ Some(false),
                /*new_thread_memory_mode*/ None,
            )
            .await;
            if compressed {
                compress_rollout(original_path.as_path());
            }
            store
                .revert_thread(RevertThreadParams {
                    thread_id,
                    before_turn_id: "turn-1".into(),
                    before_item_id: Some("steer".into()),
                    multi_agent_version: None,
                })
                .await?;
            let selected_path = state
                .get_thread(thread_id)
                .await?
                .expect("thread metadata")
                .rollout_path;
            let reopened = LocalThreadStore::new(config, Some(state.clone()));
            let context = reopened
                .load_latest_model_context(crate::LoadThreadHistoryParams {
                    thread_id,
                    include_archived: false,
                })
                .await?;
            assert_eq!(
                codex_history::latest_dynamic_tools(&context.items),
                selected_catalog.as_deref(),
            );
            if selected_catalog.is_some() {
                assert!(
                    context.items.iter().any(|item| matches!(
                        item,
                        RolloutItem::DynamicToolsUpdated { thread_id: owner, tools }
                            if *owner == thread_id && tools == &retained_catalog
                    )),
                    "the original prefix still carries its older catalog"
                );
            }
            let retained: Vec<_> = context
                .items
                .iter()
                .filter(|item| matches!(item, RolloutItem::ResponseItem(_)))
                .collect();
            assert_eq!(
                serde_json::to_value(retained)?,
                serde_json::to_value([initial_input, initial_answer, goal_update])?,
            );
            let turns = reopened
                .list_turns(ListTurnsParams {
                    thread_id,
                    include_archived: false,
                    cursor: None,
                    page_size: 10,
                    sort_direction: SortDirection::Asc,
                    items_view: StoredTurnItemsView::NotLoaded,
                })
                .await?;
            assert_eq!(
                turns
                    .turns
                    .into_iter()
                    .map(|turn| (turn.turn_id, turn.started_at, turn.status))
                    .collect::<Vec<_>>(),
                vec![(
                    "turn-1".into(),
                    Some(10),
                    crate::StoredTurnStatus::Interrupted
                )],
            );
            assert!(
                reopened
                    .revert_thread(RevertThreadParams {
                        thread_id,
                        before_turn_id: "turn-1".into(),
                        before_item_id: Some("steer".into()),
                        multi_agent_version: None,
                    })
                    .await
                    .is_err()
            );
            assert_eq!(
                state
                    .get_thread(thread_id)
                    .await?
                    .expect("thread metadata")
                    .rollout_path,
                selected_path,
            );
        }
    }
    Ok(())
}

fn user_item(thread_id: ThreadId, item_id: &str, text: &str) -> RolloutItem {
    RolloutItem::EventMsg(EventMsg::ItemCompleted(ItemCompletedEvent {
        thread_id,
        turn_id: "turn-1".into(),
        item: TurnItem::UserMessage(UserMessageItem {
            id: item_id.into(),
            client_id: None,
            content: vec![UserInput::Text {
                text: text.into(),
                text_elements: Vec::new(),
            }],
        }),
        started_at_ms: None,
        completed_at_ms: 100,
    }))
}

fn response(role: &str, text: &str) -> RolloutItem {
    RolloutItem::ResponseItem(
        ResponseItem::Message {
            status: None,
            encrypted_content: None,
            id: None,
            role: role.into(),
            content: vec![if role == "user" {
                ContentItem::InputText { text: text.into() }
            } else {
                ContentItem::OutputText {
                    annotations: None,
                    logprobs: None,
                    text: text.into(),
                }
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        }
        .into(),
    )
}

fn revert_catalog(name: &str) -> Vec<DynamicToolSpec> {
    vec![
        DynamicToolSpec::Namespace(DynamicToolNamespaceSpec {
            name: format!("catalog_{name}"),
            description: format!("Tools for the {name} catalog"),
            tools: vec![DynamicToolNamespaceTool::Function(
                DynamicToolFunctionSpec {
                    name: "lookup".into(),
                    description: "Resolve a resource with nested options".into(),
                    input_schema: serde_json::json!({
                        "type": "object",
                        "properties": {
                            "resource": {"type": "string"},
                            "options": {
                                "type": "object",
                                "properties": {"limit": {"type": "integer", "minimum": 1}},
                                "additionalProperties": false,
                            },
                        },
                        "required": ["resource"],
                        "additionalProperties": false,
                    }),
                    defer_loading: true,
                },
            )],
        }),
        DynamicToolSpec::Function(DynamicToolFunctionSpec {
            name: format!("audit_{name}"),
            description: "Audit the selected resource".into(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {"resource": {"type": "string"}},
                "required": ["resource"],
            }),
            defer_loading: false,
        }),
    ]
}
