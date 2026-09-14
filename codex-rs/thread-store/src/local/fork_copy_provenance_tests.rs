//! A real copied lineage retains original proof, never proof manufactured by flattening.

use super::*;
use codex_history::CodexHarnessMetadata;
use codex_history::ResponseItemEnvelope;
use codex_protocol::ResponseItemId;
use codex_protocol::models::AgentMessageInputContent;
use codex_protocol::protocol::AgentResponseFinalDelivery;
use codex_protocol::protocol::AgentResponseObservation;
use codex_protocol::protocol::AgentResponsePromotedTaskContext;
use codex_protocol::protocol::new_user_agent_task_context_response_item_id;
use pretty_assertions::assert_eq;

#[derive(Clone, Copy, Debug)]
enum ContextKind {
    Observed,
    UserTask,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SourceGap {
    Adjacent,
    SessionMetadata,
    RollbackMarker,
    LineageBoundary,
}

fn context_and_snapshot(kind: ContextKind) -> (ResponseItemEnvelope, RolloutItem) {
    let response = match kind {
        ContextKind::Observed => ResponseItem::AgentMessage {
            id: Some(ResponseItemId::new("amsg")),
            author: "/root/worker".to_string(),
            recipient: "/root".to_string(),
            content: vec![AgentMessageInputContent::InputText {
                text: "recorded result".to_string(),
            }],
            internal_chat_message_metadata_passthrough: None,
        },
        ContextKind::UserTask => ResponseItem::Message {
            id: Some(new_user_agent_task_context_response_item_id()),
            role: "user".to_string(),
            content: vec![ContentItem::InputText {
                text: "<user_agent_task>recorded task</user_agent_task>".to_string(),
            }],
            phase: None,
            internal_chat_message_metadata_passthrough: None,
        },
    };
    let snapshot = RolloutItem::AgentResponseObservation(AgentResponseObservation {
        observer_thread_id: ThreadId::new(),
        target_thread_id: ThreadId::new(),
        target_turn_id: Some("source-turn".to_string()),
        task_preview: None,
        promoted_task_context: AgentResponsePromotedTaskContext::from_response_item(&response),
        pending_commentary: false,
        commentary_after_sequences: Vec::new(),
        commentary_admissions: Vec::new(),
        commentary_delivery: None,
        target_messages: false,
        reply_route_enabled: None,
        reply_route_context_installed: false,
        queue_delivery: false,
        message_wake_turn_id: None,
        baseline_final_delivery: AgentResponseFinalDelivery::None,
        final_delivery: AgentResponseFinalDelivery::None,
        final_delivery_response_item_id: None,
        committed_delivery_response_item_ids: vec![response.id().expect("source identity").clone()],
        mailbox_final_subscription_message_id: None,
        mailbox_final_subscription_suppressed_message_id: None,
    });
    let mut envelope = ResponseItemEnvelope::new(response);
    envelope.metadata = Some(CodexHarnessMetadata {
        user_input_order: Some(7),
        ..Default::default()
    });
    (envelope, snapshot)
}

#[tokio::test]
async fn copying_rejects_new_observation_evidence_across_removed_source_boundaries() {
    for kind in [ContextKind::Observed, ContextKind::UserTask] {
        for gap in [
            SourceGap::Adjacent,
            SourceGap::SessionMetadata,
            SourceGap::RollbackMarker,
            SourceGap::LineageBoundary,
        ] {
            let home = TempDir::new().expect("source home");
            let root = ThreadId::new();
            let child = ThreadId::new();
            let root_path = home.path().join("root.jsonl");
            let child_path = home.path().join("child.jsonl");
            let root_meta = session_meta(root);
            let (context, snapshot) = context_and_snapshot(kind);
            let delivery = RolloutItem::InterAgentCommunicationMetadata {
                trigger_turn: false,
            };
            let mut root_items = vec![
                RolloutItem::SessionMeta(root_meta.clone()),
                delivery.clone(),
                RolloutItem::ResponseItem(context.clone()),
            ];
            match gap {
                SourceGap::Adjacent | SourceGap::LineageBoundary => {}
                SourceGap::SessionMetadata => {
                    root_items.push(RolloutItem::SessionMeta(root_meta.clone()))
                }
                SourceGap::RollbackMarker => root_items.push(RolloutItem::EventMsg(
                    EventMsg::ThreadRolledBack(ThreadRolledBackEvent {
                        num_turns: 0,
                        materialized_turns: None,
                        rollback_start_index: Some(3),
                    }),
                )),
            }
            if gap != SourceGap::LineageBoundary {
                root_items.push(snapshot.clone());
            }
            let root_contents = root_items
                .iter()
                .enumerate()
                .map(|(index, item)| rollout_line(index as u64, item.clone()) + "\n")
                .collect::<String>();
            fs::write(&root_path, &root_contents).expect("source root");
            let mut segments = vec![RolloutLineageSegment {
                rollout_id: root,
                rollout_path: root_path.clone(),
                start_ordinal: 1,
                end: None,
            }];
            let mut leaf_meta = root_meta;
            let mut child_contents = None;
            if gap == SourceGap::LineageBoundary {
                let cutoff = HistoryPosition {
                    thread_id: root,
                    end_ordinal_exclusive: 3,
                    end_byte_offset: root_contents.len() as u64,
                };
                segments[0].end = Some(cutoff);
                leaf_meta = session_meta(child);
                leaf_meta.meta.history_base = Some(cutoff);
                let contents = format!(
                    "{}\n{}\n",
                    rollout_line(
                        /*ordinal*/ 3,
                        RolloutItem::SessionMeta(leaf_meta.clone())
                    ),
                    rollout_line(/*ordinal*/ 4, snapshot.clone())
                );
                fs::write(&child_path, &contents).expect("source child");
                child_contents = Some(contents);
                segments.push(RolloutLineageSegment {
                    rollout_id: child,
                    rollout_path: child_path.clone(),
                    start_ordinal: 4,
                    end: None,
                });
            }
            let result = load_lineage(RolloutLineage { segments }, leaf_meta.clone()).await;
            if gap == SourceGap::Adjacent {
                assert_rollout_items_eq(
                    &result.expect("original proof is copied"),
                    vec![
                        RolloutItem::SessionMeta(leaf_meta),
                        delivery,
                        RolloutItem::ResponseItem(context),
                        snapshot,
                    ],
                );
            } else {
                assert!(
                    matches!(result, Err(ThreadStoreError::InvalidRequest { message })
                    if message.contains("flattening would create response observation evidence")),
                    "copy must not manufacture {kind:?} proof across {gap:?}"
                );
            }
            assert_eq!(
                fs::read_to_string(&root_path).expect("unmodified root"),
                root_contents
            );
            if let Some(contents) = child_contents {
                assert_eq!(
                    fs::read_to_string(&child_path).expect("unmodified child"),
                    contents
                );
            }
        }
    }
}
